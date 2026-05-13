use async_trait::async_trait;
use bytes::Bytes;
use rustyice_core::config::TranscodeConfig;
use rustyice_core::error::IngestError;
use rustyice_core::traits::{BroadcastBus, IngestProtocol};
use rustyice_core::types::{AudioPayload, CodecId, EncodedPacket, SourceStats, StreamPacket};
use rustyice_transcode::TranscodePipeline;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

pub struct IcecastIngest {
    chunk_size: usize,
    /// If set, reads are paced to this rate. TCP backpressure slows the sender.
    max_rate_bps: Option<u64>,
    transcode: Option<TranscodeConfig>,
}

impl IcecastIngest {
    #[must_use]
    pub fn new(chunk_size: usize) -> Self {
        Self { chunk_size, max_rate_bps: None, transcode: None }
    }

    /// Set a maximum ingestion rate (bytes/sec). Returns `self` for chaining.
    #[must_use]
    pub fn with_max_rate(mut self, bps: u64) -> Self {
        self.max_rate_bps = Some(bps);
        self
    }

    /// Enable transcoding for this ingest. Returns `self` for chaining.
    #[must_use]
    pub fn with_transcode(mut self, config: TranscodeConfig) -> Self {
        self.transcode = Some(config);
        self
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
        reader: Pin<Box<dyn AsyncRead + Send + Unpin>>,
        bus: Arc<dyn BroadcastBus>,
        codec: CodecId,
        cancellation: CancellationToken,
    ) -> Result<SourceStats, IngestError> {
        let mut pipeline: Option<TranscodePipeline> = self.transcode
            .as_ref()
            .map(|cfg| TranscodePipeline::new(cfg.clone()))
            .transpose()
            .map_err(|e| IngestError::TranscodeInit(e.to_string()))?;

        let mut stats = SourceStats::default();
        let start = Instant::now();
        // Rate in bytes/sec: manual override takes priority, otherwise
        // auto-detected from the first valid MP3 frame header.
        let mut rate: Option<u64> = self.max_rate_bps;
        let mut rate_locked = rate.is_some();

        // Reading happens in its own task so the network is drained at full
        // speed independently of the playback-paced publish loop. Otherwise the
        // pacing back-pressures the read and a client disconnect is only
        // detected after the entire kernel/hyper buffer drains at audio rate —
        // tens of seconds for typical TCP receive windows.
        let (data_tx, mut data_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let abort_token = CancellationToken::new();
        let error_slot: Arc<std::sync::Mutex<Option<std::io::Error>>> =
            Arc::new(std::sync::Mutex::new(None));

        let reader_handle = tokio::spawn(drain_reader(
            reader,
            data_tx,
            cancellation.clone(),
            abort_token.clone(),
            error_slot.clone(),
            self.chunk_size,
            codec.clone(),
        ));

        let result = loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    warn!("ingest cancelled for codec={codec}");
                    break Err(IngestError::Cancelled);
                }
                () = abort_token.cancelled() => {
                    let e = error_slot.lock().unwrap().take()
                        .expect("reader cancels abort_token only after setting error_slot");
                    warn!("ingest I/O error: {e}");
                    break Err(IngestError::Io(e));
                }
                chunk = data_rx.recv() => {
                    let Some(data) = chunk else {
                        // source disconnected — flush pipeline tail
                        if let Some(ref mut p) = pipeline {
                            match p.flush() {
                                Ok(tail) if !tail.is_empty() => {
                                    let packet = Arc::new(StreamPacket {
                                        payload: AudioPayload::Encoded(EncodedPacket {
                                            codec: codec.clone(),
                                            data: tail,
                                        }),
                                        pts: start.elapsed(),
                                        sequence: stats.packets_published,
                                    });
                                    bus.publish(packet);
                                    stats.packets_published += 1;
                                }
                                Ok(_) => {}
                                Err(e) => warn!("transcode flush error: {e}"),
                            }
                        }
                        debug!("source disconnected after {} packets", stats.packets_published);
                        break Ok(());
                    };
                    let n = data.len();

                    if !rate_locked && codec == CodecId::MP3 {
                        if let Some(bps) = rustyice_codec::mp3::scan_bitrate_bps(&data) {
                            debug!("MP3 bitrate detected: {}kbps — pacing source", bps * 8 / 1000);
                            rate = Some(bps);
                            rate_locked = true;
                        }
                    }

                    stats.bytes_received += n as u64;

                    let packet_data: Option<Bytes> = if let Some(ref mut p) = pipeline {
                        match p.push(&data) {
                            Ok(b) if b.is_empty() => None,
                            Ok(b) => Some(b),
                            Err(e) => {
                                warn!("transcode error, dropping packet: {e}");
                                None
                            }
                        }
                    } else {
                        Some(Bytes::from(data))
                    };

                    if let Some(data) = packet_data {
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

                    if let Some(bps) = rate {
                        let target = Duration::from_secs_f64(
                            stats.bytes_received as f64 / bps as f64,
                        );
                        let elapsed = start.elapsed();
                        if target > elapsed {
                            tokio::select! {
                                biased;
                                () = cancellation.cancelled() => {
                                    warn!("ingest cancelled during rate-limit sleep");
                                    break Err(IngestError::Cancelled);
                                }
                                () = abort_token.cancelled() => {
                                    let e = error_slot.lock().unwrap().take()
                                        .expect("reader cancels abort_token only after setting error_slot");
                                    warn!("ingest I/O error: {e}");
                                    break Err(IngestError::Io(e));
                                }
                                () = tokio::time::sleep(target - elapsed) => {}
                            }
                        }
                    }
                }
            }
        };

        reader_handle.abort();

        match result {
            Ok(()) => {
                stats.duration = start.elapsed();
                Ok(stats)
            }
            Err(e) => Err(e),
        }
    }
}

/// Reads from the source as fast as the network allows and forwards each chunk
/// to the publisher loop. On I/O error, parks the error in `error_slot` and
/// cancels `abort_token` so the publisher exits immediately instead of waiting
/// for the queued chunks to drain at the paced rate.
///
/// For MP3, strips any leading ID3v2 tag and Xing/Info/VBRI metadata frame.
/// Without this, embedded cover art makes the bitrate detector lock onto a
/// false sync word inside the JPEG and pace the stream at the wrong rate, and
/// the metadata frame makes live-streaming clients switch to file-mode buffering
/// and underrun.
async fn drain_reader(
    mut reader: Pin<Box<dyn AsyncRead + Send + Unpin>>,
    data_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    cancellation: CancellationToken,
    abort_token: CancellationToken,
    error_slot: Arc<std::sync::Mutex<Option<std::io::Error>>>,
    chunk_size: usize,
    codec: CodecId,
) {
    let mut buf = vec![0u8; chunk_size];
    if codec == CodecId::MP3 {
        strip_mp3_prefix(
            &mut reader,
            &data_tx,
            &mut buf,
            &cancellation,
            &abort_token,
            &error_slot,
        )
        .await;
        if cancellation.is_cancelled() || abort_token.is_cancelled() || data_tx.is_closed() {
            return;
        }
    }
    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return,
            result = reader.read(&mut buf) => {
                match result {
                    Ok(0) => return,
                    Ok(n) => {
                        if data_tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        *error_slot.lock().unwrap() = Some(e);
                        abort_token.cancel();
                        return;
                    }
                }
            }
        }
    }
}

enum ReadOutcome {
    Bytes(usize),
    Eof,
    Stop,
}

/// Reads enough of the MP3 stream to identify and skip any ID3v2 tag and
/// Xing/Info/VBRI metadata frame at the start, then forwards the first chunk
/// of real audio data. On return, subsequent reads from `reader` are pure
/// audio data, ready for pass-through.
async fn strip_mp3_prefix(
    reader: &mut Pin<Box<dyn AsyncRead + Send + Unpin>>,
    data_tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    buf: &mut [u8],
    cancellation: &CancellationToken,
    abort_token: &CancellationToken,
    error_slot: &Arc<std::sync::Mutex<Option<std::io::Error>>>,
) {
    // Phase 1: collect 10 bytes so we can identify an ID3v2 header.
    let mut head = Vec::with_capacity(16);
    while head.len() < 10 {
        match read_one(reader, buf, cancellation, abort_token, error_slot).await {
            ReadOutcome::Bytes(n) => head.extend_from_slice(&buf[..n]),
            ReadOutcome::Eof => {
                flush_remaining(head, data_tx);
                return;
            }
            ReadOutcome::Stop => return,
        }
    }

    // Phase 2: if an ID3v2 tag is present, discard the rest of it.
    let id3_total = parse_id3v2_total(&head);
    let mut pending = if id3_total == 0 {
        head
    } else if head.len() >= id3_total {
        debug!("stripped {id3_total}-byte ID3v2 tag from MP3 stream start");
        head.split_off(id3_total)
    } else {
        let mut to_skip = id3_total - head.len();
        let mut leftover = Vec::new();
        while to_skip > 0 {
            match read_one(reader, buf, cancellation, abort_token, error_slot).await {
                ReadOutcome::Bytes(n) if n <= to_skip => to_skip -= n,
                ReadOutcome::Bytes(n) => {
                    leftover.extend_from_slice(&buf[to_skip..n]);
                    break;
                }
                ReadOutcome::Eof | ReadOutcome::Stop => return,
            }
        }
        debug!("stripped {id3_total}-byte ID3v2 tag from MP3 stream start");
        leftover
    };

    // Phase 3: accumulate ~2 KB of post-ID3 audio bytes so we can detect a
    // Xing/Info/VBRI frame.
    while pending.len() < 2048 {
        match read_one(reader, buf, cancellation, abort_token, error_slot).await {
            ReadOutcome::Bytes(n) => pending.extend_from_slice(&buf[..n]),
            ReadOutcome::Eof => {
                strip_xing_in_place(&mut pending);
                flush_remaining(pending, data_tx);
                return;
            }
            ReadOutcome::Stop => return,
        }
    }

    // Phase 4: strip the metadata frame if present, forward the rest.
    strip_xing_in_place(&mut pending);
    if !pending.is_empty() {
        let _ = data_tx.send(pending);
    }
}

fn strip_xing_in_place(pending: &mut Vec<u8>) {
    if let Some(end) = rustyice_codec::mp3::detect_xing_frame_end(pending) {
        debug!("stripped {end}-byte Xing/Info/VBRI frame after ID3");
        pending.drain(..end);
    }
}

fn flush_remaining(data: Vec<u8>, data_tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
    if !data.is_empty() {
        let _ = data_tx.send(data);
    }
}

/// One cancellable read into `buf`. Errors are parked in `error_slot` and the
/// abort token is tripped so the publisher exits promptly.
async fn read_one(
    reader: &mut Pin<Box<dyn AsyncRead + Send + Unpin>>,
    buf: &mut [u8],
    cancellation: &CancellationToken,
    abort_token: &CancellationToken,
    error_slot: &Arc<std::sync::Mutex<Option<std::io::Error>>>,
) -> ReadOutcome {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => ReadOutcome::Stop,
        result = reader.read(buf) => match result {
            Ok(0) => ReadOutcome::Eof,
            Ok(n) => ReadOutcome::Bytes(n),
            Err(e) => {
                *error_slot.lock().unwrap() = Some(e);
                abort_token.cancel();
                ReadOutcome::Stop
            }
        }
    }
}

/// Returns the total size (header + body) of an ID3v2 tag at the start of
/// `data`, or 0 if no tag is present. `data` must be at least 10 bytes.
fn parse_id3v2_total(data: &[u8]) -> usize {
    if data.len() < 10 || &data[..3] != b"ID3" {
        return 0;
    }
    // Syncsafe: each of the 4 size bytes uses only its low 7 bits.
    let size = ((data[6] as usize & 0x7F) << 21)
        | ((data[7] as usize & 0x7F) << 14)
        | ((data[8] as usize & 0x7F) << 7)
        | (data[9] as usize & 0x7F);
    10 + size
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

    /// Simulates curl being cancelled mid-stream: the source delivers a burst of
    /// data, then errors. With paced reads, the body buffer between us and the
    /// client would take far longer than the test budget to drain — disconnect
    /// detection must NOT be gated on drain time.
    struct BurstThenError {
        remaining: Vec<u8>,
    }

    impl tokio::io::AsyncRead for BurstThenError {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.remaining.is_empty() {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "simulated client disconnect",
                )));
            }
            let n = self.remaining.len().min(buf.remaining());
            let tail = self.remaining.split_off(n);
            buf.put_slice(&self.remaining);
            self.remaining = tail;
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn disconnect_is_reported_without_waiting_for_paced_drain() {
        // 20KB buffered at 1KB/sec pacing would take ~20s to drain. The error
        // must surface promptly regardless — well under the timeout.
        let reader: Pin<Box<dyn AsyncRead + Send + Unpin>> =
            Box::pin(BurstThenError { remaining: vec![0u8; 20_000] });
        let bus = CollectingBus::new();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            IcecastIngest::new(4096)
                .with_max_rate(1000)
                .run(reader, bus, CodecId::MP3, CancellationToken::new()),
        )
        .await
        .expect("disconnect detection should not block on paced drain");

        assert!(matches!(result, Err(IngestError::Io(_))));
    }

    #[tokio::test]
    async fn strips_id3v2_tag_before_publishing() {
        // ID3v2.3 header: "ID3" + version(2 bytes) + flags(1) + size(4 syncsafe).
        // Size = 100 bytes of body → total ID3 = 110 bytes.
        let mut data = vec![b'I', b'D', b'3', 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 100];
        // ID3 body: 100 bytes of arbitrary content that happens to include a
        // false MP3 sync word (the bug we're guarding against).
        let mut body = vec![0xAAu8; 100];
        body[40] = 0xFF;
        body[41] = 0xFB;
        body[42] = 0x80; // bitrate index 1000 (112 kbps) — the false detection
        body[43] = 0x04;
        data.extend_from_slice(&body);
        // Then a real audio frame: MPEG-1 Layer-3, 128 kbps, 44.1 kHz, stereo.
        data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x04]);
        // Pad past the inspection threshold so the buffer-fill loop completes.
        data.resize(110 + 4096, 0u8);

        let reader: Pin<Box<dyn AsyncRead + Send + Unpin>> =
            Box::pin(std::io::Cursor::new(data));
        let bus = CollectingBus::new();
        let stats = IcecastIngest::new(64)
            .run(reader, bus.clone(), CodecId::MP3, CancellationToken::new())
            .await
            .unwrap();

        // The ID3 tag (110 bytes) must not appear in any published packet.
        let published: Vec<u8> = bus
            .collected()
            .iter()
            .filter_map(|p| match &p.payload {
                AudioPayload::Encoded(enc) => Some(enc.data.to_vec()),
                _ => None,
            })
            .flatten()
            .collect();

        assert!(
            !published.starts_with(b"ID3"),
            "ID3 tag was not stripped before publishing"
        );
        // Real audio frame must be the first thing on the bus.
        let sync = published
            .windows(4)
            .position(|w| w[0] == 0xFF && (w[1] & 0xE0) == 0xE0);
        assert_eq!(sync, Some(0), "first published byte should be an MP3 sync word");
        assert_eq!(published[2], 0x90, "should be 128 kbps frame (not the false 112 kbps in cover art)");
        assert_eq!(stats.bytes_received as usize, published.len());
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
