use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use metrics_exporter_prometheus::PrometheusBuilder;
use rustyice_admin::{build_admin_router, AdminState, ListenerMap, SessionStore};
use rustyice_core::config::{
    AuthConfig, Config, LimitsConfig, LogFormat, LoggingConfig, ServerConfig,
};
use arc_swap::ArcSwap;
use rustyice_core::error::AuthError;
use rustyice_core::mount::{ActiveMount, MountInfo, MountMetadata, MountRegistry};
use rustyice_core::traits::{AuthBackend, BroadcastBus};
use rustyice_core::types::{CodecId, StreamPacket};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

struct NullBus;
impl BroadcastBus for NullBus {
    fn publish(&self, _: Arc<StreamPacket>) {}
    fn subscribe(
        &self,
    ) -> Pin<Box<dyn futures::Stream<Item = Arc<StreamPacket>> + Send + 'static>> {
        Box::pin(futures::stream::empty())
    }
    fn subscriber_count(&self) -> usize {
        0
    }
}

struct TestAuth {
    admin_password: Option<String>,
}

#[async_trait]
impl AuthBackend for TestAuth {
    async fn verify_admin(&self, username: &str, password: &str) -> Result<bool, AuthError> {
        Ok(username == "admin"
            && self.admin_password.as_deref() == Some(password))
    }
    async fn verify_source(&self, _mount_path: &str, _password: &str) -> Result<bool, AuthError> {
        Ok(false)
    }
    async fn reload(&self, _config: &Config) -> Result<(), AuthError> {
        Ok(())
    }
}

struct StubApplier;
#[async_trait]
impl rustyice_admin::api::config::ConfigApplier for StubApplier {
    async fn apply(&self, _new_cfg: Config) -> Result<Vec<String>, String> {
        Ok(vec![])
    }
}

fn make_state() -> AdminState {
    make_state_with_admin(None)
}

fn make_state_with_admin(password: Option<&str>) -> AdminState {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let cfg = Config {
        server: ServerConfig {
            stream_bind: "127.0.0.1:0".parse().unwrap(),
            admin_bind: "127.0.0.1:0".parse().unwrap(),
            hostname: "localhost".to_string(),
        },
        logging: LoggingConfig { level: "error".to_string(), format: LogFormat::Pretty },
        auth: AuthConfig::default(),
        limits: LimitsConfig {
            max_listeners_global: 100,
            ring_size: 64,
            slow_listener_grace_s: 2,
            source_max_kbps: None,
            burst_size: 65_536,
        },
        mounts: vec![],
        tls: None,
        transcode: None,
        autodjs: vec![],
        relays: vec![],
    };
    AdminState {
        mounts: MountRegistry::new(),
        listeners: ListenerMap::new(),
        prometheus: handle,
        start_time: Instant::now(),
        auth: Arc::new(TestAuth { admin_password: password.map(str::to_string) }),
        sessions: SessionStore::new(Duration::from_secs(3600)),
        version: "test",
        stream_port: 8000,
        config: Arc::new(ArcSwap::from_pointee(cfg)),
        config_path: Arc::new(ArcSwap::from_pointee(None::<PathBuf>)),
        config_applier: Arc::new(StubApplier),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
    }
}

fn add_mount(state: &AdminState, path: &str) {
    state.mounts.add(Arc::new(ActiveMount::new(
        MountInfo {
            path: path.to_string(),
            codec: CodecId::MP3,
            source_password: "secret".to_string(),
            max_listeners: None,
            metadata: MountMetadata::default(),
        },
        Arc::new(NullBus),
    )));
}

#[tokio::test]
async fn get_mounts_returns_empty_array() {
    let app = build_admin_router(make_state());
    let response = app
        .oneshot(Request::builder().uri("/api/mounts").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], b"[]");
}

#[tokio::test]
async fn get_mounts_lists_registered_mounts() {
    let state = make_state();
    add_mount(&state, "/stream");
    let app = build_admin_router(state);
    let response = app
        .oneshot(Request::builder().uri("/api/mounts").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json[0]["path"], "/stream");
    assert_eq!(json[0]["codec"], "mp3");
    assert_eq!(json[0]["source_connected"], false);
}

fn auth_cookie(state: &AdminState) -> String {
    let token = state.sessions.create("admin".to_string());
    format!("rustyice_session={token}")
}

#[tokio::test]
async fn get_listeners_without_cookie_returns_401() {
    let app = build_admin_router(make_state());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/mounts/nothere/listeners")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn get_listeners_for_unknown_mount_returns_404_with_session() {
    let state = make_state();
    let cookie = auth_cookie(&state);
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/mounts/nothere/listeners")
                .header("Cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_stats_returns_uptime_and_counts() {
    let state = make_state();
    add_mount(&state, "/a");
    add_mount(&state, "/b");
    let app = build_admin_router(state);
    let response = app
        .oneshot(Request::builder().uri("/api/stats").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total_mounts"], 2);
    assert_eq!(json["total_listeners"], 0);
}

#[tokio::test]
async fn kick_listener_without_cookie_returns_401() {
    let app = build_admin_router(make_state());
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/listeners/9999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn kick_nonexistent_listener_returns_404_with_session() {
    let state = make_state();
    let cookie = auth_cookie(&state);
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/listeners/9999")
                .header("Cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn login_with_correct_credentials_returns_cookie() {
    let app = build_admin_router(make_state_with_admin(Some("hunter2")));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"username":"admin","password":"hunter2"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response
        .headers()
        .get("set-cookie")
        .expect("set-cookie header")
        .to_str()
        .unwrap();
    assert!(cookie.starts_with("rustyice_session="));
    assert!(cookie.contains("HttpOnly"));
}

#[tokio::test]
async fn login_with_wrong_password_returns_401() {
    let app = build_admin_router(make_state_with_admin(Some("hunter2")));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"username":"admin","password":"nope"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_without_cookie_returns_401() {
    let app = build_admin_router(make_state());
    let response = app
        .oneshot(Request::builder().uri("/api/me").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_with_session_returns_user() {
    let state = make_state_with_admin(Some("hunter2"));
    let cookie = auth_cookie(&state);
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/me")
                .header("Cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["user"], "admin");
}

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_text() {
    let app = build_admin_router(make_state());
    let response = app
        .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let ct = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.contains("text/plain"));
}

#[tokio::test]
async fn get_mounts_exposes_title_field_default_null() {
    let state = make_state();
    add_mount(&state, "/stream");
    let app = build_admin_router(state);
    let response = app
        .oneshot(Request::builder().uri("/api/mounts").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json[0].as_object().unwrap().contains_key("title"));
    assert!(json[0]["title"].is_null(), "title defaults to null");
}

#[tokio::test]
async fn get_mounts_reflects_admin_set_title() {
    let state = make_state();
    add_mount(&state, "/stream");
    // Reach into the registry and set a title directly.
    let mount = state.mounts.get("/stream").unwrap();
    mount.current_title.store(Arc::new(Some("Now Playing".to_string())));

    let app = build_admin_router(state);
    let response = app
        .oneshot(Request::builder().uri("/api/mounts").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json[0]["title"], "Now Playing");
}

#[tokio::test]
async fn put_title_without_cookie_returns_401() {
    let state = make_state();
    add_mount(&state, "/stream");
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/mounts/stream/title")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"title":"Hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn put_title_stores_value_and_returns_200() {
    let state = make_state();
    add_mount(&state, "/stream");
    let cookie = auth_cookie(&state);
    let mount = state.mounts.get("/stream").unwrap();
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/mounts/stream/title")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"title":"Artist - Song"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let stored = mount.current_title.load_full();
    assert_eq!(stored.as_deref(), Some("Artist - Song"));
}

#[tokio::test]
async fn put_title_trims_whitespace() {
    let state = make_state();
    add_mount(&state, "/stream");
    let cookie = auth_cookie(&state);
    let mount = state.mounts.get("/stream").unwrap();
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/mounts/stream/title")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"title":"   padded   "}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let stored = mount.current_title.load_full();
    assert_eq!(stored.as_deref(), Some("padded"));
}

#[tokio::test]
async fn put_title_empty_after_trim_clears_value() {
    let state = make_state();
    add_mount(&state, "/stream");
    let cookie = auth_cookie(&state);
    let mount = state.mounts.get("/stream").unwrap();
    mount.current_title.store(Arc::new(Some("preexisting".to_string())));

    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/mounts/stream/title")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"title":"   "}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(mount.current_title.load().is_none());
}

#[tokio::test]
async fn put_title_rejects_single_quote() {
    let state = make_state();
    add_mount(&state, "/stream");
    let cookie = auth_cookie(&state);
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/mounts/stream/title")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"title":"don't"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_title_rejects_newline() {
    let state = make_state();
    add_mount(&state, "/stream");
    let cookie = auth_cookie(&state);
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/mounts/stream/title")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from("{\"title\":\"line1\\nline2\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_title_rejects_carriage_return() {
    let state = make_state();
    add_mount(&state, "/stream");
    let cookie = auth_cookie(&state);
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/mounts/stream/title")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from("{\"title\":\"line1\\rline2\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_title_rejects_too_long() {
    let state = make_state();
    add_mount(&state, "/stream");
    let cookie = auth_cookie(&state);
    let app = build_admin_router(state);
    let long = "a".repeat(257);
    let body = format!(r#"{{"title":"{long}"}}"#);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/mounts/stream/title")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_title_on_unknown_mount_returns_404() {
    let state = make_state();
    let cookie = auth_cookie(&state);
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/mounts/nope/title")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"title":"Hi"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_title_clears_value() {
    let state = make_state();
    add_mount(&state, "/stream");
    let cookie = auth_cookie(&state);
    let mount = state.mounts.get("/stream").unwrap();
    mount.current_title.store(Arc::new(Some("preexisting".to_string())));

    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/mounts/stream/title")
                .header("Cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(mount.current_title.load().is_none());
}

#[tokio::test]
async fn delete_title_without_cookie_returns_401() {
    let state = make_state();
    add_mount(&state, "/stream");
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/mounts/stream/title")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ─── config view ──────────────────────────────────────────────────────────

use rustyice_core::config::{MountConfig, UserConfig};

fn session_cookie(state: &AdminState, user: &str) -> String {
    let token = state.sessions.create(user.to_string());
    format!("rustyice_session={token}")
}

#[tokio::test]
async fn get_config_requires_auth() {
    let state = make_state();
    let app = build_admin_router(state);
    let response = app
        .oneshot(Request::builder().uri("/api/config").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn get_config_redacts_secrets() {
    let state = make_state();
    let cookie = session_cookie(&state, "admin");
    let mut cfg = (*state.config.load_full()).clone();
    cfg.auth.users.push(UserConfig {
        username: "admin".into(),
        password_bcrypt: "$2y$12$realhashplaceholder".into(),
    });
    cfg.auth.source_password = Some("supersecret".into());
    cfg.mounts.push(MountConfig {
        path: "/m".into(),
        source_password: "mountpw".into(),
        max_listeners: None,
        name: None,
        description: None,
        genre: None,
        url: None,
        transcode: None,
        burst_size: None,
    });
    state.config.store(Arc::new(cfg));

    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .header("Cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(!body_str.contains("$2y$12$realhashplaceholder"), "bcrypt leaked: {body_str}");
    assert!(!body_str.contains("supersecret"), "source_password leaked: {body_str}");
    assert!(!body_str.contains("mountpw"), "mount source_password leaked: {body_str}");
    assert!(body_str.contains("***"));
}

#[tokio::test]
async fn get_config_includes_path_and_source() {
    let state = make_state();
    let cookie = session_cookie(&state, "admin");
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .header("Cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["path"], serde_json::Value::Null);
    assert_eq!(json["source"], "defaults");
}

// ─── PUT /api/config/server tests ─────────────────────────────────────────

struct RecordingApplier {
    last: std::sync::Mutex<Option<Config>>,
    fail_with: Option<String>,
    warnings: Vec<String>,
}

impl RecordingApplier {
    fn new() -> Self {
        Self { last: std::sync::Mutex::new(None), fail_with: None, warnings: vec![] }
    }
    #[allow(dead_code)]
    fn with_warnings(ws: Vec<String>) -> Self {
        Self { last: std::sync::Mutex::new(None), fail_with: None, warnings: ws }
    }
    #[allow(dead_code)]
    fn failing(msg: &str) -> Self {
        Self {
            last: std::sync::Mutex::new(None),
            fail_with: Some(msg.to_string()),
            warnings: vec![],
        }
    }
    fn take(&self) -> Option<Config> {
        self.last.lock().unwrap().take()
    }
}

#[async_trait]
impl rustyice_admin::api::config::ConfigApplier for RecordingApplier {
    async fn apply(&self, new_cfg: Config) -> Result<Vec<String>, String> {
        *self.last.lock().unwrap() = Some(new_cfg);
        if let Some(msg) = &self.fail_with {
            return Err(msg.clone());
        }
        Ok(self.warnings.clone())
    }
}

fn make_state_with_applier(applier: Arc<RecordingApplier>) -> AdminState {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let cfg = Config {
        server: ServerConfig {
            stream_bind: "127.0.0.1:0".parse().unwrap(),
            admin_bind: "127.0.0.1:0".parse().unwrap(),
            hostname: "localhost".to_string(),
        },
        logging: LoggingConfig { level: "error".to_string(), format: LogFormat::Pretty },
        auth: AuthConfig::default(),
        limits: LimitsConfig {
            max_listeners_global: 100,
            ring_size: 64,
            slow_listener_grace_s: 2,
            source_max_kbps: None,
            burst_size: 65_536,
        },
        mounts: vec![],
        tls: None,
        transcode: None,
        autodjs: vec![],
        relays: vec![],
    };
    AdminState {
        mounts: MountRegistry::new(),
        listeners: ListenerMap::new(),
        prometheus: handle,
        start_time: Instant::now(),
        auth: Arc::new(TestAuth { admin_password: None }),
        sessions: SessionStore::new(Duration::from_secs(3600)),
        version: "test",
        stream_port: 8000,
        config: Arc::new(ArcSwap::from_pointee(cfg)),
        config_path: Arc::new(ArcSwap::from_pointee(None::<PathBuf>)),
        config_applier: applier,
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
    }
}

fn install_tempfile_config(state: &AdminState, dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("config.toml");
    let toml_str = toml::to_string(&*state.config.load_full()).unwrap();
    std::fs::write(&path, toml_str).unwrap();
    state.config_path.store(Arc::new(Some(path.clone())));
    path
}

fn server_patch_json(hostname: &str) -> serde_json::Value {
    serde_json::json!({
        "server":  { "stream_bind": "0.0.0.0:8000", "admin_bind": "127.0.0.1:8001", "hostname": hostname },
        "logging": { "level": "info", "format": "pretty" },
        "limits":  {
            "max_listeners_global": 500,
            "ring_size": 64,
            "slow_listener_grace_s": 2,
            "burst_size": 65536,
            "source_max_kbps": null
        }
    })
}

#[tokio::test]
async fn put_server_happy_path() {
    let applier = Arc::new(RecordingApplier::new());
    let state = make_state_with_applier(applier.clone());
    let dir = tempfile::tempdir().unwrap();
    let path = install_tempfile_config(&state, &dir);
    let cookie = session_cookie(&state, "admin");

    let body = server_patch_json("radio.example");
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/server")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let applied = applier.take().expect("applier was called");
    assert_eq!(applied.server.hostname, "radio.example");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("radio.example"), "config.toml missing new hostname:\n{on_disk}");
}

#[tokio::test]
async fn put_server_rejects_invalid_socket_addr() {
    let applier = Arc::new(RecordingApplier::new());
    let state = make_state_with_applier(applier.clone());
    let dir = tempfile::tempdir().unwrap();
    let _ = install_tempfile_config(&state, &dir);
    let cookie = session_cookie(&state, "admin");

    let body = serde_json::json!({
        "server":  { "stream_bind": "not-a-socket", "admin_bind": "127.0.0.1:8001", "hostname": "h" },
        "logging": { "level": "info", "format": "pretty" },
        "limits":  { "max_listeners_global": 500, "ring_size": 64, "slow_listener_grace_s": 2, "burst_size": 0, "source_max_kbps": null }
    });

    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/server")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    // Axum's Json extractor returns 422 for malformed values (e.g. a
    // SocketAddr that can't parse). Semantic errors caught downstream still
    // return 400; structural errors come back as 422.
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(applier.take().is_none(), "applier should not have been called");
}

#[tokio::test]
async fn put_server_rejects_zero_ring_size() {
    let applier = Arc::new(RecordingApplier::new());
    let state = make_state_with_applier(applier.clone());
    let dir = tempfile::tempdir().unwrap();
    let path = install_tempfile_config(&state, &dir);
    let cookie = session_cookie(&state, "admin");
    let before = std::fs::read_to_string(&path).unwrap();

    let body = serde_json::json!({
        "server":  { "stream_bind": "0.0.0.0:8000", "admin_bind": "127.0.0.1:8001", "hostname": "h" },
        "logging": { "level": "info", "format": "pretty" },
        "limits":  { "max_listeners_global": 500, "ring_size": 0, "slow_listener_grace_s": 2, "burst_size": 65536, "source_max_kbps": null }
    });

    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/server")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(json["field"], "limits.ring_size");
    assert_eq!(json["disk_written"], false);

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(before, after, "disk should not have been touched on validation failure");
    assert!(applier.take().is_none());
}

#[tokio::test]
async fn put_server_requires_auth() {
    let state = make_state();
    let app = build_admin_router(state);
    let body = server_patch_json("h");
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/server")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn put_server_passes_through_warnings() {
    let applier = Arc::new(RecordingApplier::with_warnings(vec![
        "stream_bind changed — restart required for this to take effect".into(),
    ]));
    let state = make_state_with_applier(applier.clone());
    let dir = tempfile::tempdir().unwrap();
    let _ = install_tempfile_config(&state, &dir);
    let cookie = session_cookie(&state, "admin");

    let body = serde_json::json!({
        "server":  { "stream_bind": "0.0.0.0:9000", "admin_bind": "127.0.0.1:8001", "hostname": "h" },
        "logging": { "level": "info", "format": "pretty" },
        "limits":  { "max_listeners_global": 500, "ring_size": 64, "slow_listener_grace_s": 2, "burst_size": 65536, "source_max_kbps": null }
    });

    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/server")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    let warnings = json["applied_warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains("stream_bind")),
        "expected stream_bind warning in {warnings:?}",
    );
}

#[tokio::test]
async fn put_server_defaults_mode_bootstraps_config_toml() {
    let applier = Arc::new(RecordingApplier::new());
    let state = make_state_with_applier(applier.clone());
    let cookie = session_cookie(&state, "admin");
    // config_path stays None (defaults mode).

    let dir = tempfile::tempdir().unwrap();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let body = server_patch_json("new-host");

    let path_swap = state.config_path.clone();
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/server")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();

    let bootstrapped = dir.path().join("config.toml");
    let file_exists = bootstrapped.exists();
    let file_contents = if file_exists {
        std::fs::read_to_string(&bootstrapped).unwrap()
    } else {
        String::new()
    };
    let path_after = path_swap.load_full().as_ref().clone();
    std::env::set_current_dir(prev_cwd).unwrap();

    assert_eq!(status, StatusCode::OK);
    assert!(file_exists, "config.toml should have been bootstrapped");
    assert!(
        file_contents.contains("new-host"),
        "bootstrap missing patch value:\n{file_contents}",
    );
    assert!(path_after.is_some(), "config_path should be installed after first save");
    assert!(applier.take().is_some());
}

#[tokio::test]
async fn put_server_disk_unwritable_returns_500_and_skips_apply() {
    let applier = Arc::new(RecordingApplier::new());
    let state = make_state_with_applier(applier.clone());
    state.config_path.store(Arc::new(Some(PathBuf::from(
        "/this/path/definitely/does/not/exist/config.toml",
    ))));
    let cookie = session_cookie(&state, "admin");

    let body = server_patch_json("h");
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/server")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(json["disk_written"], false);
    assert!(applier.take().is_none(), "applier must not be called on disk failure");
}

#[tokio::test]
async fn put_server_apply_failed_returns_500_with_disk_written_true() {
    let applier = Arc::new(RecordingApplier::failing("auth reload: boom"));
    let state = make_state_with_applier(applier.clone());
    let dir = tempfile::tempdir().unwrap();
    let path = install_tempfile_config(&state, &dir);
    let cookie = session_cookie(&state, "admin");

    let body = server_patch_json("applied-fail-host");
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/server")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(json["disk_written"], true);
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(
        on_disk.contains("applied-fail-host"),
        "disk should reflect requested change even though apply failed:\n{on_disk}",
    );
}

#[tokio::test]
async fn put_server_concurrent_saves_serialize() {
    let applier = Arc::new(RecordingApplier::new());
    let state = make_state_with_applier(applier.clone());
    let dir = tempfile::tempdir().unwrap();
    let path = install_tempfile_config(&state, &dir);
    let cookie_a = session_cookie(&state, "admin");
    let cookie_b = session_cookie(&state, "admin");

    let body_a = server_patch_json("alpha");
    let body_b = server_patch_json("beta");

    let app = build_admin_router(state);
    let app2 = app.clone();
    let (r1, r2) = tokio::join!(
        app.oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/server")
                .header("Cookie", cookie_a)
                .header("Content-Type", "application/json")
                .body(Body::from(body_a.to_string()))
                .unwrap()
        ),
        app2.oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/server")
                .header("Cookie", cookie_b)
                .header("Content-Type", "application/json")
                .body(Body::from(body_b.to_string()))
                .unwrap()
        ),
    );
    assert_eq!(r1.unwrap().status(), StatusCode::OK);
    assert_eq!(r2.unwrap().status(), StatusCode::OK);
    let on_disk = std::fs::read_to_string(&path).unwrap();
    let has_alpha = on_disk.contains(r#""alpha""#);
    let has_beta = on_disk.contains(r#""beta""#);
    assert!(has_alpha ^ has_beta, "expected exactly one hostname, got:\n{on_disk}");
}

// ─── PUT /api/config/transcode tests ──────────────────────────────────────

fn transcode_set_body() -> serde_json::Value {
    serde_json::json!({
        "transcode": { "format": "vorbis", "sample_rate": 48000, "bitrate_kbps": 192 }
    })
}

fn transcode_clear_body() -> serde_json::Value {
    serde_json::json!({ "transcode": null })
}

#[tokio::test]
async fn put_transcode_writes_block_to_disk_and_applies() {
    let applier = Arc::new(RecordingApplier::new());
    let state = make_state_with_applier(applier.clone());
    let dir = tempfile::tempdir().unwrap();
    let path = install_tempfile_config(&state, &dir);
    let cookie = session_cookie(&state, "admin");

    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/transcode")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(transcode_set_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let applied = applier.take().expect("applier was called");
    let tc = applied.transcode.expect("transcode set on candidate");
    assert_eq!(tc.sample_rate, 48000);
    assert_eq!(tc.bitrate_kbps, 192);

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("[transcode]"), "missing [transcode]:\n{on_disk}");
    assert!(on_disk.contains(r#"format = "vorbis""#));
    assert!(on_disk.contains("bitrate_kbps = 192"));
}

#[tokio::test]
async fn put_transcode_with_null_removes_block_from_disk() {
    let applier = Arc::new(RecordingApplier::new());
    let state = make_state_with_applier(applier.clone());
    // Seed config with a transcode block present.
    {
        let mut cfg = (*state.config.load_full()).clone();
        cfg.transcode = Some(rustyice_core::config::TranscodeConfig {
            format: rustyice_core::config::TranscodeFormat::Mp3,
            sample_rate: 44_100,
            bitrate_kbps: 128,
        });
        state.config.store(Arc::new(cfg));
    }
    let dir = tempfile::tempdir().unwrap();
    let path = install_tempfile_config(&state, &dir);
    let cookie = session_cookie(&state, "admin");

    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/transcode")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(transcode_clear_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let applied = applier.take().expect("applier was called");
    assert!(applied.transcode.is_none(), "candidate should have transcode = None");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(!on_disk.contains("[transcode]"), "block should be gone:\n{on_disk}");
}

#[tokio::test]
async fn put_transcode_rejects_bad_format() {
    let applier = Arc::new(RecordingApplier::new());
    let state = make_state_with_applier(applier.clone());
    let dir = tempfile::tempdir().unwrap();
    let path = install_tempfile_config(&state, &dir);
    let cookie = session_cookie(&state, "admin");
    let before = std::fs::read_to_string(&path).unwrap();

    let body = serde_json::json!({
        "transcode": { "format": "flac", "sample_rate": 44100, "bitrate_kbps": 128 }
    });

    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/transcode")
                .header("Cookie", cookie)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(json["field"], "transcode.format");
    assert!(applier.take().is_none());
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(before, after, "disk should be untouched on validation failure");
}

#[tokio::test]
async fn put_transcode_requires_auth() {
    let state = make_state();
    let app = build_admin_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/transcode")
                .header("Content-Type", "application/json")
                .body(Body::from(transcode_set_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
