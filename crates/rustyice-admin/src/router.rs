use crate::api::{actions, mounts, stats};
use crate::metrics::metrics_handler;
use crate::state::AdminState;
use axum::{
    routing::{delete, get},
    Router,
};

pub fn build_admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/api/mounts", get(mounts::list_mounts))
        .route("/api/mounts/{path}/listeners", get(mounts::list_listeners))
        .route("/api/mounts/{path}/source", delete(actions::kick_source))
        .route("/api/listeners/{id}", delete(actions::kick_listener))
        .route("/api/stats", get(stats::global_stats))
        .route("/metrics", get(metrics_handler))
        .with_state(state)
}
