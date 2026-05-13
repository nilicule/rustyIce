use metrics_exporter_prometheus::PrometheusHandle;
use rustyice_core::mount::MountRegistry;
use rustyice_core::traits::AuthBackend;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// In-memory session store. Tokens expire after `ttl`; on every successful
/// lookup the token is sliding-window refreshed.
pub struct SessionStore {
    sessions: RwLock<HashMap<String, SessionEntry>>,
    ttl: Duration,
}

struct SessionEntry {
    user: String,
    expires_at: Instant,
}

impl SessionStore {
    #[must_use]
    pub fn new(ttl: Duration) -> Arc<Self> {
        Arc::new(Self { sessions: RwLock::new(HashMap::new()), ttl })
    }

    /// # Panics
    /// Panics if the internal `RwLock` is poisoned.
    pub fn create(&self, user: String) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let expires_at = Instant::now() + self.ttl;
        self.sessions
            .write()
            .unwrap()
            .insert(token.clone(), SessionEntry { user, expires_at });
        token
    }

    /// Look up a token. Returns the associated user (and slides the expiry
    /// forward) if valid, `None` if missing or expired.
    ///
    /// # Panics
    /// Panics if the internal `RwLock` is poisoned.
    pub fn touch(&self, token: &str) -> Option<String> {
        let mut sessions = self.sessions.write().unwrap();
        let entry = sessions.get_mut(token)?;
        if entry.expires_at < Instant::now() {
            sessions.remove(token);
            return None;
        }
        entry.expires_at = Instant::now() + self.ttl;
        Some(entry.user.clone())
    }

    /// # Panics
    /// Panics if the internal `RwLock` is poisoned.
    pub fn revoke(&self, token: &str) {
        self.sessions.write().unwrap().remove(token);
    }
}

pub type ListenerId = u64;

pub struct ListenerEntry {
    pub id: ListenerId,
    pub mount_path: String,
    pub peer_addr: Option<SocketAddr>,
    pub connected_at: Instant,
    pub cancel: CancellationToken,
}

pub struct ListenerDetail {
    pub id: ListenerId,
    pub peer_addr: Option<SocketAddr>,
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
    pub fn register(
        &self,
        mount_path: String,
        peer_addr: Option<SocketAddr>,
        cancel: CancellationToken,
    ) -> ListenerId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.entries.write().unwrap().insert(
            id,
            ListenerEntry {
                id,
                mount_path,
                peer_addr,
                connected_at: Instant::now(),
                cancel,
            },
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
    pub fn details_for_mount(&self, mount_path: &str) -> Vec<ListenerDetail> {
        self.entries
            .read()
            .unwrap()
            .values()
            .filter(|e| e.mount_path == mount_path)
            .map(|e| ListenerDetail { id: e.id, peer_addr: e.peer_addr })
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
    pub auth: Arc<dyn AuthBackend + Send + Sync>,
    pub sessions: Arc<SessionStore>,
    pub version: &'static str,
    /// Port the public stream listener is bound to. Used by the landing page
    /// to build clickable links to active mounts.
    pub stream_port: u16,
}
