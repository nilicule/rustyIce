//! HTTP client for relay upstream connections. Issues a GET, parses the
//! Content-Type into a `CodecId`, and exposes the response body as a
//! `Stream<Item = io::Result<Bytes>>`. Optional HTTP Basic credentials are
//! taken from the config.

use bytes::Bytes;
use futures::Stream;
use rustyice_core::config::RelayConfig;
use rustyice_core::types::CodecId;
use std::pin::Pin;
use std::time::Duration;

/// Errors from `open_upstream`.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("upstream returned non-success status: {0}")]
    UpstreamStatus(u16),
    #[error("upstream request failed: {0}")]
    Request(String),
    #[error("relay config has invalid credentials")]
    InvalidCredentials,
}

/// Map a Content-Type value to a `CodecId`. Same heuristic as the live
/// SOURCE-protocol handler: `mpeg` → MP3, `ogg`/`vorbis` → VORBIS,
/// everything else (including missing header) → MP3.
#[must_use]
pub fn detect_codec(content_type: Option<&str>) -> CodecId {
    match content_type {
        Some(ct) if ct.contains("mpeg") => CodecId::MP3,
        Some(ct) if ct.contains("ogg") || ct.contains("vorbis") => CodecId::VORBIS,
        _ => CodecId::MP3,
    }
}

/// Type alias for the body stream returned by `open_upstream`.
pub type ResponseStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send + Unpin>>;

/// Construct the GET request builder for a relay's upstream, including
/// optional HTTP Basic auth and a `User-Agent` identifying the relay.
fn build_request(client: &reqwest::Client, cfg: &RelayConfig) -> reqwest::RequestBuilder {
    let mut req = client
        .get(&cfg.upstream)
        .header(
            "User-Agent",
            format!("rustyice/{} (relay)", env!("CARGO_PKG_VERSION")),
        )
        .header("Icy-Metadata", "0");
    if let (Some(u), Some(p)) = (cfg.username.as_deref(), cfg.password.as_deref()) {
        req = req.basic_auth(u, Some(p));
    }
    req
}

/// Issue a GET to the upstream and return the detected codec plus the body
/// stream. The caller wraps the stream with `tokio_util::io::StreamReader` to
/// get an `AsyncRead` suitable for `IcecastIngest::run`.
///
/// # Errors
/// Returns [`RelayError`] on transport failure or a non-2xx response.
pub async fn open_upstream(
    cfg: &RelayConfig,
) -> Result<(CodecId, ResponseStream), RelayError> {
    if cfg.username.is_some() != cfg.password.is_some() {
        return Err(RelayError::InvalidCredentials);
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| RelayError::Request(e.to_string()))?;
    let resp = build_request(&client, cfg)
        .send()
        .await
        .map_err(|e| RelayError::Request(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(RelayError::UpstreamStatus(status.as_u16()));
    }
    let codec = detect_codec(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
    );
    use futures::StreamExt;
    let stream = resp.bytes_stream().map(|chunk| {
        chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
    });
    Ok((codec, Box::pin(stream)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_creds(user: Option<&str>, pass: Option<&str>) -> RelayConfig {
        RelayConfig {
            mount: "/r".into(),
            upstream: "http://example.com/stream".into(),
            name: None,
            description: None,
            genre: None,
            url: None,
            enabled: true,
            username: user.map(String::from),
            password: pass.map(String::from),
            max_listeners: None,
            burst_size: None,
            transcode: None,
        }
    }

    #[test]
    fn detect_codec_handles_known_content_types() {
        assert_eq!(detect_codec(Some("audio/mpeg")), CodecId::MP3);
        assert_eq!(detect_codec(Some("application/ogg")), CodecId::VORBIS);
        assert_eq!(detect_codec(Some("audio/ogg")), CodecId::VORBIS);
        assert_eq!(detect_codec(Some("audio/vorbis")), CodecId::VORBIS);
        assert_eq!(detect_codec(Some("audio/aac")), CodecId::MP3); // default
        assert_eq!(detect_codec(None), CodecId::MP3);
    }

    #[test]
    fn build_request_sets_basic_auth_when_creds_present() {
        let cfg = cfg_with_creds(Some("user"), Some("secret"));
        let req = build_request(&reqwest::Client::new(), &cfg).build().unwrap();
        let auth = req.headers().get("authorization").unwrap().to_str().unwrap();
        // base64("user:secret") = "dXNlcjpzZWNyZXQ="
        assert_eq!(auth, "Basic dXNlcjpzZWNyZXQ=");
        let ua = req.headers().get("user-agent").unwrap().to_str().unwrap();
        assert!(ua.starts_with("rustyice/"));
        assert!(ua.contains("(relay)"));
    }

    #[test]
    fn build_request_omits_basic_auth_when_no_creds() {
        let cfg = cfg_with_creds(None, None);
        let req = build_request(&reqwest::Client::new(), &cfg).build().unwrap();
        assert!(req.headers().get("authorization").is_none());
    }
}
