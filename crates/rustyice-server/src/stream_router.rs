use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put},
    Router,
};
use futures::TryStreamExt;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rustyice_core::types::CodecId;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tokio_util::io::StreamReader;
use tokio_util::sync::CancellationToken;
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

    match state.auth.verify_source(&mount_path, &password).await {
        Ok(true) => {}
        Ok(false) => return unauthorized("invalid source password"),
        Err(e) => {
            warn!("auth error for {mount_path}: {e}");
            return server_error();
        }
    }

    let Some(mount) = state.mounts.get(&mount_path) else {
        return (StatusCode::NOT_FOUND, "mount not configured").into_response();
    };

    if mount
        .source_connected
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return (StatusCode::CONFLICT, "mount already has an active source").into_response();
    }

    let source_cancel = CancellationToken::new();
    let effective_cancel = state.shutdown.child_token();

    *mount.source_cancel.lock().await = Some(source_cancel.clone());
    *mount.connected_at.lock().unwrap() = Some(Instant::now());

    let codec = detect_codec_from_content_type(&headers);

    info!("source connected: mount={mount_path} codec={codec}");

    let stream = body
        .into_data_stream()
        .map_err(std::io::Error::other);
    let reader: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send + Unpin>> =
        Box::pin(StreamReader::new(stream));

    let _ = state
        .ingest
        .run(reader, mount.bus.clone(), codec, effective_cancel)
        .await;

    mount.source_connected.store(false, Ordering::Release);
    *mount.source_cancel.lock().await = None;
    *mount.connected_at.lock().unwrap() = None;

    info!("source disconnected: mount={mount_path}");

    StatusCode::OK.into_response()
}

// ── Listener handler ───────────────────────────────────────────────────────

async fn listener_handler(
    State(state): State<AppState>,
    Path(mount_segment): Path<String>,
    headers: HeaderMap,
) -> Response {
    let mount_path = format!("/{mount_segment}");

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
    let subscription = mount.bus.subscribe();

    let listener_cancel = state.shutdown.child_token();
    let listener_id = state
        .listeners
        .register(mount_path.clone(), listener_cancel.clone());

    let (read_end, write_end) = tokio::io::duplex(65_536);
    let writer: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send + Unpin>> =
        Box::pin(write_end);

    let output = state.output.clone();
    let listeners_ref = state.listeners.clone();
    let cancel_clone = listener_cancel.clone();

    tokio::spawn(async move {
        let _ = output
            .run(writer, subscription, mount_info, icy_requested, cancel_clone)
            .await;
        listeners_ref.deregister(listener_id);
    });

    let stream = tokio_util::io::ReaderStream::new(read_end);

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/mpeg")
        .header("icy-name", mount.info.load().metadata.name.as_deref().unwrap_or(""))
        .header("icy-br", "128");

    if icy_requested {
        builder = builder.header("icy-metaint", cfg.limits.ring_size.to_string());
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
}
