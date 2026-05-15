//! Outbound-pull relay mounts. Each `[[relays]]` entry in the config gets a
//! tokio task that connects to an upstream Icecast-compatible URL via HTTP
//! GET and re-broadcasts the bytes to local listeners through the existing
//! `BroadcastBus` + `IcecastIngest` pipeline.

pub mod backoff;
pub mod client;

pub use backoff::Backoff;

use crate::state::AppState;
use crate::stream_router::mount_transcode;
use rustyice_core::config::RelayConfig;
use rustyice_core::mount::{ActiveMount, MountInfo, MountMetadata, SourceKind};
use rustyice_core::traits::IngestProtocol;
use rustyice_ingest::IcecastIngest;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// One running relay. Build with [`RelayTask::from_config`], start with
/// [`RelayTask::spawn`].
pub struct RelayTask {
    cfg: RelayConfig,
    mount: Arc<ActiveMount>,
    state: AppState,
    shutdown: CancellationToken,
}

impl RelayTask {
    #[must_use]
    pub fn from_config(
        cfg: RelayConfig,
        mount: Arc<ActiveMount>,
        state: AppState,
        shutdown: CancellationToken,
    ) -> Self {
        Self { cfg, mount, state, shutdown }
    }

    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    async fn run(self) {
        let RelayTask { cfg, mount, state, shutdown } = self;
        let mut backoff = Backoff::new();

        loop {
            if shutdown.is_cancelled() {
                return;
            }

            // If a live source has the slot, park until it releases.
            if mount.source_connected.load(Ordering::Acquire)
                && mount.load_source_kind() != SourceKind::Relay
            {
                wait_for_live_source_to_release(&mount, &shutdown).await;
                if shutdown.is_cancelled() {
                    return;
                }
            }

            if !claim_slot(&mount) {
                continue;
            }
            let source_cancel = mount
                .source_cancel
                .lock()
                .unwrap()
                .clone()
                .expect("claim_slot installed a cancel token");

            let outcome = run_once(&cfg, &mount, &state, &source_cancel, &shutdown).await;

            release_slot(&mount);

            match outcome {
                RunOutcome::Cancelled => {
                    if shutdown.is_cancelled() {
                        return;
                    }
                    if mount.preempt_pending.swap(false, Ordering::AcqRel) {
                        let notify = mount.non_live_resume.clone();
                        tokio::select! {
                            biased;
                            () = shutdown.cancelled() => return,
                            () = notify.notified() => {}
                        }
                    }
                    // Either preempt resumed, or admin kicked us — loop and reconnect.
                }
                RunOutcome::Ok | RunOutcome::Failed => {
                    if matches!(outcome, RunOutcome::Ok) {
                        backoff.reset();
                    }
                    let delay = backoff.next_delay();
                    tokio::select! {
                        biased;
                        () = shutdown.cancelled() => return,
                        () = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
enum RunOutcome {
    Ok,        // upstream ended cleanly (EOF)
    Failed,    // connect or stream error
    Cancelled, // source_cancel fired (admin kick or live preempt)
}

fn claim_slot(mount: &Arc<ActiveMount>) -> bool {
    if mount
        .source_connected
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    mount.store_source_kind(SourceKind::Relay);
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
    mount.store_source_kind(SourceKind::None);
    mount.source_connected.store(false, Ordering::Release);
}

async fn wait_for_live_source_to_release(
    mount: &Arc<ActiveMount>,
    shutdown: &CancellationToken,
) {
    let notify = mount.non_live_resume.clone();
    while mount.source_connected.load(Ordering::Acquire) {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            () = notify.notified() => {}
        }
    }
}

/// One connection attempt. Connects, updates the mount codec, runs ingest
/// until the upstream EOFs or cancel fires.
async fn run_once(
    cfg: &RelayConfig,
    mount: &Arc<ActiveMount>,
    state: &AppState,
    source_cancel: &CancellationToken,
    shutdown: &CancellationToken,
) -> RunOutcome {
    let connect = tokio::select! {
        biased;
        () = shutdown.cancelled() => return RunOutcome::Cancelled,
        () = source_cancel.cancelled() => return RunOutcome::Cancelled,
        r = client::open_upstream(cfg) => r,
    };
    let (codec, body) = match connect {
        Ok(t) => t,
        Err(e) => {
            warn!(mount = %cfg.mount, "relay upstream connect failed: {e}");
            return RunOutcome::Failed;
        }
    };

    info!(mount = %cfg.mount, %codec, upstream = %cfg.upstream, "relay connected");

    // Update advertised codec on the mount.
    {
        let current = mount.info.load();
        if current.codec != codec {
            let mut new_info: MountInfo = (**current).clone();
            new_info.codec = codec.clone();
            mount.info.store(Arc::new(new_info));
        }
    }

    let reader = tokio_util::io::StreamReader::new(body);
    let body_reader: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send + Unpin>> =
        Box::pin(reader);

    let ingest = build_ingest_for_relay(state, &cfg.mount, mount);
    match ingest
        .run(body_reader, mount.bus.clone(), codec, source_cancel.clone())
        .await
    {
        Ok(stats) => {
            info!(
                mount = %cfg.mount,
                bytes = stats.bytes_received,
                packets = stats.packets_published,
                "relay upstream ended"
            );
            RunOutcome::Ok
        }
        Err(rustyice_core::error::IngestError::Cancelled) => RunOutcome::Cancelled,
        Err(e) => {
            debug!(mount = %cfg.mount, "relay ingest error: {e}");
            RunOutcome::Failed
        }
    }
}

/// Build an `IcecastIngest` with the effective transcode + header capture
/// settings for the relay's mount. Mirrors the build helper used by the live
/// source path.
fn build_ingest_for_relay(
    state: &AppState,
    mount_path: &str,
    mount: &Arc<ActiveMount>,
) -> IcecastIngest {
    let cfg = state.config.load();
    let transcode = mount_transcode(&cfg, mount_path);
    let metadata = mount.info.load().metadata.clone();

    let mut ingest = IcecastIngest::default();
    if let Some(tc) = transcode {
        ingest = ingest
            .with_transcode(tc)
            .with_transcode_comments(build_vorbis_comments(&metadata));
    }
    ingest.with_header_capture(mount.header_bytes.clone())
}

fn build_vorbis_comments(meta: &MountMetadata) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if let Some(s) = &meta.name {
        v.push(("TITLE".to_string(), s.clone()));
    }
    if let Some(s) = &meta.description {
        v.push(("DESCRIPTION".to_string(), s.clone()));
    }
    if let Some(s) = &meta.genre {
        v.push(("GENRE".to_string(), s.clone()));
    }
    if let Some(s) = &meta.url {
        v.push(("LOCATION".to_string(), s.clone()));
    }
    v.push((
        "ENCODER".to_string(),
        format!("rustyice/{}", env!("CARGO_PKG_VERSION")),
    ));
    v
}
