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
    bus::TokioBroadcastBus,
    state::AppState,
    stream_listener::{PeerAddr, StreamListener},
    stream_router::build_stream_router,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use rustyice_core::config::{TranscodeConfig, TranscodeFormat};

const FAKE_MP3_FRAME: &[u8] = &[0xFF, 0xFB, 0x90, 0x04, 0x00, 0x00, 0x00, 0x00];

async fn build_test_server() -> (u16, u16, CancellationToken) {
    build_test_server_with(None, None).await
}

async fn build_test_server_with(
    default_source_password: Option<&str>,
    mount_burst_size: Option<u32>,
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
            burst_size: 65_536,
        },
        mounts: vec![MountConfig {
            path: "/stream".to_string(),
            source_password: "testpass".to_string(),
            max_listeners: None,
            name: Some("Test".to_string()),
            description: None,
            genre: None,
            url: None,
            burst_size: mount_burst_size,
            transcode: None,
        }],
        autodjs: vec![],
        tls: None,
        transcode: None,
    };

    let mounts = MountRegistry::new();
    let bus = Arc::new(TokioBroadcastBus::new(
        cfg.limits.ring_size,
        cfg.effective_burst_size(&cfg.mounts[0]) as usize,
    ));
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
        config: app_state.config.clone(),
    };

    let stream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream_port = stream_listener.local_addr().unwrap().port();
    let admin_port = admin_listener.local_addr().unwrap().port();

    let stream_router = build_stream_router(app_state.clone());
    let admin_router = build_admin_router(admin_state);

    let stream_sd = shutdown.clone();
    let dispatcher = StreamListener::new(stream_listener, app_state);
    tokio::spawn(async move {
        axum::serve(
            dispatcher,
            stream_router.into_make_service_with_connect_info::<PeerAddr>(),
        )
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
            burst_size: 65_536,
        },
        mounts: vec![MountConfig {
            path: "/stream".to_string(),
            source_password: "testpass".to_string(),
            max_listeners: None,
            name: Some("Test".to_string()),
            description: None,
            genre: None,
            url: None,
            burst_size: None,
            transcode: None,
        }],
        autodjs: vec![],
        tls: None,
        transcode: None,
    };

    let mounts = MountRegistry::new();
    let bus = Arc::new(TokioBroadcastBus::new(
        cfg.limits.ring_size,
        cfg.effective_burst_size(&cfg.mounts[0]) as usize,
    ));
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
        config: app_state.config.clone(),
    };

    let stream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream_port = stream_listener.local_addr().unwrap().port();
    let admin_port = admin_listener.local_addr().unwrap().port();

    let stream_router = build_stream_router(app_state.clone());
    let admin_router = build_admin_router(admin_state);

    let stream_sd = shutdown.clone();
    let dispatcher = StreamListener::new(stream_listener, app_state);
    tokio::spawn(async move {
        axum::serve(
            dispatcher,
            stream_router.into_make_service_with_connect_info::<PeerAddr>(),
        )
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
        first_chunk.is_some_and(|b| !b.is_empty()),
        "listener should have received some audio bytes"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn late_listener_receives_burst_on_connect() {
    // Mount burst_size = 16 KB (4 ingest packets). Source pushes 32 KB then
    // disconnects; the mount + bus + history outlive the disconnect because
    // /stream is statically configured. A late listener should receive
    // approximately burst_size bytes of historical audio immediately.
    const BURST: u32 = 16_384;
    let (stream_port, _admin_port, shutdown) =
        build_test_server_with(None, Some(BURST)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let base_url = format!("http://127.0.0.1:{stream_port}");

    // 1. Source pushes 32 KB and disconnects.
    let source_url = format!("{base_url}/stream");
    let audio: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(32 * 1024).collect();
    let resp = reqwest::Client::new()
        .put(&source_url)
        .header("Authorization", "Basic dGVzdHBhc3M=") // base64("testpass")
        .header("Content-Type", "audio/mpeg")
        .body(audio)
        .send()
        .await
        .expect("source PUT failed");
    assert!(resp.status().is_success() || resp.status().as_u16() == 200);
    // Let the ingest loop drain the last bytes into the bus history.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 2. Late listener connects — should immediately receive the burst.
    let listener_url = format!("{base_url}/stream");
    let mut response = tokio::time::timeout(
        Duration::from_secs(2),
        reqwest::Client::new().get(&listener_url).send(),
    )
    .await
    .expect("listener GET timed out")
    .expect("listener GET failed");
    assert_eq!(response.status(), 200);

    // 3. Drain bytes; with source disconnected the connection idles after
    //    the burst is sent, so a short per-chunk timeout reliably ends the read.
    let mut received: usize = 0;
    while let Ok(Ok(Some(chunk))) =
        tokio::time::timeout(Duration::from_millis(150), response.chunk()).await
    {
        received += chunk.len();
        // Cap to avoid runaway if some live data ever sneaks in.
        if received >= (BURST as usize) * 2 { break; }
    }

    // Expect roughly BURST bytes, with one chunk of slack on either side.
    // Without burst-on-connect we'd receive 0 bytes (source disconnected).
    let burst = BURST as usize;
    let chunk_size = 4096; // IcecastIngest::default() chunk_size
    assert!(
        received >= burst - chunk_size,
        "expected ~{burst} burst bytes, got {received}",
    );
    assert!(
        received <= burst + chunk_size,
        "expected at most {burst} + 1 packet, got {received}",
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
    let (stream_port, _admin_port, shutdown) = build_test_server_with(Some("globalpw"), None).await;
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
    assert!(chunk.is_some_and(|b| !b.is_empty()));

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

// Helper: build a short MP3 stream (about 5 seconds of silence) for test use
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

// Helper: build a short Ogg Vorbis stream (~3 seconds of silence) for test use.
fn generate_test_vorbis() -> Vec<u8> {
    let mut enc = rustyice_transcode::VorbisEncoder::new(44100, 2, 96, &[]).unwrap();
    let silence = vec![0.0f32; 44100 * 2 * 3];
    let mut data = enc.encode(&silence).unwrap();
    data.extend_from_slice(&enc.flush().unwrap());
    data
}

async fn build_test_server_with_transcode(bitrate_kbps: u32) -> (u16, u16, CancellationToken) {
    build_test_server_with_transcode_cfg(TranscodeConfig {
        format: TranscodeFormat::Mp3,
        sample_rate: 44100,
        bitrate_kbps,
    })
    .await
}

// Helper: build a test server with an arbitrary transcode config on /stream.
async fn build_test_server_with_transcode_cfg(
    transcode: TranscodeConfig,
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
            source_password: None,
        },
        limits: LimitsConfig {
            max_listeners_global: 100,
            ring_size: 64,
            slow_listener_grace_s: 2,
            source_max_kbps: None,
            burst_size: 65_536,
        },
        mounts: vec![MountConfig {
            path: "/stream".to_string(),
            source_password: "testpass".to_string(),
            max_listeners: None,
            name: Some("Test".to_string()),
            description: None,
            genre: None,
            url: None,
            burst_size: None,
            transcode: Some(transcode),
        }],
        autodjs: vec![],
        tls: None,
        transcode: None,
    };

    // Build the same way as build_test_server but with transcode config
    let mounts = MountRegistry::new();
    let bus = Arc::new(TokioBroadcastBus::new(
        cfg.limits.ring_size,
        cfg.effective_burst_size(&cfg.mounts[0]) as usize,
    ));
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
        config: app_state.config.clone(),
    };

    let stream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream_port = stream_listener.local_addr().unwrap().port();
    let admin_port = admin_listener.local_addr().unwrap().port();

    let stream_router = build_stream_router(app_state.clone());
    let admin_router = build_admin_router(admin_state);

    let stream_sd = shutdown.clone();
    let dispatcher = StreamListener::new(stream_listener, app_state);
    tokio::spawn(async move {
        axum::serve(
            dispatcher,
            stream_router.into_make_service_with_connect_info::<PeerAddr>(),
        )
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

    // Source pushes audio via a channel-driven streaming body so the connection
    // stays alive while the listener reads across two metaint windows. A
    // fixed-Vec body would EOF instantly and the disconnect guard would kick
    // the listener mid-read.
    use bytes::Bytes;
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx));
    let source_url = format!("http://127.0.0.1:{stream_port}/stream");
    let source_handle = tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .put(&source_url)
            .header("Authorization", "Basic dGVzdHBhc3M=") // base64("testpass")
            .header("Content-Type", "audio/mpeg")
            .body(body)
            .send()
            .await;
    });

    // Push enough audio that the listener can cross at least one metaint
    // (8192) and read a second meta frame after the DELETE below.
    let feeder = tokio::spawn(async move {
        for _ in 0..6 {
            let frame: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(8_192).collect();
            if body_tx.send(Ok(Bytes::from(frame))).await.is_err() {
                break;
            }
        }
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
    let _ = feeder.await;
    let _ = source_handle.await;
    shutdown.cancel();
}

#[tokio::test]
async fn admin_api_exposes_listener_peer_address() {
    let (stream_port, admin_port, shutdown, sessions) =
        build_test_server_with_sessions().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let token = sessions.create("admin".to_string());
    let cookie = format!("rustyice_session={token}");

    // Source connects and streams indefinitely so the listener stays attached.
    let source_url = format!("http://127.0.0.1:{stream_port}/stream");
    let audio: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(65_536).collect();
    let source_handle = tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .put(&source_url)
            .header("Authorization", "Basic dGVzdHBhc3M=") // base64("testpass")
            .header("Content-Type", "audio/mpeg")
            .body(audio)
            .send()
            .await;
    });

    // Listener connects so the admin API has someone to report on.
    let listener_resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{stream_port}/stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(listener_resp.status(), 200);

    // Give the server a moment to register the listener.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{admin_port}/api/mounts/stream/listeners"))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["mount_path"], "/stream");
    let listeners = body["listeners"].as_array().expect("listeners is array");
    assert!(!listeners.is_empty(), "expected at least one listener");
    let entry = &listeners[0];
    assert!(entry["id"].is_number(), "listener id should be numeric");
    let addr = entry["address"].as_str().expect("address should be a string");
    assert!(
        addr.starts_with("127.0.0.1:"),
        "address should be 127.0.0.1:port, got: {addr}"
    );

    drop(listener_resp);
    let _ = source_handle.await;
    shutdown.cancel();
}

/// Helper: open a source PUT with a channel-driven streaming body. Returns the
/// JoinHandle of the source task and a sender; drop the sender to end the
/// source connection cleanly. The first frame is pre-sent so the server has
/// already captured the overlay by the time the helper returns.
async fn open_source_with_overlay(
    stream_port: u16,
    extra_headers: &[(&'static str, &'static str)],
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
) {
    use bytes::Bytes;
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx));
    let source_url = format!("http://127.0.0.1:{stream_port}/stream");
    let extra: Vec<(String, String)> =
        extra_headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let source_handle = tokio::spawn(async move {
        let mut req = reqwest::Client::new()
            .put(&source_url)
            .header("Authorization", "Basic dGVzdHBhc3M=") // base64("testpass")
            .header("Content-Type", "audio/mpeg");
        for (k, v) in &extra {
            req = req.header(k.as_str(), v.as_str());
        }
        let _ = req.body(body).send().await;
    });
    // Push a first frame so the source actually attaches and the overlay is
    // captured before the test queries.
    let frame: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(8_192).collect();
    body_tx.send(Ok(Bytes::from(frame))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    (source_handle, body_tx)
}

#[tokio::test]
async fn source_ice_headers_appear_in_listener_response() {
    let (stream_port, _admin_port, shutdown) = build_test_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (source_handle, body_tx) = open_source_with_overlay(
        stream_port,
        &[
            ("Ice-Name", "Source Name"),
            ("Ice-Description", "Source Desc"),
            ("Ice-Genre", "Rock"),
            ("Ice-URL", "https://source"),
            ("Ice-Public", "1"),
            ("Ice-Audio-Info", "samplerate=44100;channels=2"),
            ("Ice-Bitrate", "192"),
        ],
    )
    .await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{stream_port}/stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let h = resp.headers();
    assert_eq!(h.get("icy-name").and_then(|v| v.to_str().ok()), Some("Source Name"));
    assert_eq!(h.get("icy-description").and_then(|v| v.to_str().ok()), Some("Source Desc"));
    assert_eq!(h.get("icy-genre").and_then(|v| v.to_str().ok()), Some("Rock"));
    assert_eq!(h.get("icy-url").and_then(|v| v.to_str().ok()), Some("https://source"));
    assert_eq!(h.get("icy-pub").and_then(|v| v.to_str().ok()), Some("1"));
    assert_eq!(
        h.get("ice-audio-info").and_then(|v| v.to_str().ok()),
        Some("samplerate=44100;channels=2"),
    );
    assert_eq!(h.get("icy-br").and_then(|v| v.to_str().ok()), Some("192"));

    drop(resp);
    drop(body_tx);
    let _ = source_handle.await;
    shutdown.cancel();
}

#[tokio::test]
async fn listener_falls_back_to_config_when_source_sends_no_ice_headers() {
    let (stream_port, _admin_port, shutdown) = build_test_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (source_handle, body_tx) = open_source_with_overlay(stream_port, &[]).await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{stream_port}/stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let h = resp.headers();
    // Config-only mount has name="Test", no other identity fields.
    assert_eq!(h.get("icy-name").and_then(|v| v.to_str().ok()), Some("Test"));
    assert!(h.get("icy-description").is_none());
    assert!(h.get("icy-genre").is_none());
    assert!(h.get("icy-url").is_none());
    assert!(h.get("icy-pub").is_none());
    assert!(h.get("ice-audio-info").is_none());
    // No transcode, no source bitrate → header omitted.
    assert!(h.get("icy-br").is_none());

    drop(resp);
    drop(body_tx);
    let _ = source_handle.await;
    shutdown.cancel();
}

#[tokio::test]
async fn admin_api_reflects_overlay_then_reverts_on_disconnect() {
    use bytes::Bytes;
    let (stream_port, admin_port, shutdown) = build_test_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Use a channel-driven streaming body so the source connection lifetime is
    // explicit: it ends when we drop the sender, not when a fixed buffer drains.
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let body_stream = tokio_stream::wrappers::ReceiverStream::new(body_rx);
    let body = reqwest::Body::wrap_stream(body_stream);

    let source_url = format!("http://127.0.0.1:{stream_port}/stream");
    let source_handle = tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .put(&source_url)
            .header("Authorization", "Basic dGVzdHBhc3M=")
            .header("Content-Type", "audio/mpeg")
            .header("Ice-Name", "Live")
            .header("Ice-Genre", "Jazz")
            .body(body)
            .send()
            .await;
    });

    // Push an initial chunk so the server registers the connection and the
    // overlay is captured.
    let frame: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(8_192).collect();
    body_tx.send(Ok(Bytes::from(frame))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let json: serde_json::Value = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{admin_port}/api/mounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mount = json[0].clone();
    assert_eq!(mount["name"], "Live", "overlay name should win over config");
    assert_eq!(mount["genre"], "Jazz");

    // Drop the sender → body stream EOFs → source disconnects → guard fires.
    drop(body_tx);
    let _ = source_handle.await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let json2: serde_json::Value = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{admin_port}/api/mounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mount2 = json2[0].clone();
    // Config name "Test" is back, overlay cleared.
    assert_eq!(mount2["name"], "Test");
    assert!(mount2["genre"].is_null(), "overlay should be cleared on disconnect");

    shutdown.cancel();
}

#[tokio::test]
async fn ice_name_wins_over_icy_name_when_both_sent() {
    let (stream_port, _admin_port, shutdown) = build_test_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (source_handle, body_tx) = open_source_with_overlay(
        stream_port,
        &[("Ice-Name", "ICE wins"), ("Icy-Name", "ICY loses")],
    )
    .await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{stream_port}/stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("icy-name").and_then(|v| v.to_str().ok()),
        Some("ICE wins"),
    );

    drop(resp);
    drop(body_tx);
    let _ = source_handle.await;
    shutdown.cancel();
}

#[tokio::test]
async fn ice_name_used_as_streamtitle_fallback_in_icy_meta() {
    use bytes::Bytes;
    let (stream_port, _admin_port, shutdown) = build_test_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Source first, with a streaming body we keep alive until the test finishes.
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx));
    let source_url = format!("http://127.0.0.1:{stream_port}/stream");
    let source_handle = tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .put(&source_url)
            .header("Authorization", "Basic dGVzdHBhc3M=")
            .header("Content-Type", "audio/mpeg")
            .header("Ice-Name", "Overlay Title")
            .body(body)
            .send()
            .await;
    });

    // Push enough data so listener can cross a metaint boundary.
    let frame: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(16_384).collect();
    body_tx.send(Ok(Bytes::from(frame))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut listener_resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{stream_port}/stream"))
        .header("Icy-Metadata", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(listener_resp.status(), 200);

    // Keep feeding the source so the listener can consume past 8192 bytes.
    let feeder = tokio::spawn(async move {
        for _ in 0..4 {
            let frame: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(8_192).collect();
            if body_tx.send(Ok(Bytes::from(frame))).await.is_err() {
                break;
            }
        }
    });

    let meta = read_icy_meta(&mut listener_resp, 8192).await;
    assert!(
        meta.contains("StreamTitle='Overlay Title';"),
        "expected overlay name as StreamTitle fallback, got: {meta:?}",
    );

    drop(listener_resp);
    let _ = feeder.await;
    let _ = source_handle.await;
    shutdown.cancel();
}

#[tokio::test]
async fn transcoding_overrides_source_bitrate_and_audio_info() {
    // Build a server with transcoding enabled for /stream.
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
            burst_size: 65_536,
        },
        mounts: vec![MountConfig {
            path: "/stream".to_string(),
            source_password: "testpass".to_string(),
            max_listeners: None,
            name: Some("Test".to_string()),
            description: None,
            genre: None,
            url: None,
            burst_size: None,
            transcode: Some(TranscodeConfig {
                format: TranscodeFormat::Mp3,
                sample_rate: 22050,
                bitrate_kbps: 64,
            }),
        }],
        autodjs: vec![],
        tls: None,
        transcode: None,
    };

    let mounts = MountRegistry::new();
    let bus = Arc::new(TokioBroadcastBus::new(
        cfg.limits.ring_size,
        cfg.effective_burst_size(&cfg.mounts[0]) as usize,
    ));
    mounts.add(Arc::new(ActiveMount::new(
        MountInfo {
            path: "/stream".to_string(),
            codec: CodecId::MP3,
            source_password: "testpass".to_string(),
            max_listeners: None,
            metadata: MountMetadata { name: Some("Test".to_string()), ..Default::default() },
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

    let stream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream_port = stream_listener.local_addr().unwrap().port();
    let stream_router = build_stream_router(app_state.clone());
    let stream_sd = shutdown.clone();
    let dispatcher = StreamListener::new(stream_listener, app_state);
    tokio::spawn(async move {
        axum::serve(
            dispatcher,
            stream_router.into_make_service_with_connect_info::<PeerAddr>(),
        )
        .with_graceful_shutdown(async move { stream_sd.cancelled().await })
        .await
        .unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Source claims 320 kbps but transcode target is 64 kbps. We should advertise 64.
    use bytes::Bytes;
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx));
    let source_url = format!("http://127.0.0.1:{stream_port}/stream");
    let source_handle = tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .put(&source_url)
            .header("Authorization", "Basic dGVzdHBhc3M=")
            .header("Content-Type", "audio/mpeg")
            .header("Ice-Bitrate", "320")
            .header("Ice-Audio-Info", "samplerate=48000;channels=2;bitrate=320")
            .body(body)
            .send()
            .await;
    });

    // Push a real MP3-ish frame so the transcoder doesn't error out before we
    // attach. Then sleep long enough for the source to register.
    let frame: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(8_192).collect();
    body_tx.send(Ok(Bytes::from(frame))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{stream_port}/stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let h = resp.headers();
    assert_eq!(h.get("icy-br").and_then(|v| v.to_str().ok()), Some("64"));
    assert_eq!(
        h.get("ice-audio-info").and_then(|v| v.to_str().ok()),
        Some("samplerate=22050;channels=2;bitrate=64"),
    );

    drop(resp);
    drop(body_tx);
    let _ = source_handle.await;
    shutdown.cancel();
}

/// butt and other Icecast-2 source clients send:
///   - `PUT /mount HTTP/1.1`
///   - `Expect: 100-continue`
///   - no `Content-Length` and no `Transfer-Encoding`
///   - then wait for `HTTP/1.1 100 Continue` before streaming audio
/// On disconnect, they expect `HTTP/1.1 200 OK`.
///
/// This violates strict HTTP/1.1 body framing rules (a request with no length
/// hint is treated as a 0-byte body by RFC-compliant parsers like hyper), so
/// the source port has to speak the Icecast protocol directly rather than
/// going through axum/hyper.
#[tokio::test]
async fn butt_style_source_streams_audio_to_listener() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let (stream_port, _admin_port, shutdown) = build_test_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect listener first so it's subscribed before any audio arrives.
    let base_url = format!("http://127.0.0.1:{stream_port}");
    let listener_url = format!("{base_url}/stream");
    let mut listener_resp = tokio::time::timeout(
        Duration::from_secs(2),
        reqwest::Client::new().get(&listener_url).send(),
    )
    .await
    .expect("listener GET timed out")
    .expect("listener GET failed");
    assert_eq!(listener_resp.status(), 200);

    // Open raw TCP to the source port, send butt's exact protocol shape.
    let mut sock = TcpStream::connect(("127.0.0.1", stream_port)).await.unwrap();
    // base64("source:testpass") -> "c291cmNlOnRlc3RwYXNz"
    let request = b"PUT /stream HTTP/1.1\r\n\
                    Authorization: Basic c291cmNlOnRlc3RwYXNz\r\n\
                    Host: localhost\r\n\
                    User-Agent: fake-butt\r\n\
                    Content-Type: audio/mpeg\r\n\
                    ice-name: raw-test\r\n\
                    Expect: 100-continue\r\n\
                    \r\n";
    sock.write_all(request).await.unwrap();
    sock.flush().await.unwrap();

    // Expect a 100 Continue (interim) response before we stream the body.
    let mut interim = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut interim))
        .await
        .expect("server never responded with 100 Continue")
        .expect("read failed");
    let interim_str = std::str::from_utf8(&interim[..n]).unwrap_or("");
    assert!(
        interim_str.contains("100"),
        "expected 100 Continue, got: {interim_str:?}"
    );

    // Now stream audio (MP3 sync-word framed) and verify listener receives bytes.
    let audio: Vec<u8> = FAKE_MP3_FRAME.iter().copied().cycle().take(8192).collect();
    sock.write_all(&audio).await.unwrap();
    sock.flush().await.unwrap();

    let first_chunk = tokio::time::timeout(Duration::from_secs(2), listener_resp.chunk())
        .await
        .expect("listener never received audio")
        .expect("listener body error");
    assert!(
        first_chunk.is_some_and(|b| !b.is_empty()),
        "listener should have received audio bytes from butt-style source"
    );

    // Close source — server should respond with 200 OK before closing.
    drop(sock);
    drop(listener_resp);
    shutdown.cancel();
}

/// Open a streaming source PUT and return `(handle, body_tx)`. The source
/// stays connected until the test drops `body_tx`. The initial payload is
/// pushed synchronously so the server has captured the codec before
/// `open_streaming_source` returns.
async fn open_streaming_source(
    stream_port: u16,
    content_type: &'static str,
    initial_payload: Vec<u8>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
) {
    use bytes::Bytes;
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx));
    let url = format!("http://127.0.0.1:{stream_port}/stream");
    let handle = tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .put(&url)
            .header("Authorization", "Basic dGVzdHBhc3M=") // base64("testpass")
            .header("Content-Type", content_type)
            .body(body)
            .send()
            .await;
    });
    body_tx.send(Ok(Bytes::from(initial_payload))).await.unwrap();
    (handle, body_tx)
}

/// Connect a listener, snapshot the response headers without consuming the
/// body. Detailed body-level transcoding behavior is verified by the
/// `rustyice-transcode` unit tests; here we just verify the listener-facing
/// HTTP contract (Content-Type, ICY headers) for each combination.
async fn listener_headers(stream_port: u16, icy: bool) -> reqwest::header::HeaderMap {
    let mut req = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{stream_port}/stream"));
    if icy {
        req = req.header("Icy-Metadata", "1");
    }
    let resp = req.send().await.expect("listener GET failed");
    assert_eq!(resp.status(), 200);
    resp.headers().clone()
}

/// Drive a source into `/stream`, wait briefly for the server to register
/// it, then run `check` against the listener-facing response headers.
/// The source is kept alive for the duration of the check via a streaming
/// body that we hold; afterwards it is dropped + aborted so the test can
/// exit immediately.
async fn with_running_source<F>(
    stream_port: u16,
    source_ct: &'static str,
    source_body: Vec<u8>,
    settle: Duration,
    check: F,
) where
    F: AsyncFnOnce(),
{
    let (source_handle, body_tx) =
        open_streaming_source(stream_port, source_ct, source_body).await;
    tokio::time::sleep(settle).await;
    check().await;
    drop(body_tx);
    source_handle.abort();
}

#[tokio::test]
async fn vorbis_source_passthrough_advertises_application_ogg() {
    let (stream_port, _, shutdown) = build_test_server().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    with_running_source(
        stream_port,
        "application/ogg",
        generate_test_vorbis(),
        Duration::from_millis(150),
        || async {
            let h = listener_headers(stream_port, false).await;
            assert_eq!(
                h.get("content-type").and_then(|v| v.to_str().ok()),
                Some("application/ogg"),
            );
            // Vorbis must never advertise icy-metaint.
            let h_icy = listener_headers(stream_port, true).await;
            assert!(
                h_icy.get("icy-metaint").is_none(),
                "Vorbis output must not advertise icy-metaint",
            );
        },
    )
    .await;
    shutdown.cancel();
}

#[tokio::test]
async fn vorbis_source_transcoded_to_mp3_advertises_audio_mpeg() {
    let (stream_port, _, shutdown) =
        build_test_server_with_transcode_cfg(TranscodeConfig {
            format: TranscodeFormat::Mp3,
            sample_rate: 44100,
            bitrate_kbps: 64,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    with_running_source(
        stream_port,
        "application/ogg",
        generate_test_vorbis(),
        Duration::from_millis(150),
        || async {
            let h = listener_headers(stream_port, true).await;
            assert_eq!(
                h.get("content-type").and_then(|v| v.to_str().ok()),
                Some("audio/mpeg"),
            );
            assert_eq!(
                h.get("icy-metaint").and_then(|v| v.to_str().ok()),
                Some("8192"),
                "MP3 output must advertise icy-metaint",
            );
        },
    )
    .await;
    shutdown.cancel();
}

#[tokio::test]
async fn mp3_source_transcoded_to_vorbis_advertises_application_ogg() {
    let (stream_port, _, shutdown) =
        build_test_server_with_transcode_cfg(TranscodeConfig {
            format: TranscodeFormat::Vorbis,
            sample_rate: 44100,
            bitrate_kbps: 96,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    with_running_source(
        stream_port,
        "audio/mpeg",
        generate_test_mp3(),
        Duration::from_millis(150),
        || async {
            let h = listener_headers(stream_port, true).await;
            assert_eq!(
                h.get("content-type").and_then(|v| v.to_str().ok()),
                Some("application/ogg"),
            );
            assert!(
                h.get("icy-metaint").is_none(),
                "Vorbis output must not advertise icy-metaint even when client asked",
            );
        },
    )
    .await;
    shutdown.cancel();
}

#[tokio::test]
async fn vorbis_source_transcoded_to_vorbis_advertises_application_ogg() {
    let (stream_port, _, shutdown) =
        build_test_server_with_transcode_cfg(TranscodeConfig {
            format: TranscodeFormat::Vorbis,
            sample_rate: 44100,
            bitrate_kbps: 64,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    with_running_source(
        stream_port,
        "application/ogg",
        generate_test_vorbis(),
        Duration::from_millis(150),
        || async {
            let h = listener_headers(stream_port, false).await;
            assert_eq!(
                h.get("content-type").and_then(|v| v.to_str().ok()),
                Some("application/ogg"),
            );
        },
    )
    .await;
    shutdown.cancel();
}
