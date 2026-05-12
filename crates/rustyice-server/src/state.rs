use arc_swap::ArcSwap;
use rustyice_admin::{AdminState, ListenerMap, SessionStore};
use rustyice_core::{
    config::Config,
    mount::MountRegistry,
    traits::{AuthBackend, IngestProtocol, OutputProtocol},
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct AppState {
    pub mounts: MountRegistry,
    pub auth: Arc<dyn AuthBackend + Send + Sync>,
    pub ingest: Arc<dyn IngestProtocol + Send + Sync>,
    pub output: Arc<dyn OutputProtocol + Send + Sync>,
    pub listeners: Arc<ListenerMap>,
    pub config: Arc<ArcSwap<Config>>,
    pub shutdown: CancellationToken,
}

impl AppState {
    #[must_use]
    pub fn admin_state(
        &self,
        prometheus: metrics_exporter_prometheus::PrometheusHandle,
    ) -> AdminState {
        AdminState {
            mounts: self.mounts.clone(),
            listeners: self.listeners.clone(),
            prometheus,
            start_time: Instant::now(),
            auth: self.auth.clone(),
            sessions: SessionStore::new(Duration::from_secs(24 * 60 * 60)),
            version: env!("CARGO_PKG_VERSION"),
            stream_port: self.config.load().server.stream_bind.port(),
        }
    }
}
