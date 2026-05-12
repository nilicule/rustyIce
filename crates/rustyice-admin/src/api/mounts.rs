use crate::state::AdminState;
use axum::extract::{Path, State};
use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;

#[derive(Serialize)]
pub struct MountStatus {
    pub path: String,
    pub codec: String,
    pub source_connected: bool,
    pub listener_count: usize,
    pub source_uptime_secs: Option<u64>,
}

pub async fn list_mounts(State(state): State<AdminState>) -> Json<Vec<MountStatus>> {
    let statuses = state
        .mounts
        .list()
        .into_iter()
        .map(|m| {
            use std::sync::atomic::Ordering;
            MountStatus {
                path: m.info.load().path.clone(),
                codec: m.info.load().codec.as_str().to_string(),
                source_connected: m.source_connected.load(Ordering::Relaxed),
                listener_count: m.listener_count(),
                source_uptime_secs: m.source_uptime().map(|d| d.as_secs()),
            }
        })
        .collect();
    Json(statuses)
}

#[derive(Serialize)]
pub struct ListenerList {
    pub mount_path: String,
    pub listener_ids: Vec<u64>,
}

pub async fn list_listeners(
    State(state): State<AdminState>,
    Path(mount_path): Path<String>,
) -> Result<Json<ListenerList>, StatusCode> {
    let path = format!("/{mount_path}");
    if state.mounts.get(&path).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(ListenerList {
        mount_path: path.clone(),
        listener_ids: state.listeners.ids_for_mount(&path),
    }))
}
