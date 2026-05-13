use crate::api::actions::ActionResult;
use crate::state::AdminState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

const MAX_TITLE_LEN: usize = 256;

#[derive(Deserialize)]
pub struct SetTitleRequest {
    pub title: String,
}

/// PUT /api/mounts/{path}/title — set or clear the runtime title.
pub async fn set_title(
    State(state): State<AdminState>,
    Path(mount_path): Path<String>,
    Json(req): Json<SetTitleRequest>,
) -> (StatusCode, Json<ActionResult>) {
    let path = format!("/{mount_path}");
    let Some(mount) = state.mounts.get(&path) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ActionResult { success: false, message: "mount not found".into() }),
        );
    };

    let trimmed = req.title.trim();

    if trimmed.is_empty() {
        mount.current_title.store(Arc::new(None));
        return (
            StatusCode::OK,
            Json(ActionResult { success: true, message: "title cleared".into() }),
        );
    }

    if trimmed.contains('\'') || trimmed.contains('\r') || trimmed.contains('\n') {
        return (
            StatusCode::BAD_REQUEST,
            Json(ActionResult {
                success: false,
                message: "title may not contain ' or newlines".into(),
            }),
        );
    }

    if trimmed.chars().count() > MAX_TITLE_LEN {
        return (
            StatusCode::BAD_REQUEST,
            Json(ActionResult {
                success: false,
                message: format!("title too long (max {MAX_TITLE_LEN} chars)"),
            }),
        );
    }

    mount.current_title.store(Arc::new(Some(trimmed.to_string())));
    (
        StatusCode::OK,
        Json(ActionResult { success: true, message: "title updated".into() }),
    )
}

/// DELETE /api/mounts/{path}/title — clear the runtime title.
pub async fn clear_title(
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
    mount.current_title.store(Arc::new(None));
    (
        StatusCode::OK,
        Json(ActionResult { success: true, message: "title cleared".into() }),
    )
}
