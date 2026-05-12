use metrics_exporter_prometheus::PrometheusHandle;
use rustyice_core::mount::MountRegistry;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

pub type ListenerId = u64;

pub struct ListenerEntry {
    pub id: ListenerId,
    pub mount_path: String,
    pub connected_at: Instant,
    pub cancel: CancellationToken,
}

#[derive(Default)]
pub struct ListenerMap {
    entries: RwLock<HashMap<ListenerId, ListenerEntry>>,
    next_id: AtomicU64,
}

impl ListenerMap {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// # Panics
    /// Panics if the internal `RwLock` is poisoned.
    pub fn register(&self, mount_path: String, cancel: CancellationToken) -> ListenerId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.entries.write().unwrap().insert(
            id,
            ListenerEntry { id, mount_path, connected_at: Instant::now(), cancel },
        );
        id
    }

    /// # Panics
    /// Panics if the internal `RwLock` is poisoned.
    pub fn deregister(&self, id: ListenerId) {
        self.entries.write().unwrap().remove(&id);
    }

    /// # Panics
    /// Panics if the internal `RwLock` is poisoned.
    pub fn kick(&self, id: ListenerId) -> bool {
        if let Some(entry) = self.entries.read().unwrap().get(&id) {
            entry.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// # Panics
    /// Panics if the internal `RwLock` is poisoned.
    pub fn ids_for_mount(&self, mount_path: &str) -> Vec<ListenerId> {
        self.entries
            .read()
            .unwrap()
            .values()
            .filter(|e| e.mount_path == mount_path)
            .map(|e| e.id)
            .collect()
    }

    /// # Panics
    /// Panics if the internal `RwLock` is poisoned.
    pub fn total_count(&self) -> usize {
        self.entries.read().unwrap().len()
    }
}

#[derive(Clone)]
pub struct AdminState {
    pub mounts: MountRegistry,
    pub listeners: Arc<ListenerMap>,
    pub prometheus: PrometheusHandle,
    pub start_time: Instant,
}
