use axum::body::Body;
use axum::http::{Request, StatusCode};
use metrics_exporter_prometheus::PrometheusBuilder;
use rustyice_admin::{build_admin_router, AdminState, ListenerMap};
use rustyice_core::mount::{ActiveMount, MountInfo, MountMetadata, MountRegistry};
use rustyice_core::traits::BroadcastBus;
use rustyice_core::types::{CodecId, StreamPacket};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
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

fn make_state() -> AdminState {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    AdminState {
        mounts: MountRegistry::new(),
        listeners: ListenerMap::new(),
        prometheus: handle,
        start_time: Instant::now(),
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

#[tokio::test]
async fn get_listeners_for_unknown_mount_returns_404() {
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
async fn kick_nonexistent_listener_returns_404() {
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
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
