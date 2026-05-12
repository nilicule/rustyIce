use async_trait::async_trait;
use bytes::Bytes;
use rustyice_core::error::IngestError;
use rustyice_core::traits::{BroadcastBus, IngestProtocol};
use rustyice_core::types::{AudioPayload, CodecId, EncodedPacket, SourceStats, StreamPacket};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

pub struct IcecastIngest {
    chunk_size: usize,
}

impl IcecastIngest {
    #[must_use]
    pub fn new(chunk_size: usize) -> Self {
        Self { chunk_size }
    }
}

impl Default for IcecastIngest {
    fn default() -> Self {
        Self::new(4096)
    }
}

#[async_trait]
impl IngestProtocol for IcecastIngest {
    fn name(&self) -> &'static str {
        "icecast"
    }

    async fn run(
        &self,
        mut reader: Pin<Box<dyn AsyncRead + Send + Unpin>>,
        bus: Arc<dyn BroadcastBus>,
        codec: CodecId,
        cancellation: CancellationToken,
    ) -> Result<SourceStats, IngestError> {
        let mut stats = SourceStats::default();
        let start = Instant::now();
        let mut buf = vec![0u8; self.chunk_size];

        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    warn!("ingest cancelled for codec={codec}");
                    return Err(IngestError::Cancelled);
                }
                result = reader.read(&mut buf) => {
                    match result {
                        Ok(0) => {
                            debug!("source disconnected after {} packets", stats.packets_published);
                            break;
                        }
                        Ok(n) => {
                            stats.bytes_received += n as u64;
                            let data = Bytes::copy_from_slice(&buf[..n]);
                            let packet = Arc::new(StreamPacket {
                                payload: AudioPayload::Encoded(EncodedPacket {
                                    codec: codec.clone(),
                                    data,
                                }),
                                pts: start.elapsed(),
                                sequence: stats.packets_published,
                            });
                            bus.publish(packet);
                            stats.packets_published += 1;
                        }
                        Err(e) => {
                            warn!("ingest I/O error: {e}");
                            return Err(IngestError::Io(e));
                        }
                    }
                }
            }
        }

        stats.duration = start.elapsed();
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::Stream;
    use rustyice_core::traits::BroadcastBus;
    use rustyice_core::types::{AudioPayload, CodecId, StreamPacket};
    use std::sync::{Arc, Mutex};

    struct CollectingBus {
        packets: Mutex<Vec<Arc<StreamPacket>>>,
    }

    impl CollectingBus {
        fn new() -> Arc<Self> {
            Arc::new(Self { packets: Mutex::new(vec![]) })
        }
        fn collected(&self) -> Vec<Arc<StreamPacket>> {
            self.packets.lock().unwrap().clone()
        }
    }

    impl BroadcastBus for CollectingBus {
        fn publish(&self, p: Arc<StreamPacket>) {
            self.packets.lock().unwrap().push(p);
        }
        fn subscribe(&self) -> Pin<Box<dyn Stream<Item = Arc<StreamPacket>> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
        fn subscriber_count(&self) -> usize {
            0
        }
    }

    #[tokio::test]
    async fn publishes_all_bytes_from_reader() {
        let data: Vec<u8> = (0u8..200).collect();
        let reader: Pin<Box<dyn AsyncRead + Send + Unpin>> =
            Box::pin(std::io::Cursor::new(data.clone()));
        let bus = CollectingBus::new();

        let stats = IcecastIngest::new(64)
            .run(reader, bus.clone(), CodecId::MP3, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(stats.bytes_received, 200);
        let total: usize = bus
            .collected()
            .iter()
            .filter_map(|p| {
                if let AudioPayload::Encoded(enc) = &p.payload {
                    Some(enc.data.len())
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(total, 200);
    }

    #[tokio::test]
    async fn sequence_numbers_are_monotonic() {
        let reader: Pin<Box<dyn AsyncRead + Send + Unpin>> =
            Box::pin(std::io::Cursor::new(vec![0u8; 300]));
        let bus = CollectingBus::new();
        IcecastIngest::new(100)
            .run(reader, bus.clone(), CodecId::MP3, CancellationToken::new())
            .await
            .unwrap();
        for (i, p) in bus.collected().iter().enumerate() {
            assert_eq!(p.sequence, i as u64);
        }
    }

    #[tokio::test]
    async fn cancellation_stops_ingest() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(256);
        let reader: Pin<Box<dyn AsyncRead + Send + Unpin>> = Box::pin(reader);
        let bus = CollectingBus::new();
        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move {
            IcecastIngest::new(64)
                .run(reader, bus, CodecId::MP3, token_clone)
                .await
        });
        writer.write_all(&[0u8; 64]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        token.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_millis(200), handle)
            .await
            .expect("ingest did not stop on cancellation")
            .unwrap();
        assert!(matches!(result, Err(IngestError::Cancelled)));
    }

    #[tokio::test]
    async fn codec_id_is_preserved_in_packets() {
        let reader: Pin<Box<dyn AsyncRead + Send + Unpin>> =
            Box::pin(std::io::Cursor::new(vec![0u8; 64]));
        let bus = CollectingBus::new();
        IcecastIngest::new(64)
            .run(reader, bus.clone(), CodecId::MP3, CancellationToken::new())
            .await
            .unwrap();
        for p in bus.collected() {
            let AudioPayload::Encoded(ref enc) = p.payload else {
                panic!("expected Encoded payload");
            };
            assert_eq!(enc.codec, CodecId::MP3);
        }
    }
}
