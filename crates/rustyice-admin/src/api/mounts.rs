use crate::state::AdminState;
use axum::extract::{Path, State};
use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;

#[derive(Serialize)]
pub struct MountStatus {
    pub path: String,
    pub codec: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub genre: Option<String>,
    pub url: Option<String>,
    pub public: Option<bool>,
    pub audio_info: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub title: Option<String>,
    pub source_connected: bool,
    pub listener_count: usize,
    pub source_uptime_secs: Option<u64>,
}

pub async fn list_mounts(State(state): State<AdminState>) -> Json<Vec<MountStatus>> {
    use std::sync::atomic::Ordering;
    let cfg = state.config.load();
    let statuses = state
        .mounts
        .list()
        .into_iter()
        .map(|m| {
            let info = m.info.load();
            let title = m.current_title.load_full().as_ref().clone();
            let transcode = mount_transcode(&cfg, &info.path);
            let identity = m.effective_identity(transcode.as_ref());
            MountStatus {
                path: info.path.clone(),
                codec: info.codec.as_str().to_string(),
                name: identity.name,
                description: identity.description,
                genre: identity.genre,
                url: identity.url,
                public: identity.public,
                audio_info: identity.audio_info,
                bitrate_kbps: identity.bitrate_kbps,
                title,
                source_connected: m.source_connected.load(Ordering::Relaxed),
                listener_count: m.listener_count(),
                source_uptime_secs: m.source_uptime().map(|d| d.as_secs()),
            }
        })
        .collect();
    Json(statuses)
}

fn mount_transcode(
    cfg: &rustyice_core::config::Config,
    mount_path: &str,
) -> Option<rustyice_core::config::TranscodeConfig> {
    if let Some(mc) = cfg.mounts.iter().find(|m| m.path == mount_path) {
        cfg.effective_transcode(mc).cloned()
    } else {
        cfg.transcode.clone()
    }
}

#[derive(Serialize)]
pub struct ListenerInfo {
    pub id: u64,
    pub address: Option<String>,
}

#[derive(Serialize)]
pub struct ListenerList {
    pub mount_path: String,
    pub listeners: Vec<ListenerInfo>,
}

/// # Errors
/// Returns `404 Not Found` if the mount does not exist.
pub async fn list_listeners(
    State(state): State<AdminState>,
    Path(mount_path): Path<String>,
) -> Result<Json<ListenerList>, StatusCode> {
    let path = format!("/{mount_path}");
    if state.mounts.get(&path).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let listeners = state
        .listeners
        .details_for_mount(&path)
        .into_iter()
        .map(|d| ListenerInfo {
            id: d.id,
            address: d.peer_addr.map(|a| a.to_string()),
        })
        .collect();
    Ok(Json(ListenerList { mount_path: path, listeners }))
}
