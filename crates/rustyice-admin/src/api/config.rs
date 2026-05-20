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
