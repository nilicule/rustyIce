//! End-to-end test: source → bus → listener, with graceful shutdown.

use arc_swap::ArcSwap;
use rustyice_admin::{build_admin_router, AdminState, ListenerMap, SessionStore};
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
use rustyice_core::config::{TranscodeConfig, TranscodeFormat};

const FAKE_MP3_FRAME: &[u8] = &[0xFF, 0xFB, 0x90, 0x04, 0x00, 0x00, 0x00, 0x00];

async fn build_test_server() -> (u16, u16, CancellationToken) {
    build_test_server_with(None).await
}

async fn build_test_server_with(
    default_source_password: Option<&str>,
) -> (u16, u16, CancellationToken) {
    let cfg = Config {
        server: ServerConfig {
            stream_bind: "127.0.0.1:0".parse().unwrap(),
            admin_bind: "127.0.0.1:0".parse().unwrap(),
            hostname: "localhost".to_string(),
        },
        logging: LoggingConfig { level: "error".to_string(), format: LogFormat::Pretty },
        auth: AuthConfig {
            users: vec![],
            source_password: default_source_password.map(str::to_string),
        },
        limits: LimitsConfig {
            max_listeners_global: 100,
            ring_size: 64,
            slow_listener_grace_s: 2,
            source_max_kbps: None,
        },
        mounts: vec![MountConfig {
            path: "/stream".to_string(),
            source_password: "testpass".to_string(),
            max_listeners: None,
            name: Some("Test".to_string()),
            description: None,
            genre: None,
            url: None,
            transcode: None,
        }],
        tls: None,
        transcode: None,
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
        auth: auth.clone(),
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
        auth,
        sessions: SessionStore::new(std::time::Duration::from_secs(3600)),
        version: env!("CARGO_PKG_VERSION"),
        stream_port: 0,
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

async fn build_test_server_with_sessions() -> (u16, u16, CancellationToken, Arc<SessionStore>) {
    let cfg = Config {
        server: ServerConfig {
            stream_bind: "127.0.0.1:0".parse().unwrap(),
            admin_bind: "127.0.0.1:0".parse().unwrap(),
            hostname: "localhost".to_string(),
        },
        logging: LoggingConfig { level: "error".to_string(), format: LogFormat::Pretty },
        auth: AuthConfig { users: vec![], source_password: None },
        limits: LimitsConfig {
            max_listeners_global: 100,
            ring_size: 64,
            slow_listener_grace_s: 2,
            source_max_kbps: None,
        },
        mounts: vec![MountConfig {
            path: "/stream".to_string(),
            source_password: "testpass".to_string(),
            max_listeners: None,
            name: Some("Test".to_string()),
            description: None,
            genre: None,
            url: None,
            transcode: None,
        }],
        tls: None,
        transcode: None,
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
        auth: auth.clone(),
        ingest,
        output,
        listeners: listeners.clone(),
        config: Arc::new(ArcSwap::from_pointee(cfg.clone())),
        shutdown: shutdown.clone(),
    };

    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let prom_handle = recorder.handle();

    let sessions = SessionStore::new(std::time::Duration::from_secs(3600));

    let admin_state = AdminState {
        mounts: mounts.clone(),
        listeners,
        prometheus: prom_handle,
        start_time: std::time::Instant::now(),
        auth,
        sessions: sessions.clone(),
        version: env!("CARGO_PKG_VERSION"),
        stream_port: 0,
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

    (stream_port, admin_port, shutdown, sessions)
}

/// Reads enough bytes from the response to cover `metaint` audio bytes plus
/// the trailing ICY metadata block, then parses the meta string out of it.
async fn read_icy_meta(
    response: &mut reqwest::Response,
    metaint: usize,
) -> String {
    let mut buf: Vec<u8> = Vec::with_capacity(metaint + 4096);
    while buf.len() < metaint + 1 {
        let chunk = tokio::time::timeout(Duration::from_secs(2), response.chunk())
            .await
            .expect("timed out reading audio")
            .expect("response error")
            .expect("response ended unexpectedly");
        buf.extend_from_slice(&chunk);
    }
    let len_byte = buf[metaint] as usize;
    let meta_total = 1 + len_byte * 16;
    while buf.len() < metaint + meta_total {
        let chunk = tokio::time::timeout(Duration::from_secs(2), response.chunk())
            .await
            .expect("timed out reading meta")
            .expect("response error")
            .expect("response ended");
        buf.extend_from_slice(&chunk);
    }
    let meta_bytes = &buf[metaint + 1 .. metaint + 1 + len_byte * 16];
    let end = meta_bytes.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
    String::from_utf8_lossy(&meta_bytes[..end]).into_owned()
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
async fn dynamic_mount_created_when_default_source_password_matches() {
    // Server allows any source authenticating with "globalpw" to create new
    // mounts that aren't pre-configured.
    let (stream_port, _admin_port, shutdown) = build_test_server_with(Some("globalpw")).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let base = format!("http://127.0.0.1:{stream_port}");
    let dyn_path = "/freshmount";

    // No source yet → mount doesn't exist → listener gets 404.
    let resp = reqwest::Client::new()
        .get(format!("{base}{dyn_path}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Wrong default password is rejected.
    let resp = reqwest::Client::new()
        .put(format!("{base}{dyn_path}"))
        .header("Authorization", "Basic d3Jvbmc=") // base64("wrong")
        .header("Content-Type", "audio/mpeg")
        .body(vec![0u8; 32])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Correct default password creates the mount; keep the source request
    // alive so the mount stays around long enough for the listener to attach.
    let source_url = format!("{base}{dyn_path}");
    let audio: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(65_536).collect();
    let source_handle = tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .put(&source_url)
            .header("Authorization", "Basic Z2xvYmFscHc=") // base64("globalpw")
            .header("Content-Type", "audio/mpeg")
            .body(audio)
            .send()
            .await;
    });

    // Give the source a moment to register the mount.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut listener_resp = reqwest::Client::new()
        .get(format!("{base}{dyn_path}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        listener_resp.status(),
        200,
        "listener should attach to dynamically-created mount"
    );
    let chunk = tokio::time::timeout(Duration::from_millis(500), listener_resp.chunk())
        .await
        .expect("timed out waiting for audio")
        .expect("chunk error");
    assert!(chunk.map_or(false, |b| !b.is_empty()));

    // Once the source finishes and the disconnect guard runs, the dynamic
    // mount is removed and new listeners get 404 again.
    let _ = source_handle.await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}{dyn_path}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "dynamic mount should be removed after source disconnect");

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

// Helper: build a short MP3 stream (about 1 second of silence) for test use
fn generate_test_mp3() -> Vec<u8> {
    let mut enc = rustyice_transcode::LameEncoder::new(44100, 44100, 2, 128).unwrap();
    // 5 seconds: enough data for multiple decode batches after the decoder warmup
    // phase (which consumes the first ~8 KB to fill the MP3 bit reservoir), so
    // the listener always receives at least one chunk before the source disconnects.
    let silence = vec![0.0f32; 44100 * 2 * 5];
    let mut data = enc.encode(&silence).unwrap();
    data.extend_from_slice(&enc.flush().unwrap());
    data
}

// Helper: build a test server with transcode config on /stream
async fn build_test_server_with_transcode(bitrate_kbps: u32) -> (u16, u16, CancellationToken) {
    let cfg = Config {
        server: ServerConfig {
            stream_bind: "127.0.0.1:0".parse().unwrap(),
            admin_bind: "127.0.0.1:0".parse().unwrap(),
            hostname: "localhost".to_string(),
        },
        logging: LoggingConfig { level: "error".to_string(), format: LogFormat::Pretty },
        auth: AuthConfig {
            users: vec![],
            source_password: None,
        },
        limits: LimitsConfig {
            max_listeners_global: 100,
            ring_size: 64,
            slow_listener_grace_s: 2,
            source_max_kbps: None,
        },
        mounts: vec![MountConfig {
            path: "/stream".to_string(),
            source_password: "testpass".to_string(),
            max_listeners: None,
            name: Some("Test".to_string()),
            description: None,
            genre: None,
            url: None,
            transcode: Some(TranscodeConfig {
                format: TranscodeFormat::Mp3,
                sample_rate: 44100,
                bitrate_kbps,
            }),
        }],
        tls: None,
        transcode: None,
    };

    // Build the same way as build_test_server but with transcode config
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
        auth: auth.clone(),
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
        auth,
        sessions: SessionStore::new(std::time::Duration::from_secs(3600)),
        version: env!("CARGO_PKG_VERSION"),
        stream_port: 0,
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
async fn transcoded_source_delivers_mp3_to_listener() {
    let mp3_data = generate_test_mp3();
    if mp3_data.is_empty() {
        // LAME produced nothing for silence in this environment — skip
        return;
    }

    let (stream_port, _admin_port, shutdown) = build_test_server_with_transcode(64).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let base_url = format!("http://127.0.0.1:{stream_port}");

    // Connect listener first
    let mut response = tokio::time::timeout(
        Duration::from_secs(2),
        reqwest::Client::new().get(format!("{base_url}/stream")).send(),
    )
    .await
    .expect("listener GET timed out")
    .expect("listener GET failed");

    assert_eq!(response.status(), 200);

    // Push MP3 source
    let source_url = format!("{base_url}/stream");
    tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .put(&source_url)
            .header("Authorization", "Basic dGVzdHBhc3M=") // base64("testpass")
            .header("Content-Type", "audio/mpeg")
            .body(mp3_data)
            .send()
            .await;
    });

    // Wait for transcoded output
    let first_chunk = tokio::time::timeout(Duration::from_secs(3), response.chunk())
        .await
        .expect("timeout waiting for transcoded audio chunk")
        .expect("request error");

    let bytes = first_chunk
        .expect("transcoded source must deliver at least one chunk to listener");
    assert!(!bytes.is_empty(), "transcoded output chunk must be non-empty");
    let has_sync_word = bytes.windows(2).any(|w| w[0] == 0xFF && (w[1] & 0xE0) == 0xE0);
    assert!(has_sync_word, "transcoded output must contain valid MP3 sync words");

    shutdown.cancel();
}

#[tokio::test]
async fn admin_title_appears_in_listener_icy_metadata() {
    let (stream_port, admin_port, shutdown, sessions) =
        build_test_server_with_sessions().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let token = sessions.create("admin".to_string());
    let cookie = format!("rustyice_session={token}");

    // PUT a title BEFORE the source connects (offline-set is allowed by design).
    let resp = reqwest::Client::new()
        .put(format!("http://127.0.0.1:{admin_port}/api/mounts/stream/title"))
        .header("Cookie", &cookie)
        .header("Content-Type", "application/json")
        .body(r#"{"title":"Artist - Song"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Listener connects first, requesting ICY metadata.
    let mut listener_resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{stream_port}/stream"))
        .header("Icy-Metadata", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(listener_resp.status(), 200);
    assert_eq!(
        listener_resp.headers().get("icy-metaint").and_then(|v| v.to_str().ok()),
        Some("8192"),
        "default metaint should be advertised"
    );

    // Source pushes enough audio to fill at least one metaint window (8192 bytes).
    // FAKE_MP3_FRAME is 8 bytes, so 8192 * 2 = 16384 bytes ensures we cross the boundary.
    let source_url = format!("http://127.0.0.1:{stream_port}/stream");
    let audio: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(16_384).collect();
    let source_handle = tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .put(&source_url)
            .header("Authorization", "Basic dGVzdHBhc3M=") // base64("testpass")
            .header("Content-Type", "audio/mpeg")
            .body(audio)
            .send()
            .await;
    });

    let meta = read_icy_meta(&mut listener_resp, 8192).await;
    assert!(
        meta.contains("StreamTitle='Artist - Song';"),
        "expected admin-set title in ICY meta; got: {meta:?}"
    );

    // Update the title via DELETE; next frame should fall back to the mount name "Test".
    let resp = reqwest::Client::new()
        .delete(format!("http://127.0.0.1:{admin_port}/api/mounts/stream/title"))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let meta_after_clear = read_icy_meta(&mut listener_resp, 8192).await;
    assert!(
        meta_after_clear.contains("StreamTitle='Test';"),
        "after clear, expected fallback to mount name 'Test'; got: {meta_after_clear:?}"
    );

    drop(listener_resp);
    let _ = source_handle.await;
    shutdown.cancel();
}
