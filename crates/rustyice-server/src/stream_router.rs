use crate::state::AppState;
use crate::stream_listener::PeerAddr;
use axum::{
    body::Body,
    extract::{ConnectInfo, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

/// Builds the router for listener traffic on the stream port.
///
/// Source uploads (PUT/SOURCE) are intercepted by
/// `crate::source_protocol::handle_source_connection` *before* the connection
/// reaches axum, because the Icecast source protocol uses request framing that
/// hyper rejects as non-RFC-compliant HTTP/1.1 (no `Content-Length`, no
/// `Transfer-Encoding`, `Expect: 100-continue` without auto-response).
pub fn build_stream_router(state: AppState) -> Router {
    Router::new()
        .route("/{mount}", get(listener_handler))
        .with_state(state)
}

/// Returns the transcode config in effect for `mount_path`, falling back to
/// the global `[transcode]` block for dynamic mounts.
pub(crate) fn mount_transcode(
    cfg: &rustyice_core::config::Config,
    mount_path: &str,
) -> Option<rustyice_core::config::TranscodeConfig> {
    if let Some(mc) = cfg.mounts.iter().find(|m| m.path == mount_path) {
        cfg.effective_transcode(mc).cloned()
    } else {
        cfg.transcode.clone()
    }
}

async fn listener_handler(
    State(state): State<AppState>,
    Path(mount_segment): Path<String>,
    ConnectInfo(peer): ConnectInfo<PeerAddr>,
    headers: HeaderMap,
) -> Response {
    let mount_path = format!("/{mount_segment}");
    let peer_addr = Some(peer.0);

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
    let source_overlay = mount.source_overlay.clone();
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
            .run(writer, subscription, mount_info, current_title, source_overlay, icy_requested, cancel_clone)
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

    let transcode = mount_transcode(&cfg, &mount_path);
    let identity = mount.effective_identity(transcode.as_ref());

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/mpeg");

    if let Some(v) = &identity.name {
        builder = builder.header("icy-name", v);
    }
    if let Some(v) = &identity.description {
        builder = builder.header("icy-description", v);
    }
    if let Some(v) = &identity.genre {
        builder = builder.header("icy-genre", v);
    }
    if let Some(v) = &identity.url {
        builder = builder.header("icy-url", v);
    }
    if let Some(p) = identity.public {
        builder = builder.header("icy-pub", if p { "1" } else { "0" });
    }
    if let Some(v) = &identity.audio_info {
        builder = builder.header("ice-audio-info", v);
    }
    if let Some(b) = identity.bitrate_kbps {
        builder = builder.header("icy-br", b.to_string());
    }

    if icy_requested {
        builder = builder.header("icy-metaint", "8192");
    }

    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
