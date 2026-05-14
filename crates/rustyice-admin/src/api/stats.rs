use crate::state::AdminState;
use axum::extract::State;
use axum::Json;
use serde::Serialize;
use std::sync::atomic::Ordering;

#[derive(Serialize)]
pub struct GlobalStats {
    pub uptime_secs: u64,
    pub total_mounts: usize,
    pub total_listeners: usize,
    /// Cumulative bytes read from sources across all mounts. The dashboard
    /// computes the live inbound bandwidth rate by sampling this value twice.
    pub total_bytes_in: u64,
    /// Cumulative bytes written to listeners across all mounts. Drives the
    /// live outbound bandwidth gauge.
    pub total_bytes_out: u64,
    pub version: &'static str,
    pub stream_port: u16,
}

pub async fn global_stats(State(state): State<AdminState>) -> Json<GlobalStats> {
    let mut total_bytes_in: u64 = 0;
    let mut total_bytes_out: u64 = 0;
    for m in state.mounts.list() {
        total_bytes_in = total_bytes_in
            .saturating_add(m.stats.bytes_received.load(Ordering::Relaxed));
        total_bytes_out = total_bytes_out
            .saturating_add(m.stats.bytes_sent.load(Ordering::Relaxed));
    }
    Json(GlobalStats {
        uptime_secs: state.start_time.elapsed().as_secs(),
        total_mounts: state.mounts.list().len(),
        total_listeners: state.listeners.total_count(),
        total_bytes_in,
        total_bytes_out,
        version: state.version,
        stream_port: state.stream_port,
    })
}
