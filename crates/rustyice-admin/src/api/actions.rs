use crate::state::AdminState;
use axum::extract::{Path, State};
use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;
use std::sync::atomic::Ordering;

#[derive(Serialize)]
pub struct ActionResult {
    pub success: bool,
    pub message: String,
}

/// # Panics
/// Panics if the `source_cancel` mutex is poisoned.
pub async fn kick_source(
    State(state): State<AdminState>,
    Path(mount_path): Path<String>,
) -> (StatusCode, Json<ActionResult>) {
    let path = format!("/{mount_path}");
    let Some(mount) = state.mounts.get(&path) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ActionResult { success: false, message: "mount not found".into() }),
        );
    };
    if !mount.source_connected.load(Ordering::Relaxed) {
        return (
            StatusCode::CONFLICT,
            Json(ActionResult { success: false, message: "no source connected".into() }),
        );
    }
    let guard = mount.source_cancel.lock().unwrap();
    if let Some(token) = guard.as_ref() {
        token.cancel();
        (StatusCode::OK, Json(ActionResult { success: true, message: "source kicked".into() }))
    } else {
        (
            StatusCode::CONFLICT,
            Json(ActionResult { success: false, message: "no source connected".into() }),
        )
    }
}

pub async fn kick_listener(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> (StatusCode, Json<ActionResult>) {
    if state.listeners.kick(id) {
        (StatusCode::OK, Json(ActionResult { success: true, message: "listener kicked".into() }))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(ActionResult { success: false, message: "listener not found".into() }),
        )
    }
}
