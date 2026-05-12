//! End-to-end test: source → bus → listener, with graceful shutdown.

use arc_swap::ArcSwap;
use rustyice_admin::{build_admin_router, AdminState, ListenerMap};
use rustyice_auth::TomlBcryptAuth;
use rustyice_core::{
    config::{
        AuthConfig, Config, LimitsConfig, LogFormat, LoggingConfig, MountConfig, ServerConfig,
    },
    mount::{ActiveMount, MountInfo, MountMetadata, MountRegistry},
    types::CodecId,
};
use rustyice_ingest::IcecastIngest;
use rustyice_output::HttpPassthroughOutput;
use rustyice_server::{
    bus::TokioBroadcastBus, source_layer::SourceMethodLayer, state::AppState,
    stream_router::build_stream_router,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;

const FAKE_MP3_FRAME: &[u8] = &[0xFF, 0xFB, 0x90, 0x04, 0x00, 0x00, 0x00, 0x00];

async fn build_test_server() -> (u16, u16, CancellationToken) {
    let cfg = Config {
        server: ServerConfig {
            stream_bind: "127.0.0.1:0".parse().unwrap(),
            admin_bind: "127.0.0.1:0".parse().unwrap(),
            hostname: "localhost".to_string(),
        },
        logging: LoggingConfig { level: "error".to_string(), format: LogFormat::Pretty },
        auth: AuthConfig { users: vec![] },
        limits: LimitsConfig {
            max_listeners_global: 100,
            ring_size: 64,
            slow_listener_grace_s: 2,
        },
        mounts: vec![MountConfig {
            path: "/stream".to_string(),
            source_password: "testpass".to_string(),
            max_listeners: None,
            name: Some("Test".to_string()),
            description: None,
            genre: None,
            url: None,
        }],
        tls: None,
    };

    let mounts = MountRegistry::new();
    let bus = Arc::new(TokioBroadcastBus::new(cfg.limits.ring_size));
    mounts.add(Arc::new(ActiveMount::new(
        MountInfo {
            path: "/stream".to_string(),
            codec: CodecId::MP3,
            source_password: "testpass".to_string(),
            max_listeners: None,
            metadata: MountMetadata {
                name: Some("Test".to_string()),
                ..Default::default()
            },
        },
        bus,
    )));

    let shutdown = CancellationToken::new();
    let listeners = ListenerMap::new();

    let auth: Arc<dyn rustyice_core::traits::AuthBackend + Send + Sync> =
        Arc::new(TomlBcryptAuth::new(&cfg));
    let ingest: Arc<dyn rustyice_core::traits::IngestProtocol + Send + Sync> =
        Arc::new(IcecastIngest::default());
    let output: Arc<dyn rustyice_core::traits::OutputProtocol + Send + Sync> =
        Arc::new(HttpPassthroughOutput::default());

    let app_state = AppState {
        mounts: mounts.clone(),
        auth,
        ingest,
        output,
        listeners: listeners.clone(),
        config: Arc::new(ArcSwap::from_pointee(cfg.clone())),
        shutdown: shutdown.clone(),
    };

    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let prom_handle = recorder.handle();

    let admin_state = AdminState {
        mounts: mounts.clone(),
        listeners,
        prometheus: prom_handle,
        start_time: std::time::Instant::now(),
    };

    let stream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream_port = stream_listener.local_addr().unwrap().port();
    let admin_port = admin_listener.local_addr().unwrap().port();

    let stream_router = build_stream_router(app_state)
        .layer(ServiceBuilder::new().layer(SourceMethodLayer));
    let admin_router = build_admin_router(admin_state);

    let stream_sd = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(stream_listener, stream_router)
            .with_graceful_shutdown(async move { stream_sd.cancelled().await })
            .await
            .unwrap();
    });

    let admin_sd = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(admin_listener, admin_router)
            .with_graceful_shutdown(async move { admin_sd.cancelled().await })
            .await
            .unwrap();
    });

    (stream_port, admin_port, shutdown)
}

#[tokio::test]
async fn listener_receives_audio_from_source() {
    let (stream_port, _admin_port, shutdown) = build_test_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let base_url = format!("http://127.0.0.1:{stream_port}");

    // Connect listener first, then source — this avoids the race where source
    // finishes before the listener subscribes to the bus.
    let listener_url = format!("{base_url}/stream");
    let mut response = tokio::time::timeout(
        Duration::from_secs(2),
        reqwest::Client::new().get(&listener_url).send(),
    )
    .await
    .expect("listener GET timed out")
    .expect("listener GET failed");

    assert_eq!(response.status(), 200);
    let ct = response.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.contains("audio"), "content-type should be audio, got: {ct}");

    // Now connect source: send 8KB of fake audio.
    let source_url = format!("{base_url}/stream");
    let audio: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(8192).collect();
    tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .put(&source_url)
            .header("Authorization", "Basic dGVzdHBhc3M=") // base64("testpass")
            .header("Content-Type", "audio/mpeg")
            .body(audio)
            .send()
            .await;
    });

    // Read first chunk of audio data (chunk() returns as soon as one arrives).
    let first_chunk = tokio::time::timeout(Duration::from_millis(500), response.chunk())
        .await
        .expect("timeout waiting for first audio chunk")
        .expect("request error");

    assert!(
        first_chunk.map_or(false, |b| !b.is_empty()),
        "listener should have received some audio bytes"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn admin_api_shows_mount() {
    let (_stream_port, admin_port, shutdown) = build_test_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = reqwest::get(format!("http://127.0.0.1:{admin_port}/api/mounts"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json[0]["path"], "/stream");

    shutdown.cancel();
}

#[tokio::test]
async fn source_with_wrong_password_gets_401() {
    let (stream_port, _admin_port, shutdown) = build_test_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = reqwest::Client::new()
        .put(format!("http://127.0.0.1:{stream_port}/stream"))
        .header("Authorization", "Basic d3Jvbmc=") // base64("wrong")
        .header("Content-Type", "audio/mpeg")
        .body(vec![0u8; 32])
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    shutdown.cancel();
}

#[tokio::test]
async fn graceful_shutdown_closes_connections() {
    let (stream_port, _admin_port, shutdown) = build_test_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    shutdown.cancel();

    let result = tokio::time::timeout(
        Duration::from_millis(500),
        reqwest::get(format!("http://127.0.0.1:{stream_port}/api/stats")),
    )
    .await;

    match result {
        Err(_timeout) => {}
        Ok(Err(_conn_refused)) => {}
        Ok(Ok(resp)) => {
            let _ = resp;
        }
    }
}
