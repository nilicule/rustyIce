#![warn(clippy::pedantic)]

use rustyice_server::{
    bus::TokioBroadcastBus,
    config_reload::watch_sighup,
    shutdown::shutdown_signal,
    state::AppState,
    stream_listener::{PeerAddr, StreamListener},
    stream_router::build_stream_router,
};
use rustyice_server::banner;

use arc_swap::ArcSwap;
use rustyice_admin::{build_admin_router, ListenerMap};
use rustyice_auth::TomlBcryptAuth;
use rustyice_core::{
    config::{self, Config, TranscodeFormat},
    mount::{ActiveMount, MountInfo, MountMetadata, MountRegistry},
    types::CodecId,
};
use rustyice_ingest::IcecastIngest;
use rustyice_output::HttpPassthroughOutput;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

const DEFAULT_CONFIG_TOML: &str = include_str!("default_config.toml");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--print-config") {
        print!("{DEFAULT_CONFIG_TOML}");
        return Ok(());
    }

    let (cfg, config_source) = load_or_default_config()?;

    print_banner(cfg.server.admin_bind);

    setup_tracing(&cfg);

    let config_label: std::borrow::Cow<'_, str> = match &config_source {
        ConfigSource::File(path) => path.display().to_string().into(),
        ConfigSource::Defaults => "built-in defaults".into(),
    };
    info!(
        version = env!("CARGO_PKG_VERSION"),
        config  = %config_label,
        "rustyice starting"
    );

    // ── Prometheus ──────────────────────────────────────────────────────────
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let prom_handle = recorder.handle();
    metrics::set_global_recorder(recorder).expect("failed to set global metrics recorder");

    // ── Mount registry ──────────────────────────────────────────────────────
    let mounts = MountRegistry::new();
    for mc in &cfg.mounts {
        let bus = Arc::new(TokioBroadcastBus::new(
            cfg.limits.ring_size,
            cfg.effective_burst_size(mc) as usize,
        ));
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

    // ── AutoDJs ────────────────────────────────────────────────────────────
    let mut autodj_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    for ac in &cfg.autodjs {
        let bus = Arc::new(TokioBroadcastBus::new(
            cfg.limits.ring_size,
            ac.burst_size.unwrap_or(cfg.limits.burst_size) as usize,
        ));
        let codec_seed = match ac.transcode.format {
            TranscodeFormat::Mp3 => CodecId::MP3,
            TranscodeFormat::Vorbis => CodecId::VORBIS,
        };
        let mount = Arc::new(ActiveMount::new(
            MountInfo {
                path: ac.mount.clone(),
                codec: codec_seed,
                // No live ingest is allowed via password on autodj-only paths.
                // (Live sources still preempt via the autodj-slot mechanism.)
                source_password: String::new(),
                max_listeners: ac.max_listeners,
                metadata: MountMetadata {
                    name: ac.name.clone(),
                    description: ac.description.clone(),
                    genre: ac.genre.clone(),
                    url: ac.url.clone(),
                },
            },
            bus,
        ));
        mounts.add(mount.clone());
        info!(mount = %ac.mount, enabled = ac.enabled, "registered autodj mount");
        if ac.enabled {
            let player = rustyice_autodj::AutoDjPlayer::from_config(ac, mount, shutdown.clone());
            autodj_handles.push(player.spawn());
        }
    }
    let _autodj_handles = autodj_handles; // keep alive until main exits

    let listeners = ListenerMap::new();
    let shared_cfg = Arc::new(ArcSwap::from_pointee(cfg.clone()));
    let auth = Arc::new(TomlBcryptAuth::new(&cfg));
    let ingest: Arc<dyn rustyice_core::traits::IngestProtocol + Send + Sync> = {
        let mut i = IcecastIngest::default();
        if let Some(kbps) = cfg.limits.source_max_kbps {
            i = i.with_max_rate(u64::from(kbps) * 1000 / 8);
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
    let stream_router = build_stream_router(app_state.clone());

    let admin_state = app_state.admin_state(prom_handle);
    let admin_router = build_admin_router(admin_state);

    // ── Spawn SIGHUP watcher ────────────────────────────────────────────────
    // Only meaningful when a real config file backs the running config —
    // there's nothing to reload from when running on built-in defaults.
    if let ConfigSource::File(config_path) = &config_source {
        let sighup_shutdown = shutdown.clone();
        tokio::spawn(watch_sighup(
            config_path.clone(),
            shared_cfg,
            auth,
            mounts,
            sighup_shutdown,
        ));
    }

    // ── Spawn admin server ──────────────────────────────────────────────────
    let admin_shutdown = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(admin_listener, admin_router)
            .with_graceful_shutdown(async move { admin_shutdown.cancelled().await })
            .await
            .expect("admin server error");
    });

    // ── Run stream server (blocks until shutdown) ───────────────────────────
    // `StreamListener` intercepts Icecast SOURCE-protocol uploads (PUT/SOURCE
    // with `Expect: 100-continue` and no body framing) and routes them through
    // `source_protocol`, which speaks the protocol directly on the TCP socket.
    // Listener GETs and everything else flow through axum/hyper as usual.
    //
    // `into_make_service_with_connect_info::<PeerAddr>()` exposes each
    // listener's peer address to handlers via `ConnectInfo<PeerAddr>` —
    // a local newtype required by the orphan rule for our custom listener.
    // Used by the admin "listeners" view to show the client address.
    let stream_shutdown = shutdown.clone();
    let dispatcher = StreamListener::new(stream_listener, app_state);
    axum::serve(
        dispatcher,
        stream_router.into_make_service_with_connect_info::<PeerAddr>(),
    )
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

enum ConfigSource {
    File(PathBuf),
    Defaults,
}

struct GeneratedCreds {
    admin_user: String,
    admin_pass: String,
    source_pass: String,
}

/// Resolve the running config: explicit `--config <path>` wins; otherwise we
/// try `config.toml` next to the binary; otherwise we fall back to compiled-in
/// defaults with **freshly generated** admin and source passwords (announced
/// on the console).
fn load_or_default_config() -> Result<(Config, ConfigSource), Box<dyn std::error::Error>> {
    if let Some(path) = parse_config_arg() {
        return Ok((config::load(&path)?, ConfigSource::File(path)));
    }
    let default_path = PathBuf::from("config.toml");
    if default_path.exists() {
        return Ok((config::load(&default_path)?, ConfigSource::File(default_path)));
    }
    let mut cfg = config::parse_str(DEFAULT_CONFIG_TOML)?;
    let admin_pass = random_password(20)?;
    let source_pass = random_password(20)?;
    cfg.auth.source_password = Some(source_pass.clone());
    let admin_user = cfg
        .auth
        .users
        .first()
        .map_or_else(|| "admin".to_string(), |u| u.username.clone());
    let admin_bcrypt = bcrypt::hash(&admin_pass, bcrypt::DEFAULT_COST)?;
    if let Some(user) = cfg.auth.users.first_mut() {
        user.password_bcrypt = admin_bcrypt;
    }
    print_defaults_notice(&GeneratedCreds {
        admin_user,
        admin_pass,
        source_pass,
    });
    Ok((cfg, ConfigSource::Defaults))
}

/// Generate a `len`-character password using `/dev/urandom` and a 62-char
/// alphanumeric set. The modulo bias is tiny (256 / 62 → 8 of 62 chars are
/// ~6% over-represented) and irrelevant for ~119-bit dev defaults.
fn random_password(len: usize) -> std::io::Result<String> {
    use std::io::Read;
    const CHARSET: &[u8] =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut buf = vec![0u8; len];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf
        .iter()
        .map(|b| CHARSET[*b as usize % CHARSET.len()] as char)
        .collect())
}

/// Notify the user that no config was found and show the randomly-generated
/// credentials that will be in effect for this run. Always prints (even on
/// non-TTY) — losing these passwords means losing admin access, so the user
/// needs to see them whether stdout is a terminal or a log collector.
fn print_defaults_notice(creds: &GeneratedCreds) {
    use std::io::IsTerminal;
    let tty = std::io::stdout().is_terminal();
    let (yellow, red, dim, reset) = if tty && std::env::var_os("NO_COLOR").is_none() {
        (
            "\x1b[38;2;233;196;106m",
            "\x1b[38;2;231;111;81m",
            "\x1b[2m",
            "\x1b[0m",
        )
    } else {
        ("", "", "", "")
    };
    let GeneratedCreds {
        admin_user,
        admin_pass,
        source_pass,
    } = creds;
    println!(
        "{yellow}no --config supplied and no config.toml in cwd \
         — starting with built-in defaults and fresh random credentials.{reset}\n\
         {red}credentials valid for this run only (regenerated on every start):{reset}\n\
         {yellow}  admin user:      {reset}{admin_user}\n\
         {yellow}  admin password:  {reset}{admin_pass}\n\
         {yellow}  source password: {reset}{source_pass}\n\n\
         {dim}to pin credentials, write the template to a file and edit:\n  \
         rustyice --print-config > config.toml{reset}\n"
    );
}

/// Print a startup banner. Skipped when stdout is not a TTY (e.g. piped to a
/// log collector) so structured-log consumers aren't fed ASCII art. Honors
/// `NO_COLOR` per <https://no-color.org/>.
fn print_banner(admin_bind: SocketAddr) {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return;
    }
    let color = std::env::var_os("NO_COLOR").is_none();
    let (cyan, dim, reset) = if color {
        ("\x1b[38;2;74;144;226m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "")
    };

    let tagline = format!(
        "icecast-compatible streaming · pure rust · v{}",
        env!("CARGO_PKG_VERSION")
    );
    let admin_url = banner::admin_url(admin_bind);
    let admin_label = format!("admin: {admin_url}");

    let tagline_pad = " ".repeat(banner::center_pad(tagline.chars().count(), banner::LOGO_WIDTH));
    let admin_pad = " ".repeat(banner::center_pad(admin_label.chars().count(), banner::LOGO_WIDTH));

    let admin_rendered = if color {
        banner::hyperlink(&admin_url, &admin_label)
    } else {
        admin_label
    };

    println!("\n{cyan}{logo}{reset}", logo = banner::LOGO);
    println!("{tagline_pad}{dim}{tagline}{reset}");
    println!("{admin_pad}{cyan}{admin_rendered}{reset}\n");
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
