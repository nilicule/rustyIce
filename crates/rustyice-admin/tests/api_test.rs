use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use metrics_exporter_prometheus::PrometheusBuilder;
use rustyice_admin::{build_admin_router, AdminState, ListenerMap, SessionStore};
use rustyice_core::config::Config;
use rustyice_core::error::AuthError;
use rustyice_core::mount::{ActiveMount, MountInfo, MountMetadata, MountRegistry};
use rustyice_core::traits::{AuthBackend, BroadcastBus};
use rustyice_core::types::{CodecId, StreamPacket};
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

fn make_state() -> AdminState {
    make_state_with_admin(None)
}

fn make_state_with_admin(password: Option<&str>) -> AdminState {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    AdminState {
        mounts: MountRegistry::new(),
        listeners: ListenerMap::new(),
        prometheus: handle,
        start_time: Instant::now(),
        auth: Arc::new(TestAuth { admin_password: password.map(str::to_string) }),
        sessions: SessionStore::new(Duration::from_secs(3600)),
        version: "test",
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
