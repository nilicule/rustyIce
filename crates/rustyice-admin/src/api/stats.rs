use crate::state::AdminState;
use axum::extract::State;
use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub struct GlobalStats {
    pub uptime_secs: u64,
    pub total_mounts: usize,
    pub total_listeners: usize,
}

pub async fn global_stats(State(state): State<AdminState>) -> Json<GlobalStats> {
    Json(GlobalStats {
        uptime_secs: state.start_time.elapsed().as_secs(),
        total_mounts: state.mounts.list().len(),
        total_listeners: state.listeners.total_count(),
    })
}
