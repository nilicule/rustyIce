//! End-to-end tests for AutoDJ playback.
//!
//! Spins up a real server (bus + admin + stream routers) in-process with a
//! temp-dir fixture folder, lets the AutoDJ task drive audio through the
//! TranscodePipeline → BroadcastBus, and exercises listener + admin paths
//! against it. Mirrors the in-process pattern of `e2e_test.rs`.

use arc_swap::ArcSwap;
use rustyice_admin::{ListenerMap, build_admin_router};
use rustyice_auth::TomlBcryptAuth;
use rustyice_autodj::AutoDjPlayer;
use rustyice_autodj::test_fixtures::write_test_fixtures;
use rustyice_core::{
    config::{
        AuthConfig, AutoDjConfig, Config, LimitsConfig, LogFormat, LoggingConfig,
        Order, ServerConfig, TranscodeConfig, TranscodeFormat,
    },
    mount::{ActiveMount, MountInfo, MountMetadata, MountRegistry},
    types::CodecId,
};
use rustyice_ingest::IcecastIngest;
use rustyice_output::HttpPassthroughOutput;
use rustyice_server::{
    bus::TokioBroadcastBus,
    state::AppState,
    stream_listener::{PeerAddr, StreamListener},
    stream_router::build_stream_router,
};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

struct TestServer {
    stream_port: u16,
    admin_port: u16,
    shutdown: CancellationToken,
    _autodj_handle: tokio::task::JoinHandle<()>,
    _fixture_dir: TempDir,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Build a server with one AutoDJ pointed at a tempdir holding the
/// generated mp3 fixture. Returns the running ports + the AutoDjPlayer handle.
async fn build_server_with_autodj(loop_playlist: bool) -> TestServer {
    // 1. Generate a fixture folder.
    let fixture_dir = tempfile::tempdir().unwrap();
    let _ = write_test_fixtures(fixture_dir.path()).unwrap();

    // 2. Build the config.
    let autodj_cfg = AutoDjConfig {
        mount: "/auto".to_string(),
        folder: fixture_dir.path().to_path_buf(),
        name: Some("Test AutoDJ".to_string()),
        description: None,
        genre: None,
        url: None,
        enabled: true,
        loop_playlist,
        order: Order::Sequential,
        max_listeners: None,
        burst_size: None,
        transcode: TranscodeConfig {
            format: TranscodeFormat::Mp3,
            sample_rate: 44_100,
            bitrate_kbps: 64,
        },
    };
    let cfg = Config {
        server: ServerConfig {
            stream_bind: "127.0.0.1:0".parse().unwrap(),
            admin_bind: "127.0.0.1:0".parse().unwrap(),
            hostname: "localhost".to_string(),
        },
        logging: LoggingConfig { level: "error".to_string(), format: LogFormat::Pretty },
        // Global source_password lets a live PUT preempt the AutoDJ via the
        // auth fallback (verify_source falls through to verify_default_source
        // when there's no per-mount [[mounts]] entry).
        auth: AuthConfig {
            users: vec![],
            source_password: Some("letmesource".to_string()),
        },
        limits: LimitsConfig {
            max_listeners_global: 100,
            ring_size: 64,
            slow_listener_grace_s: 2,
            source_max_kbps: None,
            burst_size: 65_536,
        },
        mounts: vec![],
        autodjs: vec![autodj_cfg.clone()],
        relays: vec![],
        tls: None,
        transcode: None,
    };

    // 3. Build the mount, register it.
    let mounts = MountRegistry::new();
    let bus = Arc::new(TokioBroadcastBus::new(cfg.limits.ring_size, 65_536));
    let mount = Arc::new(ActiveMount::new(
        MountInfo {
            path: "/auto".to_string(),
            codec: CodecId::MP3,
            source_password: String::new(),
            max_listeners: None,
            metadata: MountMetadata {
                name: Some("Test AutoDJ".to_string()),
                ..Default::default()
            },
        },
        bus,
    ));
    mounts.add(mount.clone());

    // 4. Wire AppState.
    let shutdown = CancellationToken::new();
    let listeners = ListenerMap::new();
    let auth: Arc<dyn rustyice_core::traits::AuthBackend + Send + Sync> =
        Arc::new(TomlBcryptAuth::new(&cfg));
    let ingest: Arc<dyn rustyice_core::traits::IngestProtocol + Send + Sync> =
        Arc::new(IcecastIngest::default());
    let output: Arc<dyn rustyice_core::traits::OutputProtocol + Send + Sync> =
        Arc::new(HttpPassthroughOutput::default());
    let shared_cfg = Arc::new(ArcSwap::from_pointee(cfg.clone()));
    let app_state = AppState {
        mounts: mounts.clone(),
        auth: auth.clone(),
        ingest,
        output,
        listeners: listeners.clone(),
        config: shared_cfg.clone(),
        shutdown: shutdown.clone(),
    };

    // 5. Bind ephemeral ports.
    let stream_listener = TcpListener::bind(cfg.server.stream_bind).await.unwrap();
    let admin_listener = TcpListener::bind(cfg.server.admin_bind).await.unwrap();
    let stream_port = stream_listener.local_addr().unwrap().port();
    let admin_port = admin_listener.local_addr().unwrap().port();

    // 6. Build the prometheus handle the admin needs.
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let prom_handle = recorder.handle();
    // Best-effort install — duplicate set is fine across tests.
    let _ = metrics::set_global_recorder(recorder);
    struct NoopApplier;
    #[async_trait::async_trait]
    impl rustyice_admin::api::config::ConfigApplier for NoopApplier {
        async fn apply(&self, _: rustyice_core::config::Config) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }
    let admin_state = app_state.admin_state(
        prom_handle,
        Arc::new(ArcSwap::from_pointee(None)),
        Arc::new(NoopApplier) as rustyice_admin::api::config::ConfigApplierRef,
        Arc::new(tokio::sync::Mutex::new(())),
    );
    let admin_router = build_admin_router(admin_state);
    let stream_router = build_stream_router(app_state.clone());

    // 7. Spawn admin server.
    let admin_shutdown = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(admin_listener, admin_router)
            .with_graceful_shutdown(async move { admin_shutdown.cancelled().await })
            .await
            .ok();
    });

    // 8. Spawn stream server.
    let stream_shutdown = shutdown.clone();
    let dispatcher = StreamListener::new(stream_listener, app_state);
    tokio::spawn(async move {
        axum::serve(
            dispatcher,
            stream_router.into_make_service_with_connect_info::<PeerAddr>(),
        )
        .with_graceful_shutdown(async move { stream_shutdown.cancelled().await })
        .await
        .ok();
    });

    // 9. Spawn the AutoDjPlayer.
    let autodj_handle = AutoDjPlayer::from_config(&autodj_cfg, mount, shutdown.clone()).spawn();

    // Give everything a moment to settle.
    tokio::time::sleep(Duration::from_millis(150)).await;

    TestServer {
        stream_port,
        admin_port,
        shutdown,
        _autodj_handle: autodj_handle,
        _fixture_dir: fixture_dir,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn autodj_streams_mp3_with_icy_name() {
    let server = build_server_with_autodj(true).await;
    let url = format!("http://127.0.0.1:{}/auto", server.stream_port);

    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap().to_str().unwrap(),
        "audio/mpeg",
    );
    assert_eq!(
        resp.headers().get("icy-name").map(|v| v.to_str().unwrap()),
        Some("Test AutoDJ"),
    );

    // Read at least one chunk of bytes to prove audio is flowing.
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let chunk = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("no bytes received within 3s")
        .expect("stream ended without yielding a chunk")
        .unwrap();
    assert!(!chunk.is_empty(), "first chunk is empty");
}

#[tokio::test(flavor = "multi_thread")]
async fn autodj_loop_false_ends_after_playlist() {
    let server = build_server_with_autodj(false).await;
    let url = format!("http://127.0.0.1:{}/api/mounts", server.admin_port);

    // The fixture is ~1 s of audio at 64 kbps; with pacing, real wall-clock
    // playback is about 1 s. Allow up to 10 s for the player to fully exit
    // and source_connected to flip back to false.
    let start = std::time::Instant::now();
    let mut last_status = serde_json::Value::Null;
    loop {
        if start.elapsed() > Duration::from_secs(10) {
            panic!(
                "autodj did not exit within 10s; last admin state for /auto: {last_status}",
            );
        }
        let resp: serde_json::Value = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let entry = resp
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["path"] == "/auto");
        if let Some(e) = entry {
            last_status = e.clone();
            if !e["source_connected"].as_bool().unwrap_or(false) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn autodj_yields_to_live_source_and_resumes() {
    let server = build_server_with_autodj(true).await;
    let admin_url = format!("http://127.0.0.1:{}/api/mounts", server.admin_port);

    // Wait for the AutoDJ to claim the mount.
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(5) {
            panic!("autodj never claimed the mount");
        }
        let json: serde_json::Value = reqwest::Client::new()
            .get(&admin_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!([]));
        let kind = json
            .as_array()
            .and_then(|arr| arr.iter().find(|m| m["path"] == "/auto"))
            .and_then(|m| m["source_kind"].as_str().map(String::from));
        if kind.as_deref() == Some("autodj") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Live PUT preempts the AutoDJ using the global source_password.
    use bytes::Bytes;
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx));
    let put_url = format!("http://127.0.0.1:{}/auto", server.stream_port);
    let live_handle = tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .put(&put_url)
            .header("Authorization", "Basic c291cmNlOmxldG1lc291cmNl") // source:letmesource
            .header("Content-Type", "audio/mpeg")
            .body(body)
            .send()
            .await;
    });

    // Push a frame so the live source claims the slot.
    body_tx
        .send(Ok(Bytes::from_static(&[0xFF, 0xFB, 0x90, 0x04])))
        .await
        .unwrap();

    // Verify source_kind flipped to "live" within 2s.
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(2) {
            panic!("live source never preempted autodj");
        }
        let json: serde_json::Value = reqwest::Client::new()
            .get(&admin_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!([]));
        let kind = json
            .as_array()
            .and_then(|arr| arr.iter().find(|m| m["path"] == "/auto"))
            .and_then(|m| m["source_kind"].as_str().map(String::from));
        if kind.as_deref() == Some("live") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Disconnect live → AutoDJ resumes.
    drop(body_tx);
    let _ = live_handle.await;

    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(5) {
            panic!("autodj did not resume after live disconnect");
        }
        let json: serde_json::Value = reqwest::Client::new()
            .get(&admin_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!([]));
        let kind = json
            .as_array()
            .and_then(|arr| arr.iter().find(|m| m["path"] == "/auto"))
            .and_then(|m| m["source_kind"].as_str().map(String::from));
        if kind.as_deref() == Some("autodj") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
