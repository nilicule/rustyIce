use futures::StreamExt;
use rustyice_core::traits::BroadcastBus;
use rustyice_core::types::{AudioPayload, StreamPacket};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::{errors::BroadcastStreamRecvError, BroadcastStream}, Stream};
use tracing::warn;

/// Fan-out bus backed by `tokio::sync::broadcast` with a rolling burst-on-connect
/// buffer. New subscribers receive up to `burst_bytes_cap` bytes of recent
/// stream data before transitioning to live so playback starts immediately
/// instead of waiting for live data to trickle in at exactly playback speed.
/// Matches Icecast's `burst-size` semantics.
pub struct TokioBroadcastBus {
    sender: broadcast::Sender<Arc<StreamPacket>>,
    /// Recent packets replayed to every new subscriber, byte-bounded. Guarded
    /// by the same lock used when creating a receiver so history and live
    /// stream are always contiguous (no gap, no duplicate).
    history: Mutex<HistoryRing>,
    burst_bytes_cap: usize,
}

/// History deque plus a running total of encoded bytes currently in the deque.
/// Kept together under one lock so the total never diverges from the contents.
struct HistoryRing {
    packets: VecDeque<Arc<StreamPacket>>,
    bytes: usize,
}

impl HistoryRing {
    fn new() -> Self {
        Self { packets: VecDeque::new(), bytes: 0 }
    }
}

impl TokioBroadcastBus {
    /// `ring_capacity` is the broadcast channel size (lag tolerance for live
    /// subscribers, maps to `limits.ring_size`). `burst_bytes` is the
    /// burst-on-connect cap in bytes (maps to the resolved per-mount
    /// `burst_size`). Setting `burst_bytes = 0` disables burst.
    #[must_use]
    pub fn new(ring_capacity: usize, burst_bytes: usize) -> Self {
        let (sender, _) = broadcast::channel(ring_capacity);
        Self {
            sender,
            history: Mutex::new(HistoryRing::new()),
            burst_bytes_cap: burst_bytes,
        }
    }
}

impl BroadcastBus for TokioBroadcastBus {
    fn publish(&self, packet: Arc<StreamPacket>) {
        // Hold the history lock while sending so that subscribe() cannot
        // observe a state where the packet is in neither history nor live stream.
        let mut hist = self.history.lock().unwrap();
        let _ = self.sender.send(Arc::clone(&packet));
        let added = packet_bytes(&packet);
        hist.packets.push_back(packet);
        hist.bytes += added;
        // Evict oldest while we're over cap. The `len > 1` guard preserves a
        // single oversized packet — Icecast semantics: send what we have.
        while hist.bytes > self.burst_bytes_cap && hist.packets.len() > 1 {
            if let Some(front) = hist.packets.pop_front() {
                hist.bytes = hist.bytes.saturating_sub(packet_bytes(&front));
            }
        }
    }

    fn subscribe(&self) -> Pin<Box<dyn Stream<Item = Arc<StreamPacket>> + Send + 'static>> {
        // Create the receiver under the history lock so the snapshot and the
        // live subscription start at the same logical position: every packet
        // that existed at this instant is in history; every packet published
        // after is in the live stream.
        let hist = self.history.lock().unwrap();
        let receiver = self.sender.subscribe();
        // burst_size = 0 means "no burst"; skip straight to live.
        if self.burst_bytes_cap == 0 {
            drop(hist);
            return Box::pin(live_stream(receiver));
        }
        // Walk from the back accumulating bytes until we hit the cap. The
        // !snapshot.is_empty() guard ensures we always deliver at least one
        // packet even if it exceeds the cap (Icecast behavior — send what we
        // have rather than starve the listener).
        let mut snapshot: Vec<Arc<StreamPacket>> = Vec::new();
        let mut acc: usize = 0;
        for p in hist.packets.iter().rev() {
            let sz = packet_bytes(p);
            if acc + sz > self.burst_bytes_cap && !snapshot.is_empty() {
                break;
            }
            acc += sz;
            snapshot.push(Arc::clone(p));
            if acc >= self.burst_bytes_cap {
                break;
            }
        }
        snapshot.reverse();
        drop(hist);

        Box::pin(futures::stream::iter(snapshot).chain(live_stream(receiver)))
    }

    fn subscribe_live(&self) -> Pin<Box<dyn Stream<Item = Arc<StreamPacket>> + Send + 'static>> {
        // Hold the history lock while subscribing so the receiver's first
        // packet is the next one published — no race where a packet could
        // appear in both the (omitted) history and the live stream.
        let _hist = self.history.lock().unwrap();
        let receiver = self.sender.subscribe();
        Box::pin(live_stream(receiver))
    }

    fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

/// Byte size of a packet for burst accounting. Only encoded payloads contribute
/// — decoded PCM is a hook for the future transcoding pipeline and isn't sent
/// over the wire as-is.
fn packet_bytes(p: &StreamPacket) -> usize {
    match &p.payload {
        AudioPayload::Encoded(e) => e.data.len(),
        AudioPayload::Decoded(_) => 0,
    }
}

fn live_stream(
    receiver: broadcast::Receiver<Arc<StreamPacket>>,
) -> impl Stream<Item = Arc<StreamPacket>> + Send + 'static {
    BroadcastStream::new(receiver).filter_map(|r| {
        futures::future::ready(match r {
            Ok(p) => Some(p),
            Err(BroadcastStreamRecvError::Lagged(n)) => {
                warn!("subscriber lagged: missed {n} packets");
                None
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::StreamExt;
    use rustyice_core::types::{AudioPayload, CodecId, EncodedPacket, StreamPacket};
    use std::sync::Arc;
    use std::time::Duration;

    /// Default-sized packet for tests that don't care about exact byte counts.
    fn make_packet(seq: u64) -> Arc<StreamPacket> {
        make_packet_sized(seq, 4)
    }

    fn make_packet_sized(seq: u64, size: usize) -> Arc<StreamPacket> {
        Arc::new(StreamPacket {
            payload: AudioPayload::Encoded(EncodedPacket {
                codec: CodecId::MP3,
                data: Bytes::from(vec![u8::try_from(seq % 256).unwrap_or(0); size]),
            }),
            pts: Duration::from_millis(seq * 26),
            sequence: seq,
        })
    }

    #[tokio::test]
    async fn subscriber_receives_published_packet() {
        let bus = TokioBroadcastBus::new(16, 65_536);
        let mut sub = bus.subscribe();
        bus.publish(make_packet(1));
        let received = tokio::time::timeout(Duration::from_millis(100), sub.next())
            .await
            .expect("timed out")
            .expect("stream ended");
        assert_eq!(received.sequence, 1);
    }

    #[tokio::test]
    async fn multiple_subscribers_each_receive_packet() {
        let bus = TokioBroadcastBus::new(16, 65_536);
        let mut sub_a = bus.subscribe();
        let mut sub_b = bus.subscribe();
        bus.publish(make_packet(42));
        let a = tokio::time::timeout(Duration::from_millis(100), sub_a.next())
            .await.unwrap().unwrap();
        let b = tokio::time::timeout(Duration::from_millis(100), sub_b.next())
            .await.unwrap().unwrap();
        assert_eq!(a.sequence, 42);
        assert_eq!(b.sequence, 42);
    }

    #[tokio::test]
    async fn subscriber_count_tracks_live_subscriptions() {
        let bus = TokioBroadcastBus::new(16, 65_536);
        assert_eq!(bus.subscriber_count(), 0);
        let sub1 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);
        let _sub2 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);
        drop(sub1);
        tokio::task::yield_now().await;
        assert_eq!(bus.subscriber_count(), 1);
    }

    #[tokio::test]
    async fn lagged_subscriber_skips_missed_packets_and_continues() {
        let bus = TokioBroadcastBus::new(2, 65_536);
        let mut sub = bus.subscribe();
        // Flood the ring (capacity 2) — subscriber will lag.
        bus.publish(make_packet(1));
        bus.publish(make_packet(2));
        bus.publish(make_packet(3));
        bus.publish(make_packet(4));
        // After the lag the subscriber should still receive new packets.
        bus.publish(make_packet(5));
        let received = tokio::time::timeout(Duration::from_millis(200), sub.next())
            .await
            .expect("subscriber should continue after lag, not hang")
            .expect("stream should not have ended");
        // Sequence must be ≥ 3 (packets 1-2 were overwritten before we could read them).
        assert!(received.sequence >= 3, "got seq={}", received.sequence);
    }

    #[tokio::test]
    async fn packet_arc_is_shared_not_copied() {
        let bus = TokioBroadcastBus::new(16, 65_536);
        let mut sub = bus.subscribe();
        let original = make_packet(99);
        bus.publish(Arc::clone(&original));
        let received = tokio::time::timeout(Duration::from_millis(100), sub.next())
            .await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&original, &received));
    }

    #[tokio::test]
    async fn late_subscriber_receives_history() {
        let bus = TokioBroadcastBus::new(16, 65_536);
        bus.publish(make_packet(1));
        bus.publish(make_packet(2));
        bus.publish(make_packet(3));
        // Subscribe after the packets were already published.
        let mut sub = bus.subscribe();
        let p1 = tokio::time::timeout(Duration::from_millis(100), sub.next()).await.unwrap().unwrap();
        let p2 = tokio::time::timeout(Duration::from_millis(100), sub.next()).await.unwrap().unwrap();
        let p3 = tokio::time::timeout(Duration::from_millis(100), sub.next()).await.unwrap().unwrap();
        assert_eq!(p1.sequence, 1);
        assert_eq!(p2.sequence, 2);
        assert_eq!(p3.sequence, 3);
    }

    #[tokio::test]
    async fn history_and_live_are_contiguous_without_duplicates() {
        // Packets published before subscribe go into history; the one published
        // after goes into the live stream. Combined stream must yield all three
        // exactly once in order.
        let bus = TokioBroadcastBus::new(16, 65_536);
        bus.publish(make_packet(1));
        bus.publish(make_packet(2));
        let mut sub = bus.subscribe();
        bus.publish(make_packet(3));
        let seqs: Vec<u64> = vec![
            tokio::time::timeout(Duration::from_millis(100), sub.next()).await.unwrap().unwrap().sequence,
            tokio::time::timeout(Duration::from_millis(100), sub.next()).await.unwrap().unwrap().sequence,
            tokio::time::timeout(Duration::from_millis(100), sub.next()).await.unwrap().unwrap().sequence,
        ];
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn byte_cap_evicts_oldest() {
        // 10-byte packets, 25-byte burst cap — only the suffix that fits should
        // be retained and replayed.
        let bus = TokioBroadcastBus::new(64, 25);
        for i in 1..=5 {
            bus.publish(make_packet_sized(i, 10));
        }
        let mut sub = bus.subscribe();
        let mut got: Vec<u64> = Vec::new();
        while let Ok(Some(p)) = tokio::time::timeout(Duration::from_millis(50), sub.next()).await {
            got.push(p.sequence);
            if got.len() >= 3 { break; }
        }
        // 25 bytes / 10 bytes/packet = 2 full packets. With the back-walk
        // including up to the cap boundary (>= cap stops loop), we keep the
        // last 2 packets.
        assert_eq!(got, vec![4, 5]);
    }

    #[tokio::test]
    async fn burst_zero_returns_live_only() {
        let bus = TokioBroadcastBus::new(16, 0);
        bus.publish(make_packet(1));
        bus.publish(make_packet(2));
        let mut sub = bus.subscribe();
        // No history should be replayed. Publish a live packet and assert
        // we get only that one.
        bus.publish(make_packet(3));
        let p = tokio::time::timeout(Duration::from_millis(100), sub.next()).await.unwrap().unwrap();
        assert_eq!(p.sequence, 3);
        // Nothing else should be queued.
        let nothing = tokio::time::timeout(Duration::from_millis(50), sub.next()).await;
        assert!(nothing.is_err(), "expected no further packets; got {nothing:?}");
    }

    #[tokio::test]
    async fn oversized_single_packet_replayed_whole() {
        // Single packet exceeds the burst cap — Icecast behavior is to send
        // what we have anyway rather than starving the new listener.
        let bus = TokioBroadcastBus::new(16, 4);
        bus.publish(make_packet_sized(1, 100));
        let mut sub = bus.subscribe();
        let p = tokio::time::timeout(Duration::from_millis(100), sub.next()).await.unwrap().unwrap();
        assert_eq!(p.sequence, 1);
        match &p.payload {
            AudioPayload::Encoded(e) => assert_eq!(e.data.len(), 100),
            AudioPayload::Decoded(_) => panic!("expected Encoded"),
        }
    }

    #[tokio::test]
    async fn history_caps_at_byte_capacity() {
        // 4-byte packets, 12-byte burst cap = 3 packets retained.
        let bus = TokioBroadcastBus::new(16, 12);
        for i in 1..=5 {
            bus.publish(make_packet_sized(i, 4));
        }
        let mut sub = bus.subscribe();
        let p1 = tokio::time::timeout(Duration::from_millis(100), sub.next()).await.unwrap().unwrap();
        let p2 = tokio::time::timeout(Duration::from_millis(100), sub.next()).await.unwrap().unwrap();
        let p3 = tokio::time::timeout(Duration::from_millis(100), sub.next()).await.unwrap().unwrap();
        assert_eq!(p1.sequence, 3);
        assert_eq!(p2.sequence, 4);
        assert_eq!(p3.sequence, 5);
    }
}
