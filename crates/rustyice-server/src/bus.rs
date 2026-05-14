use futures::StreamExt;
use rustyice_core::traits::BroadcastBus;
use rustyice_core::types::StreamPacket;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::{errors::BroadcastStreamRecvError, BroadcastStream}, Stream};
use tracing::warn;

/// Fan-out bus backed by `tokio::sync::broadcast` with a rolling history
/// buffer. New subscribers receive recent packets first so they can fill their
/// playback buffer immediately instead of waiting for live data to trickle in
/// at exactly playback speed.
pub struct TokioBroadcastBus {
    sender: broadcast::Sender<Arc<StreamPacket>>,
    /// Recent packets replayed to every new subscriber. Guarded by the same
    /// lock used when creating a receiver so history and live stream are always
    /// contiguous (no gap, no duplicate).
    history: Mutex<VecDeque<Arc<StreamPacket>>>,
    history_cap: usize,
}

/// Cap on packets delivered as initial history to a new subscriber. Sized to
/// fill a typical HTTP player's pre-play buffer (~1 s at 128 kbps with 4 KB
/// chunks) and no more — delivering the full ring at network speed makes some
/// players (notably VLC) misbehave on the burst-then-live transition.
const HISTORY_SNAPSHOT_PACKETS: usize = 4;

impl TokioBroadcastBus {
    /// `capacity` is both the broadcast ring size and the history ring size.
    /// Maps to `limits.ring_size` in config.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender,
            history: Mutex::new(VecDeque::with_capacity(capacity)),
            history_cap: capacity,
        }
    }
}

impl BroadcastBus for TokioBroadcastBus {
    fn publish(&self, packet: Arc<StreamPacket>) {
        // Hold the history lock while sending so that subscribe() cannot
        // observe a state where the packet is in neither history nor live stream.
        let mut hist = self.history.lock().unwrap();
        let _ = self.sender.send(Arc::clone(&packet));
        if hist.len() >= self.history_cap {
            hist.pop_front();
        }
        hist.push_back(packet);
    }

    fn subscribe(&self) -> Pin<Box<dyn Stream<Item = Arc<StreamPacket>> + Send + 'static>> {
        // Create the receiver under the history lock so the snapshot and the
        // live subscription start at the same logical position: every packet
        // that existed at this instant is in history; every packet published
        // after is in the live stream.
        let hist = self.history.lock().unwrap();
        let receiver = self.sender.subscribe();
        let skip = hist.len().saturating_sub(HISTORY_SNAPSHOT_PACKETS);
        let history_snapshot: Vec<Arc<StreamPacket>> =
            hist.iter().skip(skip).cloned().collect();
        drop(hist);

        Box::pin(futures::stream::iter(history_snapshot).chain(live_stream(receiver)))
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

    fn make_packet(seq: u64) -> Arc<StreamPacket> {
        Arc::new(StreamPacket {
            payload: AudioPayload::Encoded(EncodedPacket {
                codec: CodecId::MP3,
                data: Bytes::from(vec![u8::try_from(seq % 256).unwrap_or(0); 4]),
            }),
            pts: Duration::from_millis(seq * 26),
            sequence: seq,
        })
    }

    #[tokio::test]
    async fn subscriber_receives_published_packet() {
        let bus = TokioBroadcastBus::new(16);
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
        let bus = TokioBroadcastBus::new(16);
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
        let bus = TokioBroadcastBus::new(16);
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
        let bus = TokioBroadcastBus::new(2);
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
        let bus = TokioBroadcastBus::new(16);
        let mut sub = bus.subscribe();
        let original = make_packet(99);
        bus.publish(Arc::clone(&original));
        let received = tokio::time::timeout(Duration::from_millis(100), sub.next())
            .await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&original, &received));
    }

    #[tokio::test]
    async fn late_subscriber_receives_history() {
        let bus = TokioBroadcastBus::new(16);
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
        let bus = TokioBroadcastBus::new(16);
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
    async fn history_caps_at_capacity() {
        let bus = TokioBroadcastBus::new(3);
        for i in 1..=5 {
            bus.publish(make_packet(i));
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
