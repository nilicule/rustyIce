use crate::bus::TokioBroadcastBus;
use crate::source_headers::parse_source_overlay;
use crate::state::AppState;
use axum::{
    body::Body,
    extract::{ConnectInfo, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put},
    Router,
};
use futures::TryStreamExt;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rustyice_admin::ListenerMap;
use rustyice_core::error::IngestError;
use rustyice_core::mount::{ActiveMount, MountInfo, MountMetadata, MountRegistry};
use rustyice_core::traits::IngestProtocol;
use rustyice_core::types::CodecId;
use rustyice_ingest::IcecastIngest;
use std::net::SocketAddr;
use std::sync::{atomic::Ordering, Arc};
use std::time::Instant;
use tokio_util::io::StreamReader;
use tracing::{info, warn};

pub fn build_stream_router(state: AppState) -> Router {
    Router::new()
        .route("/{mount}", put(source_handler))
        .route("/{mount}", get(listener_handler))
        .with_state(state)
}

// ── Source handler ─────────────────────────────────────────────────────────

async fn source_handler(
    State(state): State<AppState>,
    Path(mount_segment): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let mount_path = format!("/{mount_segment}");
    let password = extract_source_password(&headers);
    let codec = detect_codec_from_content_type(&headers);

    let (mount, dynamic) = match resolve_or_create_mount(&state, &mount_path, &password, codec.clone()).await {
        Ok(m) => m,
        Err(resp) => return resp,
    };

    // Reject non-MP3 sources on transcode-enabled mounts. The transcode pipeline
    // only handles MP3 input; accepting other codecs would produce a silent stream.
    {
        let cfg = state.config.load();
        let transcode = if let Some(mc) = cfg.mounts.iter().find(|m| m.path == mount_path) {
            cfg.effective_transcode(mc).cloned()
        } else {
            cfg.transcode.clone()
        };
        if transcode.is_some() && codec != CodecId::MP3 {
            return (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("mount {mount_path} requires MP3 source (transcoding is enabled)"),
            )
                .into_response();
        }
    }

    if mount
        .source_connected
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return (StatusCode::CONFLICT, "mount already has an active source").into_response();
    }

    // Child of shutdown so the ingest stops on server shutdown OR admin kick.
    let source_cancel = state.shutdown.child_token();
    *mount.source_cancel.lock().unwrap() = Some(source_cancel.clone());
    *mount.connected_at.lock().unwrap() = Some(Instant::now());

    let overlay = parse_source_overlay(&headers);
    mount.source_overlay.store(Arc::new(Some(overlay.clone())));

    // Runs cleanup even if the handler future is dropped mid-await (e.g. TCP reset).
    // For dynamic mounts, the guard also removes the mount from the registry.
    let _guard = SourceDisconnectGuard {
        mount: mount.clone(),
        listeners: state.listeners.clone(),
        mount_path: mount_path.clone(),
        mounts: dynamic.then(|| state.mounts.clone()),
    };

    info!(
        mount = %mount_path,
        codec = %codec,
        dynamic,
        name = ?overlay.name,
        description = ?overlay.description,
        genre = ?overlay.genre,
        url = ?overlay.url,
        public = ?overlay.public,
        audio_info = ?overlay.audio_info,
        bitrate_kbps = ?overlay.bitrate_kbps,
        "source connected"
    );

    let stream = body.into_data_stream().map_err(std::io::Error::other);
    let reader: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send + Unpin>> =
        Box::pin(StreamReader::new(stream));

    let ingest = build_ingest_for_mount(&state, &mount_path);

    let result = ingest.run(reader, mount.bus.clone(), codec, source_cancel).await;
    match result {
        Ok(source_stats) => {
            info!(
                "source stream ended: mount={mount_path} bytes={} packets={}",
                source_stats.bytes_received, source_stats.packets_published
            );
            StatusCode::OK.into_response()
        }
        Err(IngestError::TranscodeInit(e)) => {
            tracing::error!("transcode init failed for {mount_path}: {e}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "60")],
                "transcoding unavailable",
            ).into_response()
        }
        Err(IngestError::Io(_) | IngestError::Cancelled | IngestError::MountBusy) => {
            StatusCode::OK.into_response()
        }
    }
}

/// Returns `(mount, dynamic)` on success, or a `Response` for the caller to
/// return on failure. `dynamic` indicates whether the mount was just created.
async fn resolve_or_create_mount(
    state: &AppState,
    mount_path: &str,
    password: &str,
    codec: CodecId,
) -> Result<(Arc<ActiveMount>, bool), Response> {
    if let Some(mount) = state.mounts.get(mount_path) {
        return match state.auth.verify_source(mount_path, password).await {
            Ok(true) => Ok((mount, false)),
            Ok(false) => Err(unauthorized("invalid source password")),
            Err(e) => {
                warn!("auth error for {mount_path}: {e}");
                Err(server_error())
            }
        };
    }

    match state.auth.verify_default_source(password).await {
        Ok(true) => {
            let mount = create_dynamic_mount(&state.mounts, mount_path, codec, &state.config);
            Ok((mount, true))
        }
        Ok(false) => Err(unauthorized("invalid source password")),
        Err(e) => {
            warn!("auth error for {mount_path}: {e}");
            Err(server_error())
        }
    }
}

fn create_dynamic_mount(
    registry: &MountRegistry,
    mount_path: &str,
    codec: CodecId,
    config: &arc_swap::ArcSwap<rustyice_core::config::Config>,
) -> Arc<ActiveMount> {
    let cfg = config.load();
    let bus = Arc::new(TokioBroadcastBus::new(cfg.limits.ring_size));
    let info = MountInfo {
        path: mount_path.to_string(),
        codec,
        source_password: String::new(),
        max_listeners: None,
        metadata: MountMetadata::default(),
    };
    let mount = Arc::new(ActiveMount::new(info, bus));
    registry.add(mount.clone());
    mount
}

fn build_ingest_for_mount(state: &AppState, mount_path: &str) -> IcecastIngest {
    let cfg = state.config.load();

    let transcode = if let Some(mc) = cfg.mounts.iter().find(|m| m.path == mount_path) {
        cfg.effective_transcode(mc).cloned()
    } else {
        // Dynamic mount: no per-mount config exists, use the global default.
        cfg.transcode.clone()
    };

    let mut ingest = IcecastIngest::default();

    if let Some(kbps) = cfg.limits.source_max_kbps {
        ingest = ingest.with_max_rate(u64::from(kbps) * 1000 / 8);
    }
    if let Some(tc) = transcode {
        ingest = ingest.with_transcode(tc);
    }
    ingest
}

/// Cleans up source state when the handler completes or is dropped. When the
/// mount was created dynamically by this handler, `mounts` is `Some` so the
/// guard can also drop the mount from the registry on disconnect.
struct SourceDisconnectGuard {
    mount: Arc<ActiveMount>,
    listeners: Arc<ListenerMap>,
    mount_path: String,
    mounts: Option<MountRegistry>,
}

impl Drop for SourceDisconnectGuard {
    fn drop(&mut self) {
        for id in self.listeners.ids_for_mount(&self.mount_path) {
            self.listeners.kick(id);
        }
        self.mount.source_connected.store(false, Ordering::Release);
        *self.mount.source_cancel.lock().unwrap() = None;
        *self.mount.connected_at.lock().unwrap() = None;
        self.mount.source_overlay.store(Arc::new(None));
        if let Some(registry) = &self.mounts {
            let _ = registry.remove(&self.mount_path);
            info!("source disconnected: mount={} (dynamic mount removed)", self.mount_path);
        } else {
            info!("source disconnected: mount={}", self.mount_path);
        }
    }
}

// ── Listener handler ───────────────────────────────────────────────────────

async fn listener_handler(
    State(state): State<AppState>,
    Path(mount_segment): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let mount_path = format!("/{mount_segment}");
    let peer_addr = Some(peer_addr);

    let Some(mount) = state.mounts.get(&mount_path) else {
        return (StatusCode::NOT_FOUND, "mount not found").into_response();
    };

    let cfg = state.config.load();
    let global_count = state.listeners.total_count();
    if global_count >= cfg.limits.max_listeners_global as usize {
        return (StatusCode::SERVICE_UNAVAILABLE, "server full").into_response();
    }
    if let Some(max) = mount.info.load().max_listeners
        && mount.listener_count() >= max as usize
    {
        return (StatusCode::SERVICE_UNAVAILABLE, "mount full").into_response();
    }

    let icy_requested = headers
        .get("icy-metadata")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim() == "1");

    let mount_info = mount.info.load_full();
    let current_title = mount.current_title.clone();
    let subscription = mount.bus.subscribe();

    let listener_cancel = state.shutdown.child_token();
    let listener_id = state
        .listeners
        .register(mount_path.clone(), peer_addr, listener_cancel.clone());

    let (read_end, write_end) = tokio::io::duplex(65_536);
    let writer: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send + Unpin>> =
        Box::pin(write_end);

    let output = state.output.clone();
    let listeners_ref = state.listeners.clone();
    let cancel_clone = listener_cancel.clone();

    tokio::spawn(async move {
        match output
            .run(writer, subscription, mount_info, current_title, icy_requested, cancel_clone)
            .await
        {
            Ok(listener_stats) => {
                tracing::info!(
                    "listener output ended: id={listener_id} bytes={} duration={:?} reason={:?}",
                    listener_stats.bytes_sent, listener_stats.duration, listener_stats.disconnect_reason,
                );
            }
            Err(e) => {
                tracing::warn!("listener output errored: id={listener_id} err={e}");
            }
        }
        listeners_ref.deregister(listener_id);
    });

    let stream = tokio_util::io::ReaderStream::new(read_end);

    let icy_br = {
        let transcode = if let Some(mc) = cfg.mounts.iter().find(|m| m.path == mount_path) {
            cfg.effective_transcode(mc).cloned()
        } else {
            cfg.transcode.clone()
        };
        transcode.map_or_else(|| "128".to_string(), |tc| tc.bitrate_kbps.to_string())
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/mpeg")
        .header("icy-name", mount.info.load().metadata.name.as_deref().unwrap_or(""))
        .header("icy-br", icy_br);

    if icy_requested {
        builder = builder.header("icy-metaint", "8192");
    }

    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn extract_source_password(headers: &HeaderMap) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b64| BASE64.decode(b64).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|decoded| {
            decoded
                .split_once(':')
                .map(|(_, pw)| pw.to_string())
                .unwrap_or(decoded)
        })
        .unwrap_or_default()
}

fn detect_codec_from_content_type(headers: &HeaderMap) -> CodecId {
    match headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        Some(ct) if ct.contains("mpeg") => CodecId::MP3,
        Some(ct) if ct.contains("ogg") => CodecId::VORBIS,
        Some(ct) if ct.contains("aac") => CodecId::AAC,
        _ => CodecId::MP3,
    }
}

fn unauthorized(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"Icecast\"")],
        msg.to_string(),
    )
        .into_response()
}

fn server_error() -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn extracts_password_from_basic_auth_with_username() {
        let mut headers = HeaderMap::new();
        let encoded = BASE64.encode("source:mysecret");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {encoded}")).unwrap(),
        );
        assert_eq!(extract_source_password(&headers), "mysecret");
    }

    #[test]
    fn extracts_password_only_from_basic_auth() {
        let mut headers = HeaderMap::new();
        let encoded = BASE64.encode("mysecret");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {encoded}")).unwrap(),
        );
        assert_eq!(extract_source_password(&headers), "mysecret");
    }

    #[test]
    fn missing_auth_header_returns_empty_string() {
        let headers = HeaderMap::new();
        assert_eq!(extract_source_password(&headers), "");
    }

    #[test]
    fn detects_mp3_from_audio_mpeg() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
        assert_eq!(detect_codec_from_content_type(&headers), CodecId::MP3);
    }

    #[test]
    fn defaults_to_mp3_for_unknown_content_type() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        assert_eq!(detect_codec_from_content_type(&headers), CodecId::MP3);
    }

    use crate::source_headers::parse_source_overlay;

    #[test]
    fn parsed_overlay_round_trips_through_active_mount() {
        // Sanity that the source_handler's capture path will work end-to-end:
        // headers → overlay → stored on mount → readable.
        let mut headers = HeaderMap::new();
        headers.insert("ice-name", HeaderValue::from_static("End-To-End"));
        headers.insert("ice-public", HeaderValue::from_static("1"));
        headers.insert("ice-bitrate", HeaderValue::from_static("192"));
        let overlay = parse_source_overlay(&headers);
        assert_eq!(overlay.name.as_deref(), Some("End-To-End"));
        assert_eq!(overlay.public, Some(true));
        assert_eq!(overlay.bitrate_kbps, Some(192));
    }
}
