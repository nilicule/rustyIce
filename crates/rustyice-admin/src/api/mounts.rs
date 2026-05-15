use crate::state::AdminState;
use axum::extract::{Path, State};
use axum::Json;
use axum::http::StatusCode;
use rustyice_core::config::TranscodeFormat;
use rustyice_core::types::CodecId;
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
    pub source_kind: &'static str,
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
            let output_codec = resolve_output_codec(&info.codec, transcode.as_ref());
            MountStatus {
                path: info.path.clone(),
                codec: output_codec.as_str().to_string(),
                name: identity.name,
                description: identity.description,
                genre: identity.genre,
                url: identity.url,
                public: identity.public,
                audio_info: identity.audio_info,
                bitrate_kbps: identity.bitrate_kbps,
                title,
                source_connected: m.source_connected.load(Ordering::Relaxed),
                source_kind: source_kind(&m),
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

/// The codec listeners actually receive: the transcode target if one is
/// configured, otherwise the passthrough source codec. Mirrors the resolution
/// in `rustyice_server::stream_router` so admin/UI and listener responses agree.
fn resolve_output_codec(
    source_codec: &CodecId,
    transcode: Option<&rustyice_core::config::TranscodeConfig>,
) -> CodecId {
    match transcode.map(|tc| &tc.format) {
        Some(TranscodeFormat::Mp3) => CodecId::MP3,
        Some(TranscodeFormat::Vorbis) => CodecId::VORBIS,
        None => source_codec.clone(),
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

fn source_kind(m: &rustyice_core::mount::ActiveMount) -> &'static str {
    use std::sync::atomic::Ordering;
    if !m.source_connected.load(Ordering::Acquire) {
        return "none";
    }
    if m.source_is_autodj.load(Ordering::Acquire) {
        "autodj"
    } else {
        "live"
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustyice_core::mount::{ActiveMount, MountInfo, MountMetadata};
    use rustyice_core::traits::BroadcastBus;
    use rustyice_core::types::StreamPacket;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    struct NullBus;
    impl BroadcastBus for NullBus {
        fn publish(&self, _: Arc<StreamPacket>) {}
        fn subscribe(&self) -> Pin<Box<dyn futures::Stream<Item = Arc<StreamPacket>> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
        fn subscriber_count(&self) -> usize { 0 }
    }

    fn info(path: &str) -> MountInfo {
        MountInfo {
            path: path.into(),
            codec: CodecId::MP3,
            source_password: "x".into(),
            max_listeners: None,
            metadata: MountMetadata::default(),
        }
    }

    #[test]
    fn source_kind_reports_none_live_and_autodj() {
        let m_none = ActiveMount::new(info("/n"), Arc::new(NullBus));
        let m_live = ActiveMount::new(info("/l"), Arc::new(NullBus));
        m_live.source_connected.store(true, Ordering::Release);
        let m_auto = ActiveMount::new(info("/a"), Arc::new(NullBus));
        m_auto.source_connected.store(true, Ordering::Release);
        m_auto.source_is_autodj.store(true, Ordering::Release);

        assert_eq!(source_kind(&m_none), "none");
        assert_eq!(source_kind(&m_live), "live");
        assert_eq!(source_kind(&m_auto), "autodj");
    }
}
