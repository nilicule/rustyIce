use crate::decode::FileDecoder;
use crate::tags::display_title;
use bytes::Bytes;
use rustyice_codec::OggHeaderCapture;
use rustyice_core::config::{TranscodeConfig, TranscodeFormat};
use rustyice_core::mount::ActiveMount;
use rustyice_core::types::{AudioPayload, CodecId, EncodedPacket, StreamPacket};
use rustyice_transcode::TranscodePipeline;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Play one track end-to-end: decode → push_pcm → publish → pace.
///
/// Returns `Ok(())` when the track finishes naturally or is skipped due to a
/// recoverable error. Returns `Err(PlayError::Cancelled)` if `cancel` fires.
///
/// All mutable state (sequence, output_bytes, header_capture) is passed in by
/// the caller so a multi-track loop can carry it across tracks without
/// restarting the encoder or breaking the published Ogg/MP3 stream.
#[allow(clippy::too_many_arguments)]
pub async fn play_track(
    track: &Path,
    pipeline: &mut TranscodePipeline,
    transcode: &TranscodeConfig,
    mount: &Arc<ActiveMount>,
    sequence: &mut u64,
    output_bytes: &mut u64,
    start: Instant,
    header_capture: &mut Option<OggHeaderCapture>,
    cancel: &CancellationToken,
) -> Result<(), PlayError> {
    // Update per-track ICY title for MP3 listeners.
    let title = display_title(track);
    mount.current_title.store(Arc::new(Some(title.clone())));
    debug!(?track, "autodj now playing: {title}");

    let mut decoder = match FileDecoder::open(track) {
        Ok(d) => d,
        Err(e) => {
            warn!(?track, "autodj: failed to open file, skipping: {e}");
            return Ok(());
        }
    };

    let output_codec = match transcode.format {
        TranscodeFormat::Mp3 => CodecId::MP3,
        TranscodeFormat::Vorbis => CodecId::VORBIS,
    };
    let output_bps: u64 = u64::from(transcode.bitrate_kbps) * 1000 / 8;

    loop {
        if cancel.is_cancelled() {
            return Err(PlayError::Cancelled);
        }
        let chunk = match decoder.next() {
            Ok(Some(c)) => c,
            Ok(None) => return Ok(()),
            Err(e) => {
                warn!(?track, "autodj decode error, skipping rest of track: {e}");
                return Ok(());
            }
        };
        let encoded = match pipeline.push_pcm(chunk.samples, chunk.sample_rate, chunk.channels) {
            Ok(b) => b,
            Err(e) => {
                warn!(?track, "autodj encode error, dropping chunk: {e}");
                continue;
            }
        };
        if encoded.is_empty() {
            continue;
        }
        publish_encoded(
            encoded, output_codec.clone(), mount, sequence, start, header_capture, output_bytes,
        );
        pace(*output_bytes, output_bps, start, cancel).await?;
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_encoded(
    data: Bytes,
    codec: CodecId,
    mount: &Arc<ActiveMount>,
    sequence: &mut u64,
    start: Instant,
    header_capture: &mut Option<OggHeaderCapture>,
    output_bytes: &mut u64,
) {
    *output_bytes += data.len() as u64;
    if let Some(cap) = header_capture.as_mut() {
        cap.push(&data);
        if cap.is_settled() {
            if let Some(headers) = cap.header_bytes() {
                mount.header_bytes.store(Arc::new(Some(headers)));
            }
            *header_capture = None;
        }
    }
    let packet = Arc::new(StreamPacket {
        payload: AudioPayload::Encoded(EncodedPacket { codec, data }),
        pts: start.elapsed(),
        sequence: *sequence,
    });
    mount.bus.publish(packet);
    *sequence += 1;
}

async fn pace(
    output_bytes: u64,
    output_bps: u64,
    start: Instant,
    cancel: &CancellationToken,
) -> Result<(), PlayError> {
    if output_bps == 0 {
        return Ok(());
    }
    #[allow(clippy::cast_precision_loss)]
    let target = Duration::from_secs_f64(output_bytes as f64 / output_bps as f64);
    let elapsed = start.elapsed();
    if target > elapsed {
        let sleep_for = target - elapsed;
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(PlayError::Cancelled),
            () = tokio::time::sleep(sleep_for) => {}
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum PlayError {
    #[error("cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::Stream;
    use rustyice_core::config::TranscodeFormat;
    use rustyice_core::mount::{MountInfo, MountMetadata};
    use rustyice_core::traits::BroadcastBus;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    struct CollectingBus { packets: Mutex<Vec<Arc<StreamPacket>>> }
    impl BroadcastBus for CollectingBus {
        fn publish(&self, p: Arc<StreamPacket>) { self.packets.lock().unwrap().push(p); }
        fn subscribe(&self) -> Pin<Box<dyn Stream<Item = Arc<StreamPacket>> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
        fn subscriber_count(&self) -> usize { 0 }
    }

    fn test_mount() -> Arc<ActiveMount> {
        let info = MountInfo {
            path: "/auto".to_string(),
            codec: CodecId::MP3,
            source_password: String::new(),
            max_listeners: None,
            metadata: MountMetadata::default(),
        };
        let bus = Arc::new(CollectingBus { packets: Mutex::new(vec![]) });
        Arc::new(ActiveMount::new(info, bus))
    }

    #[tokio::test]
    async fn play_track_publishes_mp3_packets_and_sets_title() {
        let dir = tempfile::tempdir().unwrap();
        let (mp3, _) = crate::test_fixtures::write_test_fixtures(dir.path()).unwrap();

        let mount = test_mount();
        let transcode = TranscodeConfig {
            format: TranscodeFormat::Mp3,
            sample_rate: 44_100,
            bitrate_kbps: 64,
        };
        let mut pipeline = TranscodePipeline::new(
            transcode.clone(),
            CodecId::MP3,
            vec![],
        ).unwrap();
        let mut sequence = 0u64;
        let mut output_bytes = 0u64;
        let mut header_capture = None;
        let cancel = CancellationToken::new();
        // The fixture is ~1s — pace targets real-time. Cancel after the file
        // is fully decoded but before pacing has elapsed all 1s of audio.
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel_clone.cancel();
        });

        let _ = play_track(
            &mp3,
            &mut pipeline,
            &transcode,
            &mount,
            &mut sequence,
            &mut output_bytes,
            Instant::now(),
            &mut header_capture,
            &cancel,
        ).await;

        let title = mount.current_title.load_full();
        assert_eq!(title.as_deref(), Some("Test Artist - Test Track"));
        assert!(sequence > 0, "expected at least one packet published");
        // Sanity check that source_connected was NOT touched — this function
        // is called by the orchestrator which handles slot claim/release.
        assert!(!mount.source_connected.load(Ordering::Acquire));
    }
}
