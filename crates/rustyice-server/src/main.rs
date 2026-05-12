#![warn(clippy::pedantic)]

use rustyice_server::{
    bus::TokioBroadcastBus,
    config_reload::watch_sighup,
    shutdown::shutdown_signal,
    source_layer::SourceMethodLayer,
    state::AppState,
    stream_router::build_stream_router,
};

use arc_swap::ArcSwap;
use rustyice_admin::{build_admin_router, ListenerMap};
use rustyice_auth::TomlBcryptAuth;
use rustyice_core::{
    config::{self, Config},
    mount::{ActiveMount, MountInfo, MountMetadata, MountRegistry},
    types::CodecId,
};
use rustyice_ingest::IcecastIngest;
use rustyice_output::HttpPassthroughOutput;
use std::{path::PathBuf, sync::Arc};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = parse_config_arg().unwrap_or_else(|| PathBuf::from("config.toml"));
    let cfg = config::load(&config_path)?;

    setup_tracing(&cfg);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config  = %config_path.display(),
        "rustyice starting"
    );

    // ── Prometheus ──────────────────────────────────────────────────────────
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let prom_handle = recorder.handle();
    metrics::set_global_recorder(recorder).expect("failed to set global metrics recorder");

    // ── Mount registry ──────────────────────────────────────────────────────
    let mounts = MountRegistry::new();
    for mc in &cfg.mounts {
        let bus = Arc::new(TokioBroadcastBus::new(cfg.limits.ring_size));
        mounts.add(Arc::new(ActiveMount::new(
            MountInfo {
                path: mc.path.clone(),
                codec: CodecId::MP3,
                source_password: mc.source_password.clone(),
                max_listeners: mc.max_listeners,
                metadata: MountMetadata {
                    name: mc.name.clone(),
                    description: mc.description.clone(),
                    genre: mc.genre.clone(),
                    url: mc.url.clone(),
                },
            },
            bus,
        )));
        info!(mount = %mc.path, "registered mount");
    }

    // ── Shared state ────────────────────────────────────────────────────────
    let shutdown = CancellationToken::new();
    let listeners = ListenerMap::new();
    let shared_cfg = Arc::new(ArcSwap::from_pointee(cfg.clone()));
    let auth = Arc::new(TomlBcryptAuth::new(&cfg));
    let ingest: Arc<dyn rustyice_core::traits::IngestProtocol + Send + Sync> = {
        let mut i = IcecastIngest::default();
        if let Some(kbps) = cfg.limits.source_max_kbps {
            i = i.with_max_rate(kbps as u64 * 1000 / 8);
        }
        Arc::new(i)
    };
    let output: Arc<dyn rustyice_core::traits::OutputProtocol + Send + Sync> =
        Arc::new(HttpPassthroughOutput::default());

    let app_state = AppState {
        mounts: mounts.clone(),
        auth: auth.clone(),
        ingest,
        output,
        listeners: listeners.clone(),
        config: shared_cfg.clone(),
        shutdown: shutdown.clone(),
    };

    // ── Bind ports ──────────────────────────────────────────────────────────
    let stream_listener = TcpListener::bind(cfg.server.stream_bind).await?;
    let admin_listener = TcpListener::bind(cfg.server.admin_bind).await?;

    info!(addr = %cfg.server.stream_bind, "stream port bound");
    info!(addr = %cfg.server.admin_bind, "admin port bound");

    // ── Build routers ───────────────────────────────────────────────────────
    let stream_router = build_stream_router(app_state.clone())
        .layer(ServiceBuilder::new().layer(SourceMethodLayer));

    let admin_state = app_state.admin_state(prom_handle);
    let admin_router = build_admin_router(admin_state);

    // ── Spawn SIGHUP watcher ────────────────────────────────────────────────
    let sighup_shutdown = shutdown.clone();
    tokio::spawn(watch_sighup(config_path, shared_cfg, auth, mounts, sighup_shutdown));

    // ── Spawn admin server ──────────────────────────────────────────────────
    let admin_shutdown = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(admin_listener, admin_router)
            .with_graceful_shutdown(async move { admin_shutdown.cancelled().await })
            .await
            .expect("admin server error");
    });

    // ── Run stream server (blocks until shutdown) ───────────────────────────
    let stream_shutdown = shutdown.clone();
    axum::serve(stream_listener, stream_router)
        .with_graceful_shutdown(async move {
            shutdown_signal(stream_shutdown.clone()).await;
        })
        .await?;

    info!("rustyice stopped");
    Ok(())
}

fn parse_config_arg() -> Option<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    let pos = args.iter().position(|a| a == "--config")?;
    args.get(pos + 1).map(PathBuf::from)
}

fn setup_tracing(cfg: &Config) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&cfg.logging.level));
    match cfg.logging.format {
        rustyice_core::config::LogFormat::Json => {
            fmt().json().with_env_filter(filter).init();
        }
        rustyice_core::config::LogFormat::Pretty => {
            fmt().with_env_filter(filter).init();
        }
    }
}
