use futures::StreamExt;
use rustyice_core::traits::BroadcastBus;
use rustyice_core::types::StreamPacket;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, Stream};

/// Fan-out bus backed by `tokio::sync::broadcast`.
///
/// On `RecvError::Lagged` the subscriber stream terminates so the output
/// protocol can close the connection cleanly. Swap for a custom lock-free
/// ring in v2 by replacing this struct; the `BroadcastBus` trait is unchanged.
pub struct TokioBroadcastBus {
    sender: broadcast::Sender<Arc<StreamPacket>>,
}

impl TokioBroadcastBus {
    /// `capacity` is the number of packets the ring can hold before the
    /// oldest is overwritten. Maps to `limits.ring_size` in config.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }
}

impl BroadcastBus for TokioBroadcastBus {
    fn publish(&self, packet: Arc<StreamPacket>) {
        // Err means no active receivers — that's fine, just drop the packet.
        let _ = self.sender.send(packet);
    }

    fn subscribe(&self) -> Pin<Box<dyn Stream<Item = Arc<StreamPacket>> + Send + 'static>> {
        let receiver = self.sender.subscribe();
        // BroadcastStream yields Result<T, BroadcastStreamRecvError>.
        // take_while(is_ok) terminates the stream on the first Lagged error.
        let stream = BroadcastStream::new(receiver)
            .take_while(|r| futures::future::ready(r.is_ok()))
            .filter_map(|r| futures::future::ready(r.ok()));
        Box::pin(stream)
    }

    fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
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
                data: Bytes::from(vec![seq as u8; 4]),
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
        let _sub1 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);
        let _sub2 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);
        drop(_sub1);
        tokio::task::yield_now().await;
        assert_eq!(bus.subscriber_count(), 1);
    }

    #[tokio::test]
    async fn lagged_subscriber_stream_terminates() {
        let bus = TokioBroadcastBus::new(2);
        let mut sub = bus.subscribe();
        bus.publish(make_packet(1));
        bus.publish(make_packet(2));
        bus.publish(make_packet(3));
        bus.publish(make_packet(4));
        let result = tokio::time::timeout(Duration::from_millis(200), async {
            while sub.next().await.is_some() {}
        })
        .await;
        assert!(result.is_ok(), "stream must terminate on lag, not hang");
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
}
