use crate::traits::BroadcastBus;
use crate::types::CodecId;
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// Hot-reloadable display metadata for a mount.
#[derive(Debug, Clone, Default)]
pub struct MountMetadata {
    pub name: Option<String>,
    pub description: Option<String>,
    pub genre: Option<String>,
    pub url: Option<String>,
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
    pub bytes_received: AtomicU64,
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
}
