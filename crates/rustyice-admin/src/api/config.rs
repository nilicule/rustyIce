use crate::state::AdminState;
use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use rustyice_core::config::{
    AuthConfig, AutoDjConfig, Config, LimitsConfig, LoggingConfig, MountConfig, RelayConfig,
    ServerConfig, TlsConfig, TranscodeConfig,
};
use serde::Serialize;
use std::sync::Arc;

/// Narrow seam between the admin crate (which exposes config-edit endpoints)
/// and the server crate (which owns the actual diff/apply logic). The trait
/// stays oblivious to MountRegistry, AutoDjRegistry, etc.
#[async_trait]
pub trait ConfigApplier: Send + Sync {
    /// Apply `new_cfg` to the running server. Returns a list of human-readable
    /// "restart required" warnings on success, or a single error message on
    /// failure.
    async fn apply(&self, new_cfg: Config) -> Result<Vec<String>, String>;
}

pub type ConfigApplierRef = Arc<dyn ConfigApplier>;

const REDACTED: &str = "***";

#[derive(Serialize)]
pub struct ConfigResponse {
    pub server: ServerConfig,
    pub logging: LoggingConfig,
    pub auth: AuthConfig,
    pub limits: LimitsConfig,
    pub mounts: Vec<MountConfig>,
    pub autodjs: Vec<AutoDjConfig>,
    pub relays: Vec<RelayConfig>,
    pub tls: Option<TlsConfig>,
    pub transcode: Option<TranscodeConfig>,
    pub path: Option<String>,
    pub source: &'static str,
}

fn redact(cfg: &Config) -> Config {
    let mut out = cfg.clone();
    for u in &mut out.auth.users {
        u.password_bcrypt = REDACTED.to_string();
    }
    if out.auth.source_password.is_some() {
        out.auth.source_password = Some(REDACTED.to_string());
    }
    for m in &mut out.mounts {
        m.source_password = REDACTED.to_string();
    }
    for r in &mut out.relays {
        if r.password.is_some() {
            r.password = Some(REDACTED.to_string());
        }
    }
    out
}

fn build_config_response(cfg: &Config, state: &AdminState) -> ConfigResponse {
    let redacted = redact(cfg);
    let path_swap = state.config_path.load();
    let (path, source) = match path_swap.as_ref().as_ref() {
        Some(p) => (Some(p.display().to_string()), "file"),
        None => (None, "defaults"),
    };
    ConfigResponse {
        server: redacted.server,
        logging: redacted.logging,
        auth: redacted.auth,
        limits: redacted.limits,
        mounts: redacted.mounts,
        autodjs: redacted.autodjs,
        relays: redacted.relays,
        tls: redacted.tls,
        transcode: redacted.transcode,
        path,
        source,
    }
}

pub async fn get_config(State(state): State<AdminState>) -> impl IntoResponse {
    let cfg = state.config.load_full();
    (StatusCode::OK, Json(build_config_response(&cfg, &state))).into_response()
}

// ─── PUT /api/config/server ────────────────────────────────────────────────

use crate::config_write::{
    self, LimitsSubPatch, LoggingSubPatch, ServerPatch as WritePatch, ServerSubPatch, WriteError,
};
use rustyice_core::config::LogFormat;
use serde::Deserialize;
use toml_edit::DocumentMut;

#[derive(Deserialize)]
pub struct ServerPutBody {
    pub server: ServerSubBody,
    pub logging: LoggingSubBody,
    pub limits: LimitsSubBody,
}

#[derive(Deserialize)]
pub struct ServerSubBody {
    pub stream_bind: std::net::SocketAddr,
    pub admin_bind: std::net::SocketAddr,
    pub hostname: String,
}

#[derive(Deserialize)]
pub struct LoggingSubBody {
    pub level: String,
    pub format: String,
}

#[derive(Deserialize)]
pub struct LimitsSubBody {
    pub max_listeners_global: u32,
    pub ring_size: usize,
    pub slow_listener_grace_s: u64,
    pub burst_size: u32,
    pub source_max_kbps: Option<u32>,
}

#[derive(Serialize)]
pub struct PutSuccess {
    #[serde(flatten)]
    pub config: ConfigResponse,
    pub applied_warnings: Vec<String>,
}

#[derive(Serialize)]
pub struct PutError {
    pub error: String,
    pub field: Option<String>,
    pub disk_written: bool,
}

pub async fn put_server(
    State(state): State<AdminState>,
    Json(body): Json<ServerPutBody>,
) -> impl IntoResponse {
    let patch = WritePatch {
        server: ServerSubPatch {
            stream_bind: body.server.stream_bind,
            admin_bind: body.server.admin_bind,
            hostname: body.server.hostname,
        },
        logging: LoggingSubPatch {
            level: body.logging.level,
            format: body.logging.format,
        },
        limits: LimitsSubPatch {
            max_listeners_global: body.limits.max_listeners_global,
            ring_size: body.limits.ring_size,
            slow_listener_grace_s: body.limits.slow_listener_grace_s,
            burst_size: body.limits.burst_size,
            source_max_kbps: body.limits.source_max_kbps,
        },
    };

    if let Err(WriteError::Validate { field, message }) =
        config_write::validate_server_patch(&patch)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(PutError { error: message, field: Some(field), disk_written: false }),
        )
            .into_response();
    }

    let current = state.config.load_full();
    let mut candidate: Config = (*current).clone();
    candidate.server.stream_bind = patch.server.stream_bind;
    candidate.server.admin_bind = patch.server.admin_bind;
    candidate.server.hostname = patch.server.hostname.clone();
    candidate.logging.level = patch.logging.level.clone();
    candidate.logging.format = match patch.logging.format.as_str() {
        "pretty" => LogFormat::Pretty,
        "json" => LogFormat::Json,
        _ => unreachable!("validated above"),
    };
    candidate.limits.max_listeners_global = patch.limits.max_listeners_global;
    candidate.limits.ring_size = patch.limits.ring_size;
    candidate.limits.slow_listener_grace_s = patch.limits.slow_listener_grace_s;
    candidate.limits.burst_size = patch.limits.burst_size;
    candidate.limits.source_max_kbps = patch.limits.source_max_kbps;

    if let Err(msg) = candidate.validate_paths() {
        return (
            StatusCode::BAD_REQUEST,
            Json(PutError { error: msg, field: None, disk_written: false }),
        )
            .into_response();
    }

    let _guard = state.config_write_lock.lock().await;

    let path_snapshot = state.config_path.load_full();
    let (doc_string, path_for_write): (String, std::path::PathBuf) = match path_snapshot.as_ref() {
        Some(p) => match std::fs::read_to_string(p) {
            Ok(s) => (s, p.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let bootstrap = match toml::to_string(&candidate) {
                    Ok(s) => s,
                    Err(e) => return io_error_response(format!("serialize: {e}")),
                };
                (bootstrap, p.clone())
            }
            Err(e) => return io_error_response(format!("read config: {e}")),
        },
        None => {
            let bootstrap = match toml::to_string(&candidate) {
                Ok(s) => s,
                Err(e) => return io_error_response(format!("serialize: {e}")),
            };
            (bootstrap, std::path::PathBuf::from("config.toml"))
        }
    };

    let mut doc: DocumentMut = match doc_string.parse() {
        Ok(d) => d,
        Err(e) => return io_error_response(format!("parse existing config: {e}")),
    };
    config_write::apply_server_patch(&mut doc, &patch);

    if let Err(e) = config_write::atomic_write(&path_for_write, &doc.to_string()) {
        return io_error_response(format!("write config.toml: {e}"));
    }

    let apply_result = state.config_applier.apply(candidate).await;
    match apply_result {
        Ok(warnings) => {
            if path_snapshot.as_ref().is_none() {
                state.config_path.store(Arc::new(Some(path_for_write)));
            }
            tracing::info!(section = "server", "config saved via api");
            let cfg = state.config.load_full();
            let resp_body = build_config_response(&cfg, &state);
            (
                StatusCode::OK,
                Json(PutSuccess { config: resp_body, applied_warnings: warnings }),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(section = "server", error = %e, "config apply failed (disk already written)");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(PutError { error: e, field: None, disk_written: true }),
            )
                .into_response()
        }
    }
}

fn io_error_response(msg: String) -> axum::response::Response {
    tracing::error!(error = %msg, "config save disk error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(PutError { error: msg, field: None, disk_written: false }),
    )
        .into_response()
}
