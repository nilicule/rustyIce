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
    self, AuthSubPatch, LimitsSubPatch, LoggingSubPatch, MountSubPatch, MountsPatch,
    RelaySubPatch, RelaysPatch, ServerPatch as WritePatch, ServerSubPatch, TranscodePatch,
    TranscodeSubPatch, WriteError,
};
use rustyice_core::config::{LogFormat, TranscodeFormat};
use serde::Deserialize;
use toml_edit::DocumentMut;

#[derive(Deserialize)]
pub struct ServerPutBody {
    pub server: ServerSubBody,
    pub logging: LoggingSubBody,
    pub limits: LimitsSubBody,
    /// Optional `[auth]` partial. The only field surfaced here is
    /// `source_password` — per-user changes go through the Users section.
    #[serde(default)]
    pub auth: Option<AuthSubBody>,
}

#[derive(Deserialize)]
pub struct AuthSubBody {
    #[serde(default)]
    pub source_password: Option<String>,
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
    // Resolve the global source_password the same way mounts do: empty
    // string or the redacted sentinel means "leave the existing value
    // alone". An explicit non-empty new value rotates it. The handler
    // never has a way to *clear* the password from the UI — operators
    // can drop the key by editing config.toml directly.
    let current = state.config.load_full();
    let auth_patch = match body.auth.as_ref().and_then(|a| a.source_password.as_deref()) {
        Some(p) if !p.is_empty() && p != "***" => Some(AuthSubPatch {
            source_password: Some(p.to_string()),
        }),
        _ => None,
    };

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
        auth: auth_patch,
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
    if let Some(auth_patch) = &patch.auth {
        candidate.auth.source_password = auth_patch.source_password.clone();
    }

    if let Err(msg) = candidate.validate_paths() {
        return (
            StatusCode::BAD_REQUEST,
            Json(PutError { error: msg, field: None, disk_written: false }),
        )
            .into_response();
    }

    persist_and_apply(&state, "server", candidate, |doc| {
        config_write::apply_server_patch(doc, &patch);
    })
    .await
}

// ─── PUT /api/config/transcode ────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TranscodePutBody {
    /// `None` (or an absent `transcode` key with `#[serde(default)]`) removes
    /// the global transcode block.
    #[serde(default)]
    pub transcode: Option<TranscodeSubBody>,
}

#[derive(Deserialize)]
pub struct TranscodeSubBody {
    pub format: String,
    pub sample_rate: u32,
    pub bitrate_kbps: u32,
}

pub async fn put_transcode(
    State(state): State<AdminState>,
    Json(body): Json<TranscodePutBody>,
) -> impl IntoResponse {
    let patch = TranscodePatch {
        transcode: body.transcode.as_ref().map(|s| TranscodeSubPatch {
            format: s.format.clone(),
            sample_rate: s.sample_rate,
            bitrate_kbps: s.bitrate_kbps,
        }),
    };

    if let Err(WriteError::Validate { field, message }) =
        config_write::validate_transcode_patch(&patch)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(PutError { error: message, field: Some(field), disk_written: false }),
        )
            .into_response();
    }

    let current = state.config.load_full();
    let mut candidate: Config = (*current).clone();
    candidate.transcode = patch.transcode.as_ref().map(|s| TranscodeConfig {
        format: match s.format.as_str() {
            "mp3" => TranscodeFormat::Mp3,
            "vorbis" => TranscodeFormat::Vorbis,
            _ => unreachable!("validated above"),
        },
        sample_rate: s.sample_rate,
        bitrate_kbps: s.bitrate_kbps,
    });

    if let Err(msg) = candidate.validate_paths() {
        return (
            StatusCode::BAD_REQUEST,
            Json(PutError { error: msg, field: None, disk_written: false }),
        )
            .into_response();
    }

    persist_and_apply(&state, "transcode", candidate, |doc| {
        config_write::apply_transcode_patch(doc, &patch);
    })
    .await
}

// ─── PUT /api/config/mounts ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MountsPutBody {
    pub mounts: Vec<MountSubBody>,
}

#[derive(Deserialize)]
pub struct MountSubBody {
    pub path: String,
    /// Optional on the wire so the client can omit it (or send the redaction
    /// sentinel) to mean "keep the existing password". The server resolves
    /// every entry to a concrete password before patching disk.
    #[serde(default)]
    pub source_password: Option<String>,
    #[serde(default)]
    pub max_listeners: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub burst_size: Option<u32>,
    #[serde(default)]
    pub transcode: Option<TranscodeSubBody>,
}

pub async fn put_mounts(
    State(state): State<AdminState>,
    Json(body): Json<MountsPutBody>,
) -> impl IntoResponse {
    // Map existing mount path → current source_password so we can keep
    // unchanged passwords without making the client re-enter them.
    let current = state.config.load_full();
    let existing_passwords: std::collections::HashMap<&str, &str> = current
        .mounts
        .iter()
        .map(|m| (m.path.as_str(), m.source_password.as_str()))
        .collect();

    // Resolve each request entry's password in this priority order:
    //   1. Explicit non-empty value in the request (and not the redacted
    //      sentinel) — operator is rotating or setting a fresh password.
    //   2. The existing per-mount password if the path is already configured
    //      — operator left the field blank to keep what's there.
    //   3. The global `[auth].source_password` — operator created a new mount
    //      and is happy to fall back to the shared source password.
    //   4. Otherwise reject — there is no password to authenticate sources
    //      against.
    let global_source_password = current.auth.source_password.clone();
    let mut resolved: Vec<MountSubPatch> = Vec::with_capacity(body.mounts.len());
    for (idx, m) in body.mounts.into_iter().enumerate() {
        let supplied = m.source_password.as_deref();
        let password = match supplied {
            Some(p) if !p.is_empty() && p != "***" => p.to_string(),
            _ => {
                if let Some(p) = existing_passwords.get(m.path.as_str()) {
                    (*p).to_string()
                } else if let Some(p) = &global_source_password {
                    p.clone()
                } else {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(PutError {
                            error: "new mount requires source_password (no global \
                                    [auth].source_password is set to fall back on)"
                                .into(),
                            field: Some(format!("mounts[{idx}].source_password")),
                            disk_written: false,
                        }),
                    )
                        .into_response();
                }
            }
        };
        resolved.push(MountSubPatch {
            path: m.path,
            source_password: password,
            max_listeners: m.max_listeners,
            name: m.name,
            description: m.description,
            genre: m.genre,
            url: m.url,
            burst_size: m.burst_size,
            transcode: m.transcode.map(|t| TranscodeSubPatch {
                format: t.format,
                sample_rate: t.sample_rate,
                bitrate_kbps: t.bitrate_kbps,
            }),
        });
    }
    let patch = MountsPatch { mounts: resolved };

    if let Err(WriteError::Validate { field, message }) =
        config_write::validate_mounts_patch(&patch)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(PutError { error: message, field: Some(field), disk_written: false }),
        )
            .into_response();
    }

    // Build candidate Config. Reuse the `current` snapshot loaded above.
    let mut candidate: Config = (*current).clone();
    candidate.mounts = patch
        .mounts
        .iter()
        .map(|m| MountConfig {
            path: m.path.clone(),
            source_password: m.source_password.clone(),
            max_listeners: m.max_listeners,
            name: m.name.clone(),
            description: m.description.clone(),
            genre: m.genre.clone(),
            url: m.url.clone(),
            burst_size: m.burst_size,
            transcode: m.transcode.as_ref().map(|t| TranscodeConfig {
                format: match t.format.as_str() {
                    "mp3" => TranscodeFormat::Mp3,
                    "vorbis" => TranscodeFormat::Vorbis,
                    _ => unreachable!("validated above"),
                },
                sample_rate: t.sample_rate,
                bitrate_kbps: t.bitrate_kbps,
            }),
        })
        .collect();

    // Cross-section: no path collisions with autodjs/relays.
    if let Err(msg) = candidate.validate_paths() {
        return (
            StatusCode::BAD_REQUEST,
            Json(PutError { error: msg, field: None, disk_written: false }),
        )
            .into_response();
    }

    persist_and_apply(&state, "mounts", candidate, |doc| {
        config_write::apply_mounts_patch(doc, &patch);
    })
    .await
}

// ─── PUT /api/config/relays ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RelaysPutBody {
    pub relays: Vec<RelaySubBody>,
}

#[derive(Deserialize)]
pub struct RelaySubBody {
    pub mount: String,
    pub upstream: String,
    #[serde(default = "default_true_relay_enabled_body")]
    pub enabled: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    /// Optional on the wire. Empty/omitted/sentinel means "keep existing
    /// upstream password for relays that match by mount". A new value
    /// rotates it; for brand-new relays an empty value is accepted (no
    /// fallback exists — relays without credentials connect anonymously).
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub max_listeners: Option<u32>,
    #[serde(default)]
    pub burst_size: Option<u32>,
    #[serde(default)]
    pub transcode: Option<TranscodeSubBody>,
}

fn default_true_relay_enabled_body() -> bool {
    true
}

pub async fn put_relays(
    State(state): State<AdminState>,
    Json(body): Json<RelaysPutBody>,
) -> impl IntoResponse {
    // Map existing relay mount path → current password so we can keep
    // unchanged credentials without making the client re-enter them.
    let current = state.config.load_full();
    let existing_passwords: std::collections::HashMap<&str, &str> = current
        .relays
        .iter()
        .filter_map(|r| r.password.as_deref().map(|p| (r.mount.as_str(), p)))
        .collect();

    let mut resolved: Vec<RelaySubPatch> = Vec::with_capacity(body.relays.len());
    for r in body.relays.into_iter() {
        let supplied = r.password.as_deref();
        let password = match supplied {
            Some(p) if !p.is_empty() && p != "***" => Some(p.to_string()),
            Some(_) => existing_passwords.get(r.mount.as_str()).map(|p| (*p).to_string()),
            None => existing_passwords.get(r.mount.as_str()).map(|p| (*p).to_string()),
        };
        resolved.push(RelaySubPatch {
            mount: r.mount,
            upstream: r.upstream,
            enabled: r.enabled,
            name: r.name,
            description: r.description,
            genre: r.genre,
            url: r.url,
            username: r.username,
            password,
            max_listeners: r.max_listeners,
            burst_size: r.burst_size,
            transcode: r.transcode.map(|t| TranscodeSubPatch {
                format: t.format,
                sample_rate: t.sample_rate,
                bitrate_kbps: t.bitrate_kbps,
            }),
        });
    }
    let patch = RelaysPatch { relays: resolved };

    if let Err(WriteError::Validate { field, message }) =
        config_write::validate_relays_patch(&patch)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(PutError { error: message, field: Some(field), disk_written: false }),
        )
            .into_response();
    }

    let mut candidate: Config = (*current).clone();
    candidate.relays = patch
        .relays
        .iter()
        .map(|r| RelayConfig {
            mount: r.mount.clone(),
            upstream: r.upstream.clone(),
            name: r.name.clone(),
            description: r.description.clone(),
            genre: r.genre.clone(),
            url: r.url.clone(),
            enabled: r.enabled,
            username: r.username.clone(),
            password: r.password.clone(),
            max_listeners: r.max_listeners,
            burst_size: r.burst_size,
            transcode: r.transcode.as_ref().map(|t| TranscodeConfig {
                format: match t.format.as_str() {
                    "mp3" => TranscodeFormat::Mp3,
                    "vorbis" => TranscodeFormat::Vorbis,
                    _ => unreachable!("validated above"),
                },
                sample_rate: t.sample_rate,
                bitrate_kbps: t.bitrate_kbps,
            }),
        })
        .collect();

    // Cross-section: no path collisions with mounts/autodjs.
    if let Err(msg) = candidate.validate_paths() {
        return (
            StatusCode::BAD_REQUEST,
            Json(PutError { error: msg, field: None, disk_written: false }),
        )
            .into_response();
    }

    persist_and_apply(&state, "relays", candidate, |doc| {
        config_write::apply_relays_patch(doc, &patch);
    })
    .await
}

/// Shared persist-and-apply pipeline used by every `PUT /api/config/<section>`
/// handler. The caller provides:
/// - a validated candidate `Config` (with the section's patch already overlaid)
/// - a closure that mutates a `toml_edit::DocumentMut` to apply the same patch
/// - the section name, used purely for logging
///
/// Steps: lock → read disk (or bootstrap) → patch doc → atomic write → apply.
async fn persist_and_apply(
    state: &AdminState,
    section: &'static str,
    candidate: Config,
    patch_doc: impl Fn(&mut DocumentMut),
) -> axum::response::Response {
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
    patch_doc(&mut doc);

    if let Err(e) = config_write::atomic_write(&path_for_write, &doc.to_string()) {
        return io_error_response(format!("write config.toml: {e}"));
    }

    let apply_result = state.config_applier.apply(candidate).await;
    match apply_result {
        Ok(warnings) => {
            if path_snapshot.as_ref().is_none() {
                state.config_path.store(Arc::new(Some(path_for_write)));
            }
            tracing::info!(section, "config saved via api");
            let cfg = state.config.load_full();
            let resp_body = build_config_response(&cfg, state);
            (
                StatusCode::OK,
                Json(PutSuccess { config: resp_body, applied_warnings: warnings }),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(section, error = %e, "config apply failed (disk already written)");
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
