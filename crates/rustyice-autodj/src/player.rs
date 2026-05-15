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

use crate::playlist::{Order, scan, order_tracks};
use rustyice_core::config::AutoDjConfig;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use tokio::task::JoinHandle;

/// One running AutoDJ. Constructed via [`AutoDjPlayer::from_config`], started
/// via [`AutoDjPlayer::spawn`].
pub struct AutoDjPlayer {
    pub(crate) folder: PathBuf,
    pub(crate) order: Order,
    pub(crate) loop_playlist: bool,
    pub(crate) transcode: TranscodeConfig,
    pub(crate) vorbis_comments: Vec<(String, String)>,
    pub(crate) mount: Arc<ActiveMount>,
    pub(crate) shutdown: CancellationToken,
}

impl AutoDjPlayer {
    /// Build a player from the config block and the registered mount.
    #[must_use]
    pub fn from_config(
        cfg: &AutoDjConfig,
        mount: Arc<ActiveMount>,
        shutdown: CancellationToken,
    ) -> Self {
        let vorbis_comments = vorbis_comments_for(cfg);
        Self {
            folder: cfg.folder.clone(),
            order: cfg.order.clone().into(),
            loop_playlist: cfg.loop_playlist,
            transcode: cfg.transcode.clone(),
            vorbis_comments,
            mount,
            shutdown,
        }
    }

    /// Run the player on a fresh tokio task. The returned handle resolves
    /// when the player exits (cleanly or on shutdown).
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    async fn run(self) {
        let Self {
            folder,
            order,
            loop_playlist,
            transcode,
            vorbis_comments,
            mount,
            shutdown,
        } = self;

        loop {
            if shutdown.is_cancelled() {
                return;
            }
            // Wait for the mount to be free of any live source before claiming.
            if mount.source_connected.load(Ordering::Acquire) {
                wait_for_live_source_to_release(&mount, &shutdown).await;
                if shutdown.is_cancelled() {
                    return;
                }
            }

            // Scan + order playlist.
            let tracks = match scan(&folder) {
                Ok(t) if t.is_empty() => {
                    warn!(folder = %folder.display(), "autodj: empty folder, idling 30s");
                    sleep_or_shutdown(Duration::from_secs(30), &shutdown).await;
                    continue;
                }
                Ok(t) => order_tracks(t, order, None),
                Err(e) => {
                    warn!(folder = %folder.display(), "autodj: scan failed: {e}");
                    return;
                }
            };

            if !claim_slot(&mount) {
                // Lost the race to a live source; loop and wait again.
                continue;
            }

            let outcome = play_playlist(
                &tracks,
                &transcode,
                &vorbis_comments,
                &mount,
                &shutdown,
            ).await;

            release_slot(&mount);

            match outcome {
                PassOutcome::Cancelled => {
                    if shutdown.is_cancelled() { return; }
                    // Cancelled by source_cancel — either preempt or kick.
                    if mount.preempt_pending.swap(false, Ordering::AcqRel) {
                        mount.autodj_resume.notified().await;
                    }
                    // either branch: advance and loop
                    continue;
                }
                PassOutcome::Completed => {
                    if loop_playlist {
                        continue;
                    }
                    debug!("autodj playlist exhausted and loop=false, exiting");
                    return;
                }
            }
        }
    }
}

#[derive(Debug)]
enum PassOutcome { Completed, Cancelled }

fn vorbis_comments_for(cfg: &AutoDjConfig) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if let Some(s) = &cfg.name        { v.push(("TITLE".into(), s.clone())); }
    if let Some(s) = &cfg.description { v.push(("DESCRIPTION".into(), s.clone())); }
    if let Some(s) = &cfg.genre       { v.push(("GENRE".into(), s.clone())); }
    if let Some(s) = &cfg.url         { v.push(("LOCATION".into(), s.clone())); }
    v.push(("ENCODER".into(), format!("rustyice/{}", env!("CARGO_PKG_VERSION"))));
    v
}

async fn wait_for_live_source_to_release(
    mount: &Arc<ActiveMount>,
    shutdown: &CancellationToken,
) {
    let notify = mount.autodj_resume.clone();
    while mount.source_connected.load(Ordering::Acquire) {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            () = notify.notified() => {}
        }
    }
}

async fn sleep_or_shutdown(d: Duration, shutdown: &CancellationToken) {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => {}
        () = tokio::time::sleep(d) => {}
    }
}

fn claim_slot(mount: &Arc<ActiveMount>) -> bool {
    if mount.source_connected
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    mount.source_is_autodj.store(true, Ordering::Release);
    *mount.connected_at.lock().unwrap() = Some(Instant::now());
    let token = CancellationToken::new();
    *mount.source_cancel.lock().unwrap() = Some(token);
    true
}

fn release_slot(mount: &Arc<ActiveMount>) {
    *mount.source_cancel.lock().unwrap() = None;
    *mount.connected_at.lock().unwrap() = None;
    mount.source_overlay.store(Arc::new(None));
    mount.header_bytes.store(Arc::new(None));
    mount.source_is_autodj.store(false, Ordering::Release);
    mount.source_connected.store(false, Ordering::Release);
}

async fn play_playlist(
    tracks: &[PathBuf],
    transcode: &TranscodeConfig,
    vorbis_comments: &[(String, String)],
    mount: &Arc<ActiveMount>,
    shutdown: &CancellationToken,
) -> PassOutcome {
    let cancel = match mount.source_cancel.lock().unwrap().clone() {
        Some(t) => t,
        None => return PassOutcome::Cancelled,
    };
    // Use a token combining shutdown + this AutoDJ's source_cancel.
    let cancel_combined = CancellationToken::new();
    let bridge = spawn_cancel_bridge(cancel_combined.clone(), cancel.clone(), shutdown.clone());

    // Pipeline accepts any source codec — push_pcm bypasses the decoder stage.
    let mut pipeline = match TranscodePipeline::new(
        transcode.clone(),
        CodecId::MP3, // placeholder; push_pcm doesn't use the decoder
        vorbis_comments.to_vec(),
    ) {
        Ok(p) => p,
        Err(e) => {
            warn!("autodj: transcode init failed: {e}");
            bridge.abort();
            return PassOutcome::Completed;
        }
    };

    let mut sequence = 0u64;
    let mut output_bytes = 0u64;
    let start = Instant::now();
    let mut header_capture: Option<OggHeaderCapture> =
        if matches!(transcode.format, TranscodeFormat::Vorbis) {
            Some(OggHeaderCapture::new())
        } else {
            None
        };

    for track in tracks {
        if cancel_combined.is_cancelled() {
            bridge.abort();
            return PassOutcome::Cancelled;
        }
        match play_track(
            track,
            &mut pipeline,
            transcode,
            mount,
            &mut sequence,
            &mut output_bytes,
            start,
            &mut header_capture,
            &cancel_combined,
        ).await {
            Ok(()) => {}
            Err(PlayError::Cancelled) => {
                bridge.abort();
                return PassOutcome::Cancelled;
            }
        }
    }
    bridge.abort();
    PassOutcome::Completed
}

fn spawn_cancel_bridge(
    out: CancellationToken,
    a: CancellationToken,
    b: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            () = a.cancelled() => out.cancel(),
            () = b.cancelled() => out.cancel(),
        }
    })
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

    #[tokio::test]
    async fn loop_false_exits_after_playlist_exhausts() {
        let dir = tempfile::tempdir().unwrap();
        let (mp3, _) = crate::test_fixtures::write_test_fixtures(dir.path()).unwrap();
        // Move the mp3 into a new directory so the playlist has a single file
        // we can plausibly play through within the test budget.
        let playlist_dir = tempfile::tempdir().unwrap();
        std::fs::copy(&mp3, playlist_dir.path().join("track.mp3")).unwrap();

        let mount = test_mount();
        let cfg = AutoDjConfig {
            mount: "/auto".to_string(),
            folder: playlist_dir.path().to_path_buf(),
            name: None, description: None, genre: None, url: None,
            enabled: true,
            loop_playlist: false,
            order: rustyice_core::config::Order::Sequential,
            max_listeners: None,
            burst_size: None,
            transcode: TranscodeConfig {
                format: TranscodeFormat::Mp3,
                sample_rate: 44_100,
                bitrate_kbps: 64,
            },
        };
        let shutdown = CancellationToken::new();
        let player = AutoDjPlayer::from_config(&cfg, mount.clone(), shutdown.clone());
        let handle = player.spawn();

        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(result.is_ok(), "player did not exit within 5s when loop=false");
        assert!(!mount.source_connected.load(Ordering::Acquire), "slot must be released");
        assert!(!mount.source_is_autodj.load(Ordering::Acquire));
    }
}
