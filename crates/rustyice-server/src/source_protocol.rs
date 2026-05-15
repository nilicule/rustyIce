//! Icecast SOURCE-protocol handler that operates directly on a `TcpStream`,
//! below axum and hyper.
//!
//! Common Icecast-2 source clients (butt, ezstream, EdCast, BUTT-Mac, mixxx,
//! liquidsoap with `output.icecast`) send requests that violate strict HTTP/1.1
//! body framing rules:
//!
//! - `PUT /mount HTTP/1.1` (or the legacy `SOURCE /mount HTTP/1.0`)
//! - `Expect: 100-continue`
//! - **No `Content-Length` and no `Transfer-Encoding: chunked`**
//! - Audio is streamed only *after* the server responds with `100 Continue`,
//!   then continues until the client disconnects.
//!
//! hyper 1.x rejects a request body with no length hint as a 0-byte body
//! (per RFC 7230) and does *not* auto-respond to `Expect: 100-continue`. Going
//! through axum/hyper therefore makes the handler observe an empty body, return
//! a final `200 OK`, and close — and the source client never gets a chance to
//! stream. We bypass hyper for these connections entirely.

use crate::bus::TokioBroadcastBus;
use crate::source_headers::parse_source_overlay;
use crate::state::AppState;
use crate::stream_router::mount_transcode;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use http::{header, HeaderMap};
use rustyice_admin::ListenerMap;
use rustyice_core::error::IngestError;
use rustyice_core::mount::{ActiveMount, MountInfo, MountMetadata, MountRegistry};
use rustyice_core::traits::IngestProtocol;
use rustyice_core::types::CodecId;
use rustyice_ingest::IcecastIngest;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{atomic::Ordering, Arc};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};
use tokio::net::{tcp::OwnedWriteHalf, TcpStream};
use tracing::{debug, info, warn};

/// Time we'll wait for a source client to finish sending its HTTP headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Hard cap on header block size; anything larger is malformed or hostile.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Handle a connection that's already been classified as a SOURCE-protocol
/// upload by the dispatcher (first bytes are `PUT ` or `SOURCE `).
pub async fn handle_source_connection(
    socket: TcpStream,
    peer_addr: SocketAddr,
    state: AppState,
) -> io::Result<()> {
    let (mut read_half, mut write_half) = socket.into_split_for_handshake();

    let parsed = match read_request_head(&mut read_half).await {
        Ok(p) => p,
        Err(e) => {
            warn!("source request parse failed from {peer_addr}: {e}");
            let _ = write_status(&mut write_half, 400, "Bad Request").await;
            return Err(e);
        }
    };

    let mount_path = parsed.path.clone();
    let headers = parsed.headers;
    let leftover = parsed.leftover;

    let password = extract_source_password(&headers);
    let codec = detect_codec_from_content_type(&headers);

    let (mount, dynamic) =
        match resolve_or_create_mount(&state, &mount_path, &password, codec.clone()).await {
            Ok(m) => m,
            Err(AuthOutcome::Unauthorized) => {
                let _ = write_status_with_auth_challenge(&mut write_half).await;
                return Ok(());
            }
            Err(AuthOutcome::Internal) => {
                let _ = write_status(&mut write_half, 500, "Internal Server Error").await;
                return Ok(());
            }
        };

    {
        let cfg = state.config.load();
        let transcode = mount_transcode(&cfg, &mount_path);
        if transcode.is_some()
            && !matches!(codec, CodecId::MP3 | CodecId::VORBIS)
        {
            let _ = write_status(
                &mut write_half,
                415,
                "mount requires MP3 or Ogg Vorbis source (transcoding is enabled)",
            )
            .await;
            return Ok(());
        }
    }

    if mount
        .source_connected
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        let _ = write_status(&mut write_half, 409, "mount already has an active source").await;
        return Ok(());
    }

    let source_cancel = state.shutdown.child_token();
    *mount.source_cancel.lock().unwrap() = Some(source_cancel.clone());
    *mount.connected_at.lock().unwrap() = Some(Instant::now());

    // Update the mount's advertised codec to match the connected source.  For
    // pre-configured mounts the registry was built with `CodecId::MP3` as a
    // placeholder; without this swap, a Vorbis passthrough source would still
    // serve listeners with `Content-Type: audio/mpeg` until reconfigured.
    {
        let current_info = mount.info.load();
        if current_info.codec != codec {
            let mut new_info = (**current_info).clone();
            new_info.codec = codec.clone();
            mount.info.store(Arc::new(new_info));
        }
    }

    let overlay = parse_source_overlay(&headers);
    mount.source_overlay.store(Arc::new(Some(overlay.clone())));

    // Runs cleanup even if the future is dropped mid-await (e.g. TCP reset).
    // For dynamic mounts, also removes the mount from the registry.
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
        peer = %peer_addr,
        name = ?overlay.name,
        description = ?overlay.description,
        genre = ?overlay.genre,
        url = ?overlay.url,
        public = ?overlay.public,
        audio_info = ?overlay.audio_info,
        bitrate_kbps = ?overlay.bitrate_kbps,
        "source connected"
    );

    // Respect Content-Length if the client provided one (curl, reqwest, …).
    // Without it, read until the client closes — that's how Icecast clients
    // signal end-of-stream.
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    // Reply *before* the body so the client knows to start streaming. Three
    // flavours, ordered most-specific first:
    //
    //   - `Expect: 100-continue` (butt, ezstream, …): reply with
    //     `HTTP/1.1 100 Continue`. They send the body after.
    //
    //   - Classic Icecast-2 source clients (libshout 2.x via Mixxx, EdCast,
    //     older Liquidsoap, …): no `Expect`, no `Content-Length`, no
    //     `Transfer-Encoding: chunked` — they sit in `SHOUT_STATE_RESP`
    //     until they read a successful status line. Reply with
    //     `HTTP/1.0 200 OK` upfront; without it they never transmit audio.
    //
    //   - Conventional HTTP clients with body framing (reqwest, curl, …):
    //     skip the upfront response. They've already started streaming the
    //     body; we read it and emit a normal final 200 OK after.
    //
    // Real Icecast 2 replies immediately after auth for the first two cases.
    // hyper does not auto-respond to `100-continue` in 1.x, and obviously
    // won't make up an upfront 200, so we do both by hand.
    let expects_continue = headers
        .get(header::EXPECT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("100-continue"));
    let is_chunked = headers
        .get(header::TRANSFER_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let is_classic_icecast = !expects_continue && content_length.is_none() && !is_chunked;
    let upfront_response: &[u8] = if expects_continue {
        b"HTTP/1.1 100 Continue\r\n\r\n"
    } else if is_classic_icecast {
        b"HTTP/1.0 200 OK\r\n\r\n"
    } else {
        b""
    };
    if !upfront_response.is_empty() {
        if let Err(e) = write_half.write_all(upfront_response).await {
            warn!("failed sending upfront response to {peer_addr}: {e}");
            return Ok(());
        }
        if let Err(e) = write_half.flush().await {
            warn!("failed flushing upfront response to {peer_addr}: {e}");
            return Ok(());
        }
    }

    let chained = ChainReader::new(leftover, read_half);
    // Wrap so every byte read from the network is reflected in
    // `mount.stats.bytes_received` for the live inbound-bandwidth gauge.
    // CountingReader stays innermost so the gauge reflects real bytes-on-wire
    // including any chunked framing overhead.
    let counted = CountingReader::new(chained, mount.stats.clone());
    let body_reader: Pin<Box<dyn AsyncRead + Send + Unpin>> = if is_chunked {
        // RFC 7230 §3.3.3: chunked supersedes Content-Length when both are set,
        // and end-of-stream is the zero-size chunk, not a fixed byte count.
        Box::pin(crate::chunked::ChunkedDecoder::new(counted))
    } else if let Some(len) = content_length {
        Box::pin(counted.take(len))
    } else {
        Box::pin(counted)
    };

    let ingest = build_ingest_for_mount(&state, &mount_path, &mount);
    let result = ingest
        .run(body_reader, mount.bus.clone(), codec, source_cancel)
        .await;

    match &result {
        Ok(stats) => info!(
            "source stream ended: mount={mount_path} bytes={} packets={}",
            stats.bytes_received, stats.packets_published
        ),
        Err(IngestError::TranscodeInit(e)) => {
            tracing::error!("transcode init failed for {mount_path}: {e}");
            let _ = write_status_retry_after(&mut write_half, 503, "transcoding unavailable", 60).await;
            return Ok(());
        }
        Err(IngestError::Io(e)) => debug!("source io error: mount={mount_path} err={e}"),
        Err(IngestError::Cancelled) => debug!("source cancelled: mount={mount_path}"),
        Err(IngestError::MountBusy) => debug!("source mount busy: mount={mount_path}"),
    }

    // Terminal 200 OK only when we haven't already sent one upfront. For
    // classic-Icecast clients we wrote `HTTP/1.0 200 OK` before reading the
    // body; sending a second status line now would either corrupt the
    // connection or (more typically) fail because the peer already closed.
    if !is_classic_icecast {
        let _ = write_status(&mut write_half, 200, "OK").await;
    }
    let _ = write_half.shutdown().await;
    Ok(())
}

// ── Request parsing ────────────────────────────────────────────────────────

struct ParsedHead {
    path: String,
    headers: HeaderMap,
    /// Any bytes read past the `\r\n\r\n` boundary — these are the start of
    /// the request body and must be replayed before further reads.
    leftover: Vec<u8>,
}

async fn read_request_head<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<ParsedHead> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + HEADER_READ_TIMEOUT;

    loop {
        let n = match tokio::time::timeout_at(deadline, reader.read(&mut tmp)).await {
            Ok(Ok(0)) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "client closed before completing request headers",
                ));
            }
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "header read timed out"))
            }
        };
        buf.extend_from_slice(&tmp[..n]);

        if buf.len() > MAX_HEADER_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "headers too large"));
        }

        let mut header_slots = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut header_slots);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(consumed)) => {
                let path = req
                    .path
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing path"))?
                    .to_string();
                let mut hm = HeaderMap::new();
                for h in req.headers.iter() {
                    if let (Ok(name), Ok(value)) = (
                        http::header::HeaderName::from_bytes(h.name.as_bytes()),
                        http::header::HeaderValue::from_bytes(h.value),
                    ) {
                        hm.append(name, value);
                    }
                }
                let leftover = buf[consumed..].to_vec();
                return Ok(ParsedHead { path, headers: hm, leftover });
            }
            Ok(httparse::Status::Partial) => continue,
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad request: {e}"),
                ))
            }
        }
    }
}

// ── Auth + mount resolution ────────────────────────────────────────────────

/// If `mount` is currently held by the in-process AutoDJ, set
/// `preempt_pending`, cancel the AutoDJ, and wait up to `wait_for` for it to
/// release the slot. Returns `true` on success, `false` on timeout.
pub(crate) async fn preempt_autodj_if_present(
    mount: &Arc<ActiveMount>,
    wait_for: Duration,
) -> bool {
    if !mount.source_connected.load(Ordering::Acquire)
        || !mount.source_is_autodj.load(Ordering::Acquire)
    {
        return true; // nothing to preempt
    }
    mount.preempt_pending.store(true, Ordering::Release);
    let token = mount.source_cancel.lock().unwrap().clone();
    if let Some(t) = token { t.cancel(); }

    let deadline = tokio::time::Instant::now() + wait_for;
    while mount.source_connected.load(Ordering::Acquire) {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

enum AuthOutcome {
    Unauthorized,
    Internal,
}

async fn resolve_or_create_mount(
    state: &AppState,
    mount_path: &str,
    password: &str,
    codec: CodecId,
) -> Result<(Arc<ActiveMount>, bool), AuthOutcome> {
    if let Some(mount) = state.mounts.get(mount_path) {
        if !preempt_autodj_if_present(&mount, Duration::from_millis(500)).await {
            warn!("autodj preempt timed out for {mount_path}");
            return Err(AuthOutcome::Internal);
        }
        return match state.auth.verify_source(mount_path, password).await {
            Ok(true) => Ok((mount, false)),
            Ok(false) => Err(AuthOutcome::Unauthorized),
            Err(e) => {
                warn!("auth error for {mount_path}: {e}");
                Err(AuthOutcome::Internal)
            }
        };
    }

    match state.auth.verify_default_source(password).await {
        Ok(true) => {
            let mount = create_dynamic_mount(&state.mounts, mount_path, codec, &state.config);
            Ok((mount, true))
        }
        Ok(false) => Err(AuthOutcome::Unauthorized),
        Err(e) => {
            warn!("auth error for {mount_path}: {e}");
            Err(AuthOutcome::Internal)
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
    let bus = Arc::new(TokioBroadcastBus::new(
        cfg.limits.ring_size,
        cfg.limits.burst_size as usize,
    ));
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

fn build_ingest_for_mount(
    state: &AppState,
    mount_path: &str,
    mount: &Arc<ActiveMount>,
) -> IcecastIngest {
    let cfg = state.config.load();
    let transcode = mount_transcode(&cfg, mount_path);
    let metadata = mount.info.load().metadata.clone();

    let mut ingest = IcecastIngest::default();
    if let Some(kbps) = cfg.limits.source_max_kbps {
        ingest = ingest.with_max_rate(u64::from(kbps) * 1000 / 8);
    }
    if let Some(tc) = transcode {
        ingest = ingest
            .with_transcode(tc)
            .with_transcode_comments(build_vorbis_comments(&metadata));
    }
    ingest.with_header_capture(mount.header_bytes.clone())
}

/// Build the Vorbis comment block embedded in the encoder when the transcode
/// target is Vorbis. Keys follow the Xiph Vorbis comment convention.
fn build_vorbis_comments(meta: &MountMetadata) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if let Some(s) = &meta.name {
        v.push(("TITLE".to_string(), s.clone()));
    }
    if let Some(s) = &meta.description {
        v.push(("DESCRIPTION".to_string(), s.clone()));
    }
    if let Some(s) = &meta.genre {
        v.push(("GENRE".to_string(), s.clone()));
    }
    if let Some(s) = &meta.url {
        v.push(("LOCATION".to_string(), s.clone()));
    }
    v.push((
        "ENCODER".to_string(),
        format!("rustyice/{}", env!("CARGO_PKG_VERSION")),
    ));
    v
}

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
        self.mount.header_bytes.store(Arc::new(None));
        // If an AutoDJ owns this mount path and was preempted by us, wake it.
        self.mount.autodj_resume.notify_one();
        if let Some(registry) = &self.mounts {
            let _ = registry.remove(&self.mount_path);
            info!("source disconnected: mount={} (dynamic mount removed)", self.mount_path);
        } else {
            info!("source disconnected: mount={}", self.mount_path);
        }
    }
}

// ── Header helpers ─────────────────────────────────────────────────────────

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
        Some(ct) if ct.contains("ogg") || ct.contains("vorbis") => CodecId::VORBIS,
        Some(ct) if ct.contains("aac") => CodecId::AAC,
        _ => CodecId::MP3,
    }
}

// ── Response writers ───────────────────────────────────────────────────────

async fn write_status(w: &mut OwnedWriteHalf, code: u16, reason: &str) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    w.write_all(response.as_bytes()).await?;
    w.flush().await
}

async fn write_status_retry_after(
    w: &mut OwnedWriteHalf,
    code: u16,
    reason: &str,
    retry_after_s: u32,
) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nRetry-After: {retry_after_s}\r\nConnection: close\r\n\r\n"
    );
    w.write_all(response.as_bytes()).await?;
    w.flush().await
}

async fn write_status_with_auth_challenge(w: &mut OwnedWriteHalf) -> io::Result<()> {
    let response = "HTTP/1.1 401 Unauthorized\r\n\
                    WWW-Authenticate: Basic realm=\"Icecast\"\r\n\
                    Content-Length: 0\r\n\
                    Connection: close\r\n\r\n";
    w.write_all(response.as_bytes()).await?;
    w.flush().await
}

// ── Reader that ticks `mount.stats.bytes_received` on each successful read ─

struct CountingReader<R> {
    inner: R,
    stats: Arc<rustyice_core::mount::MountStats>,
}

impl<R: AsyncRead + Unpin> CountingReader<R> {
    fn new(inner: R, stats: Arc<rustyice_core::mount::MountStats>) -> Self {
        Self { inner, stats }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for CountingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            let delta = buf.filled().len() - before;
            if delta > 0 {
                self.stats
                    .bytes_received
                    .fetch_add(delta as u64, Ordering::Relaxed);
            }
        }
        result
    }
}

// ── Reader that replays buffered prefix bytes before reading from inner ────

pub(crate) struct ChainReader<R> {
    buffer: Vec<u8>,
    pos: usize,
    inner: R,
}

impl<R> ChainReader<R> {
    fn new(buffer: Vec<u8>, inner: R) -> Self {
        Self { buffer, pos: 0, inner }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ChainReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pos < self.buffer.len() {
            let remaining = &self.buffer[self.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

// ── Local extension trait to mirror `TcpStream::into_split` naming ─────────
//
// `into_split` returns `(OwnedReadHalf, OwnedWriteHalf)`. We rename here only
// to keep the call site self-documenting at the dispatch boundary.

trait IntoSplitForHandshake {
    fn into_split_for_handshake(self) -> (tokio::net::tcp::OwnedReadHalf, OwnedWriteHalf);
}

impl IntoSplitForHandshake for TcpStream {
    fn into_split_for_handshake(self) -> (tokio::net::tcp::OwnedReadHalf, OwnedWriteHalf) {
        self.into_split()
    }
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

    #[tokio::test]
    async fn read_request_head_parses_butt_shaped_put() {
        // The exact wire shape captured from butt 1.46.
        let raw = b"PUT /stream2 HTTP/1.1\r\n\
                    Authorization: Basic c291cmNlOmxldG1lc291cmNl\r\n\
                    Host: localhost:9000\r\n\
                    User-Agent: butt 1.46.0\r\n\
                    Content-Type: audio/mpeg\r\n\
                    ice-bitrate: 128\r\n\
                    ice-name: no name\r\n\
                    ice-public: 0\r\n\
                    ice-audio-info: ice-bitrate=128;ice-channels=2;ice-samplerate=44100\r\n\
                    Expect: 100-continue\r\n\r\n\
                    LEFTOVER_BODY_BYTES";

        let mut cursor = tokio::io::BufReader::new(std::io::Cursor::new(&raw[..]));
        let parsed = read_request_head(&mut cursor).await.unwrap();

        assert_eq!(parsed.path, "/stream2");
        assert_eq!(parsed.headers.get("ice-name").unwrap(), "no name");
        assert_eq!(parsed.headers.get("expect").unwrap(), "100-continue");
        assert!(parsed.headers.get("content-length").is_none());
        assert_eq!(parsed.leftover, b"LEFTOVER_BODY_BYTES");
    }

    #[tokio::test]
    async fn chain_reader_yields_buffer_then_inner() {
        use tokio::io::AsyncReadExt;
        let inner = std::io::Cursor::new(b"WORLD".to_vec());
        let mut r = ChainReader::new(b"HELLO ".to_vec(), inner);
        let mut out = String::new();
        r.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "HELLO WORLD");
    }

    use rustyice_core::mount::{ActiveMount as TestActiveMount, MountInfo as TestMountInfo, MountMetadata as TestMountMetadata, MountRegistry as TestMountRegistry};
    use std::time::Duration as StdDuration;

    fn fake_autodj_mount() -> (TestMountRegistry, Arc<TestActiveMount>) {
        use crate::bus::TokioBroadcastBus;
        let bus = Arc::new(TokioBroadcastBus::new(64, 0));
        let info = TestMountInfo {
            path: "/auto".to_string(),
            codec: CodecId::MP3,
            source_password: "letmesource".to_string(),
            max_listeners: None,
            metadata: TestMountMetadata::default(),
        };
        let mount = Arc::new(TestActiveMount::new(info, bus));
        let registry = TestMountRegistry::new();
        registry.add(mount.clone());
        (registry, mount)
    }

    #[tokio::test]
    async fn preempt_autodj_cancels_token_and_waits_for_release() {
        let (_reg, mount) = fake_autodj_mount();
        // Pretend an AutoDJ has the slot.
        mount.source_connected.store(true, Ordering::Release);
        mount.source_is_autodj.store(true, Ordering::Release);
        let autodj_cancel = tokio_util::sync::CancellationToken::new();
        *mount.source_cancel.lock().unwrap() = Some(autodj_cancel.clone());

        // Spawn a fake "autodj task" that releases the slot when cancelled.
        let mount_clone = mount.clone();
        tokio::spawn(async move {
            autodj_cancel.cancelled().await;
            mount_clone.source_connected.store(false, Ordering::Release);
            mount_clone.source_is_autodj.store(false, Ordering::Release);
        });

        let ok = super::preempt_autodj_if_present(&mount, StdDuration::from_millis(500)).await;
        assert!(ok, "preempt should succeed inside the timeout");
        assert!(mount.preempt_pending.load(Ordering::Acquire), "preempt_pending must be set");
        assert!(!mount.source_connected.load(Ordering::Acquire), "slot must be released");
    }

    #[tokio::test]
    async fn preempt_returns_true_immediately_when_no_autodj() {
        let (_reg, mount) = fake_autodj_mount();
        // Mount is fresh — source_connected=false, source_is_autodj=false.
        let ok = super::preempt_autodj_if_present(&mount, StdDuration::from_millis(500)).await;
        assert!(ok);
        assert!(!mount.preempt_pending.load(Ordering::Acquire),
            "preempt_pending must NOT be set when there was nothing to preempt");
    }

    use crate::source_headers::parse_source_overlay;

    #[test]
    fn parsed_overlay_round_trips_through_active_mount() {
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
