use crate::config::TranscodeConfig;
use crate::traits::BroadcastBus;
use crate::types::CodecId;
use arc_swap::ArcSwap;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// Who currently owns a mount's source slot. Stored on `ActiveMount` as
/// an `AtomicU8`; reads/writes go through `load_source_kind`/`store_source_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SourceKind {
    None = 0,
    Live = 1,
    AutoDj = 2,
    Relay = 3,
}

impl SourceKind {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => SourceKind::Live,
            2 => SourceKind::AutoDj,
            3 => SourceKind::Relay,
            _ => SourceKind::None,
        }
    }
}

/// Hot-reloadable display metadata for a mount.
#[derive(Debug, Clone, Default)]
pub struct MountMetadata {
    pub name: Option<String>,
    pub description: Option<String>,
    pub genre: Option<String>,
    pub url: Option<String>,
}

/// Source-supplied identity headers captured on connect.
///
/// Populated from `Ice-*` / `Icy-*` request headers and merged over
/// `MountMetadata` per-field by `ActiveMount::effective_identity`. Cleared
/// on source disconnect.
#[derive(Debug, Clone, Default)]
pub struct SourceOverlay {
    pub name: Option<String>,
    pub description: Option<String>,
    pub genre: Option<String>,
    pub url: Option<String>,
    pub public: Option<bool>,
    pub audio_info: Option<String>,
    pub bitrate_kbps: Option<u32>,
}

/// Effective merged station identity used for outbound listener response
/// headers, the ICY metaint block, and admin API JSON. Built by
/// `ActiveMount::effective_identity` from `info.metadata` + `source_overlay`,
/// with transcode-target precedence for `bitrate_kbps` / `audio_info`.
#[derive(Debug, Clone, Default)]
pub struct EffectiveIdentity {
    pub name: Option<String>,
    pub description: Option<String>,
    pub genre: Option<String>,
    pub url: Option<String>,
    pub public: Option<bool>,
    pub audio_info: Option<String>,
    pub bitrate_kbps: Option<u32>,
}

/// Full configuration for a mount point.
/// Stored behind `ArcSwap` so it can be atomically replaced on SIGHUP
/// without locking any request path.
#[derive(Debug, Clone)]
pub struct MountInfo {
    pub path: String,
    pub codec: CodecId,
    /// Checked on source connect. Hot-reloadable.
    pub source_password: String,
    /// Hard cap on concurrent listeners. Hot-reloadable.
    pub max_listeners: Option<u32>,
    /// Display metadata. Hot-reloadable.
    pub metadata: MountMetadata,
}

/// Cumulative statistics for a mount. All fields use atomics for lock-free
/// reads from the admin API and metrics scraper.
#[derive(Debug, Default)]
pub struct MountStats {
    /// Bytes read from the connected source (raw network bytes — drives the
    /// admin dashboard's live inbound-bandwidth gauge).
    pub bytes_received: AtomicU64,
    /// Bytes written to listeners on this mount (sum across all listeners
    /// past and present — drives the live outbound-bandwidth gauge).
    pub bytes_sent: AtomicU64,
    pub packets_published: AtomicU64,
    pub total_listener_seconds: AtomicU64,
    pub peak_listeners: AtomicU32,
}

/// Live state for an active mount point.
pub struct ActiveMount {
    /// Atomically swappable mount config — hot-reload replaces this in one store.
    pub info: Arc<ArcSwap<MountInfo>>,
    pub bus: Arc<dyn BroadcastBus>,
    pub source_connected: AtomicBool,
    /// Set when the source connects; cleared when it disconnects.
    pub connected_at: Mutex<Option<Instant>>,
    pub stats: Arc<MountStats>,
    /// Cancellation token for the active source task.
    /// Set to `Some` when a source connects; the admin API cancels it to kick the source.
    pub source_cancel: std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>,
    /// Runtime now-playing title set via admin API. Lock-free read on the
    /// listener hot path. `None` = use `info.metadata.name` fallback.
    pub current_title: Arc<ArcSwap<Option<String>>>,
    /// Source-supplied identity headers, populated on connect and cleared on
    /// disconnect. Lock-free read on the listener hot path. `None` = no source
    /// connected, or source supplied nothing.
    pub source_overlay: Arc<ArcSwap<Option<SourceOverlay>>>,
    /// Codec-format prefix bytes that must be sent to every new listener
    /// before joining the live bus stream. For Ogg Vorbis output (passthrough
    /// *or* transcoded) this carries the three Vorbis header pages — a Vorbis
    /// decoder cannot decode audio packets without them, so mid-stream join
    /// would otherwise produce silence/errors. `None` for MP3 output.
    pub header_bytes: Arc<ArcSwap<Option<Bytes>>>,
    /// What currently owns the source slot: None, a live HTTP source, the
    /// in-process AutoDJ, or a Relay. Read on every PUT/SOURCE connect to
    /// decide whether preemption is allowed.
    pub source_kind: AtomicU8,
    /// Set by a live-source handler before it cancels a non-live owner of
    /// the slot, telling the AutoDJ or Relay task to park on
    /// `non_live_resume` instead of advancing.
    pub preempt_pending: AtomicBool,
    /// Notified by the live source's disconnect guard so a parked AutoDJ
    /// or Relay task wakes and re-claims the slot.
    pub non_live_resume: Arc<tokio::sync::Notify>,
}

impl ActiveMount {
    pub fn new(info: MountInfo, bus: Arc<dyn BroadcastBus>) -> Self {
        Self {
            info: Arc::new(ArcSwap::from_pointee(info)),
            bus,
            source_connected: AtomicBool::new(false),
            connected_at: Mutex::new(None),
            stats: Arc::new(MountStats::default()),
            source_cancel: std::sync::Mutex::new(None),
            current_title: Arc::new(ArcSwap::from_pointee(None)),
            source_overlay: Arc::new(ArcSwap::from_pointee(None)),
            header_bytes: Arc::new(ArcSwap::from_pointee(None)),
            source_kind: AtomicU8::new(SourceKind::None as u8),
            preempt_pending: AtomicBool::new(false),
            non_live_resume: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Snapshot the current listener count.
    #[must_use]
    pub fn listener_count(&self) -> usize {
        self.bus.subscriber_count()
    }

    /// Duration the current source has been connected, if any.
    #[must_use]
    pub fn source_uptime(&self) -> Option<std::time::Duration> {
        self.connected_at
            .lock()
            .ok()?
            .as_ref()
            .map(Instant::elapsed)
    }

    /// Snapshot the current SourceKind.
    #[must_use]
    pub fn load_source_kind(&self) -> SourceKind {
        SourceKind::from_u8(self.source_kind.load(Ordering::Acquire))
    }

    /// Set the current SourceKind.
    pub fn store_source_kind(&self, k: SourceKind) {
        self.source_kind.store(k as u8, Ordering::Release);
    }

    /// Merge `info.metadata` (config) and `source_overlay` (live) into the
    /// effective view shown to listeners and operators.
    ///
    /// String fields: overlay wins per-field, config fills the rest.
    /// `public`: overlay only — no config equivalent.
    /// `bitrate_kbps` / `audio_info`: when `transcode` is `Some`, the transcode
    /// target wins (we know what we're actually emitting); otherwise the
    /// overlay value is used; otherwise the field is `None`.
    #[must_use]
    pub fn effective_identity(&self, transcode: Option<&TranscodeConfig>) -> EffectiveIdentity {
        let info = self.info.load();
        let overlay_snap = self.source_overlay.load_full();
        let overlay = overlay_snap.as_ref().as_ref();

        let name = overlay
            .and_then(|o| o.name.clone())
            .or_else(|| info.metadata.name.clone());
        let description = overlay
            .and_then(|o| o.description.clone())
            .or_else(|| info.metadata.description.clone());
        let genre = overlay
            .and_then(|o| o.genre.clone())
            .or_else(|| info.metadata.genre.clone());
        let url = overlay
            .and_then(|o| o.url.clone())
            .or_else(|| info.metadata.url.clone());
        let public = overlay.and_then(|o| o.public);

        let (bitrate_kbps, audio_info) = match transcode {
            Some(tc) => (
                Some(tc.bitrate_kbps),
                Some(format!(
                    "samplerate={};channels=2;bitrate={}",
                    tc.sample_rate, tc.bitrate_kbps
                )),
            ),
            None => (
                overlay.and_then(|o| o.bitrate_kbps),
                overlay.and_then(|o| o.audio_info.clone()),
            ),
        };

        EffectiveIdentity {
            name,
            description,
            genre,
            url,
            public,
            audio_info,
            bitrate_kbps,
        }
    }
}

/// Thread-safe registry of all active mount points.
///
/// Uses `std::sync::RwLock` (not tokio's) because critical sections
/// are brief `HashMap` operations with no await points.
#[derive(Clone, Default)]
pub struct MountRegistry {
    inner: Arc<RwLock<HashMap<String, Arc<ActiveMount>>>>,
}

impl MountRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a mount. Replaces any existing mount at the same path.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    pub fn add(&self, mount: Arc<ActiveMount>) {
        let path = mount.info.load().path.clone();
        self.inner.write().expect("mount registry poisoned").insert(path, mount);
    }

    /// Remove and return the mount at `path`, or `None` if not present.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn remove(&self, path: &str) -> Option<Arc<ActiveMount>> {
        self.inner.write().expect("mount registry poisoned").remove(path)
    }

    /// Look up a mount by path. Returns `None` if the mount doesn't exist.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<Arc<ActiveMount>> {
        self.inner.read().expect("mount registry poisoned").get(path).cloned()
    }

    /// Snapshot of all current mounts.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn list(&self) -> Vec<Arc<ActiveMount>> {
        self.inner.read().expect("mount registry poisoned").values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::BroadcastBus;
    use crate::types::{CodecId, StreamPacket};
    use futures::Stream;
    use std::pin::Pin;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    struct MockBus;
    impl BroadcastBus for MockBus {
        fn publish(&self, _: Arc<StreamPacket>) {}
        fn subscribe(&self) -> Pin<Box<dyn Stream<Item = Arc<StreamPacket>> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
        fn subscriber_count(&self) -> usize { 0 }
    }

    fn make_mount_info(path: &str) -> MountInfo {
        MountInfo {
            path: path.to_string(),
            codec: CodecId::MP3,
            source_password: "secret".to_string(),
            max_listeners: None,
            metadata: MountMetadata::default(),
        }
    }

    #[test]
    fn add_and_get_mount() {
        let registry = MountRegistry::new();
        let mount = Arc::new(ActiveMount::new(
            make_mount_info("/stream"),
            Arc::new(MockBus),
        ));
        registry.add(mount);
        let found = registry.get("/stream").unwrap();
        assert_eq!(found.info.load().path, "/stream");
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let registry = MountRegistry::new();
        assert!(registry.get("/nothere").is_none());
    }

    #[test]
    fn remove_mount() {
        let registry = MountRegistry::new();
        registry.add(Arc::new(ActiveMount::new(
            make_mount_info("/stream"),
            Arc::new(MockBus),
        )));
        assert!(registry.remove("/stream").is_some());
        assert!(registry.get("/stream").is_none());
    }

    #[test]
    fn list_mounts_returns_all() {
        let registry = MountRegistry::new();
        registry.add(Arc::new(ActiveMount::new(make_mount_info("/a"), Arc::new(MockBus))));
        registry.add(Arc::new(ActiveMount::new(make_mount_info("/b"), Arc::new(MockBus))));
        let mut paths: Vec<String> = registry
            .list()
            .into_iter()
            .map(|m| m.info.load().path.clone())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["/a", "/b"]);
    }

    #[test]
    fn source_connected_flag_starts_false() {
        let registry = MountRegistry::new();
        registry.add(Arc::new(ActiveMount::new(
            make_mount_info("/stream"),
            Arc::new(MockBus),
        )));
        let mount = registry.get("/stream").unwrap();
        assert!(!mount.source_connected.load(Ordering::Relaxed));
        mount.source_connected.store(true, Ordering::Relaxed);
        assert!(mount.source_connected.load(Ordering::Relaxed));
    }

    #[test]
    fn mount_info_hot_swap() {
        let registry = MountRegistry::new();
        registry.add(Arc::new(ActiveMount::new(
            make_mount_info("/stream"),
            Arc::new(MockBus),
        )));
        let mount = registry.get("/stream").unwrap();
        let mut new_info = make_mount_info("/stream");
        new_info.source_password = "new_secret".to_string();
        mount.info.store(Arc::new(new_info));
        assert_eq!(mount.info.load().source_password, "new_secret");
    }

    #[test]
    fn current_title_defaults_to_none() {
        let mount = ActiveMount::new(make_mount_info("/stream"), Arc::new(MockBus));
        assert!(mount.current_title.load().is_none());
    }

    #[test]
    fn current_title_can_be_set_and_cleared() {
        let mount = ActiveMount::new(make_mount_info("/stream"), Arc::new(MockBus));
        mount.current_title.store(Arc::new(Some("Artist - Song".to_string())));
        assert_eq!(mount.current_title.load().as_deref(), Some("Artist - Song"));
        mount.current_title.store(Arc::new(None));
        assert!(mount.current_title.load().is_none());
    }

    #[test]
    fn current_title_survives_info_hot_swap() {
        let mount = ActiveMount::new(make_mount_info("/stream"), Arc::new(MockBus));
        mount.current_title.store(Arc::new(Some("persisting".to_string())));
        let mut new_info = make_mount_info("/stream");
        new_info.source_password = "rotated".to_string();
        mount.info.store(Arc::new(new_info));
        assert_eq!(mount.current_title.load().as_deref(), Some("persisting"));
    }

    #[test]
    fn source_overlay_defaults_to_none() {
        let mount = ActiveMount::new(make_mount_info("/stream"), Arc::new(MockBus));
        assert!(mount.source_overlay.load().is_none());
    }

    #[test]
    fn source_overlay_can_be_set_and_cleared() {
        let mount = ActiveMount::new(make_mount_info("/stream"), Arc::new(MockBus));
        let overlay = SourceOverlay {
            name: Some("From Source".to_string()),
            genre: Some("Rock".to_string()),
            public: Some(true),
            bitrate_kbps: Some(192),
            ..Default::default()
        };
        mount.source_overlay.store(Arc::new(Some(overlay)));
        let snap = mount.source_overlay.load_full();
        let stored = snap.as_ref().as_ref().unwrap();
        assert_eq!(stored.name.as_deref(), Some("From Source"));
        assert_eq!(stored.genre.as_deref(), Some("Rock"));
        assert_eq!(stored.public, Some(true));
        assert_eq!(stored.bitrate_kbps, Some(192));

        mount.source_overlay.store(Arc::new(None));
        assert!(mount.source_overlay.load().is_none());
    }

    #[test]
    fn source_overlay_survives_info_hot_swap() {
        let mount = ActiveMount::new(make_mount_info("/stream"), Arc::new(MockBus));
        mount.source_overlay.store(Arc::new(Some(SourceOverlay {
            name: Some("persisting".to_string()),
            ..Default::default()
        })));
        let mut new_info = make_mount_info("/stream");
        new_info.source_password = "rotated".to_string();
        mount.info.store(Arc::new(new_info));
        let snap = mount.source_overlay.load_full();
        assert_eq!(snap.as_ref().as_ref().unwrap().name.as_deref(), Some("persisting"));
    }

    use crate::config::{TranscodeConfig, TranscodeFormat};

    fn mount_with_metadata(meta: MountMetadata) -> ActiveMount {
        let info = MountInfo {
            path: "/stream".to_string(),
            codec: CodecId::MP3,
            source_password: "x".to_string(),
            max_listeners: None,
            metadata: meta,
        };
        ActiveMount::new(info, Arc::new(MockBus))
    }

    #[test]
    fn effective_identity_uses_config_when_overlay_absent() {
        let mount = mount_with_metadata(MountMetadata {
            name: Some("Config Name".to_string()),
            description: Some("Config Desc".to_string()),
            genre: Some("Jazz".to_string()),
            url: Some("https://cfg".to_string()),
        });
        let id = mount.effective_identity(None);
        assert_eq!(id.name.as_deref(), Some("Config Name"));
        assert_eq!(id.description.as_deref(), Some("Config Desc"));
        assert_eq!(id.genre.as_deref(), Some("Jazz"));
        assert_eq!(id.url.as_deref(), Some("https://cfg"));
        assert!(id.public.is_none());
        assert!(id.audio_info.is_none());
        assert!(id.bitrate_kbps.is_none());
    }

    #[test]
    fn effective_identity_overlay_wins_per_field() {
        let mount = mount_with_metadata(MountMetadata {
            name: Some("Config Name".to_string()),
            description: Some("Config Desc".to_string()),
            genre: Some("Jazz".to_string()),
            url: Some("https://cfg".to_string()),
        });
        mount.source_overlay.store(Arc::new(Some(SourceOverlay {
            name: Some("Source Name".to_string()),
            genre: Some("Rock".to_string()),
            public: Some(true),
            bitrate_kbps: Some(128),
            ..Default::default()
        })));
        let id = mount.effective_identity(None);
        // Overlay wins where set:
        assert_eq!(id.name.as_deref(), Some("Source Name"));
        assert_eq!(id.genre.as_deref(), Some("Rock"));
        assert_eq!(id.public, Some(true));
        assert_eq!(id.bitrate_kbps, Some(128));
        // Falls back to config where overlay didn't set:
        assert_eq!(id.description.as_deref(), Some("Config Desc"));
        assert_eq!(id.url.as_deref(), Some("https://cfg"));
    }

    #[test]
    fn effective_identity_transcode_overrides_bitrate_and_audio_info() {
        let mount = mount_with_metadata(MountMetadata::default());
        mount.source_overlay.store(Arc::new(Some(SourceOverlay {
            bitrate_kbps: Some(320),
            audio_info: Some("samplerate=48000;channels=2".to_string()),
            ..Default::default()
        })));
        let tc = TranscodeConfig {
            format: TranscodeFormat::Mp3,
            sample_rate: 44100,
            bitrate_kbps: 128,
        };
        let id = mount.effective_identity(Some(&tc));
        assert_eq!(id.bitrate_kbps, Some(128));
        assert_eq!(
            id.audio_info.as_deref(),
            Some("samplerate=44100;channels=2;bitrate=128"),
        );
    }

    #[test]
    fn effective_identity_passthrough_uses_overlay_bitrate_and_audio_info() {
        let mount = mount_with_metadata(MountMetadata::default());
        mount.source_overlay.store(Arc::new(Some(SourceOverlay {
            bitrate_kbps: Some(192),
            audio_info: Some("samplerate=44100".to_string()),
            ..Default::default()
        })));
        let id = mount.effective_identity(None);
        assert_eq!(id.bitrate_kbps, Some(192));
        assert_eq!(id.audio_info.as_deref(), Some("samplerate=44100"));
    }

    #[test]
    fn effective_identity_passthrough_omits_bitrate_when_source_silent() {
        let mount = mount_with_metadata(MountMetadata::default());
        // No overlay set at all — pure passthrough, no source headers.
        let id = mount.effective_identity(None);
        assert!(id.bitrate_kbps.is_none());
        assert!(id.audio_info.is_none());
    }

    #[test]
    fn active_mount_source_kind_defaults_to_none() {
        let mount = ActiveMount::new(make_mount_info("/a"), Arc::new(MockBus));
        assert_eq!(mount.load_source_kind(), SourceKind::None);
        assert!(!mount.preempt_pending.load(Ordering::Relaxed));
    }

    #[test]
    fn active_mount_source_kind_round_trips() {
        let mount = ActiveMount::new(make_mount_info("/a"), Arc::new(MockBus));
        mount.store_source_kind(SourceKind::Live);
        assert_eq!(mount.load_source_kind(), SourceKind::Live);
        mount.store_source_kind(SourceKind::AutoDj);
        assert_eq!(mount.load_source_kind(), SourceKind::AutoDj);
        mount.store_source_kind(SourceKind::Relay);
        assert_eq!(mount.load_source_kind(), SourceKind::Relay);
        mount.store_source_kind(SourceKind::None);
        assert_eq!(mount.load_source_kind(), SourceKind::None);
    }

    #[tokio::test]
    async fn non_live_resume_notify_wakes_waiter() {
        let mount = Arc::new(ActiveMount::new(make_mount_info("/a"), Arc::new(MockBus)));
        let notify = mount.non_live_resume.clone();
        let waiter = tokio::spawn(async move {
            notify.notified().await;
            true
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        mount.non_live_resume.notify_one();
        let woke = tokio::time::timeout(std::time::Duration::from_millis(200), waiter)
            .await
            .expect("notify did not wake waiter")
            .unwrap();
        assert!(woke);
    }
}
