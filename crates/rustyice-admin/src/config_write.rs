//! Format-preserving writes to `config.toml`.
//!
//! Each per-section function takes a `toml_edit::DocumentMut` and a typed
//! patch struct, mutates only the keys named in the patch, and leaves
//! comments, key order, and whitespace intact.

use serde::Deserialize;
use std::net::SocketAddr;
use toml_edit::{value, DocumentMut, Item};

#[derive(Debug, Deserialize)]
pub struct ServerPatch {
    pub server: ServerSubPatch,
    pub logging: LoggingSubPatch,
    pub limits: LimitsSubPatch,
    /// Optional `[auth]` partial. `None` means "don't touch the [auth] table
    /// at all" — leaves user entries and any other operator-set keys alone.
    /// Per-user management lives on the Users section.
    #[serde(default)]
    pub auth: Option<AuthSubPatch>,
}

#[derive(Debug, Deserialize)]
pub struct AuthSubPatch {
    /// `Some(s)` writes `[auth].source_password = s`; `None` removes the key.
    pub source_password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ServerSubPatch {
    pub stream_bind: SocketAddr,
    pub admin_bind: SocketAddr,
    pub hostname: String,
}

#[derive(Debug, Deserialize)]
pub struct LoggingSubPatch {
    pub level: String,
    /// "pretty" or "json".
    pub format: String,
}

#[derive(Debug, Deserialize)]
pub struct LimitsSubPatch {
    pub max_listeners_global: u32,
    pub ring_size: usize,
    pub slow_listener_grace_s: u64,
    pub burst_size: u32,
    /// `None` removes the key from the file; `Some(n)` writes it.
    pub source_max_kbps: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("invalid value in field {field}: {message}")]
    Validate { field: String, message: String },
    #[error("cannot read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot parse existing config file: {0}")]
    Parse(#[from] toml_edit::TomlError),
}

/// Run semantic validation that goes beyond serde type coercion. Returns
/// a structured error pointing at the offending field path.
///
/// # Errors
/// Returns `WriteError::Validate` when a field fails its semantic bound.
pub fn validate_server_patch(p: &ServerPatch) -> Result<(), WriteError> {
    if p.limits.ring_size == 0 {
        return Err(WriteError::Validate {
            field: "limits.ring_size".into(),
            message: "must be greater than 0".into(),
        });
    }
    if p.limits.max_listeners_global == 0 {
        return Err(WriteError::Validate {
            field: "limits.max_listeners_global".into(),
            message: "must be greater than 0".into(),
        });
    }
    const MAX_BURST: u32 = 16 * 1024 * 1024;
    if p.limits.burst_size > MAX_BURST {
        return Err(WriteError::Validate {
            field: "limits.burst_size".into(),
            message: format!("must be <= {MAX_BURST} bytes (16 MiB)"),
        });
    }
    if p.logging.format != "pretty" && p.logging.format != "json" {
        return Err(WriteError::Validate {
            field: "logging.format".into(),
            message: "must be \"pretty\" or \"json\"".into(),
        });
    }
    if p.server.hostname.trim().is_empty() {
        return Err(WriteError::Validate {
            field: "server.hostname".into(),
            message: "must be non-empty".into(),
        });
    }
    if p.logging.level.trim().is_empty() {
        return Err(WriteError::Validate {
            field: "logging.level".into(),
            message: "must be non-empty".into(),
        });
    }
    Ok(())
}

/// Patch the three Server-section tables in `doc`. Creates missing tables.
pub fn apply_server_patch(doc: &mut DocumentMut, p: &ServerPatch) {
    let server = ensure_table(doc, "server");
    server["stream_bind"] = value(p.server.stream_bind.to_string());
    server["admin_bind"] = value(p.server.admin_bind.to_string());
    server["hostname"] = value(p.server.hostname.clone());

    let logging = ensure_table(doc, "logging");
    logging["level"] = value(p.logging.level.clone());
    logging["format"] = value(p.logging.format.clone());

    let limits = ensure_table(doc, "limits");
    limits["max_listeners_global"] = value(i64::from(p.limits.max_listeners_global));
    limits["ring_size"] = value(i64::try_from(p.limits.ring_size).unwrap_or(i64::MAX));
    limits["slow_listener_grace_s"] =
        value(i64::try_from(p.limits.slow_listener_grace_s).unwrap_or(i64::MAX));
    limits["burst_size"] = value(i64::from(p.limits.burst_size));
    match p.limits.source_max_kbps {
        Some(n) => {
            limits["source_max_kbps"] = value(i64::from(n));
        }
        None => {
            if let Some(t) = limits.as_table_mut() {
                t.remove("source_max_kbps");
            }
        }
    }

    // Only touch `[auth]` when the patch explicitly opts in — this leaves
    // `[[auth.users]]` and any other operator-managed keys untouched on
    // every Server-section save.
    if let Some(auth_patch) = &p.auth {
        let auth = ensure_table(doc, "auth");
        match &auth_patch.source_password {
            Some(s) => {
                auth["source_password"] = value(s.clone());
            }
            None => {
                if let Some(t) = auth.as_table_mut() {
                    t.remove("source_password");
                }
            }
        }
    }
}

// ─── Transcode section ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TranscodePatch {
    /// `None` removes the `[transcode]` block from the file; `Some(_)` writes it.
    pub transcode: Option<TranscodeSubPatch>,
}

#[derive(Debug, Deserialize)]
pub struct TranscodeSubPatch {
    /// "mp3" or "vorbis".
    pub format: String,
    pub sample_rate: u32,
    pub bitrate_kbps: u32,
}

/// # Errors
/// Returns `WriteError::Validate` when a sub-field is out of range.
pub fn validate_transcode_patch(p: &TranscodePatch) -> Result<(), WriteError> {
    let Some(sub) = &p.transcode else {
        return Ok(());
    };
    if sub.format != "mp3" && sub.format != "vorbis" {
        return Err(WriteError::Validate {
            field: "transcode.format".into(),
            message: "must be \"mp3\" or \"vorbis\"".into(),
        });
    }
    if sub.sample_rate == 0 {
        return Err(WriteError::Validate {
            field: "transcode.sample_rate".into(),
            message: "must be greater than 0".into(),
        });
    }
    if sub.sample_rate > 192_000 {
        return Err(WriteError::Validate {
            field: "transcode.sample_rate".into(),
            message: "must be <= 192000 Hz".into(),
        });
    }
    if sub.bitrate_kbps == 0 {
        return Err(WriteError::Validate {
            field: "transcode.bitrate_kbps".into(),
            message: "must be greater than 0".into(),
        });
    }
    if sub.bitrate_kbps > 512 {
        return Err(WriteError::Validate {
            field: "transcode.bitrate_kbps".into(),
            message: "must be <= 512 kbps".into(),
        });
    }
    Ok(())
}

/// Patch the `[transcode]` table in `doc`. Removes it entirely when the
/// patch's `transcode` is `None` — leaves the rest of the document alone.
pub fn apply_transcode_patch(doc: &mut DocumentMut, p: &TranscodePatch) {
    match &p.transcode {
        Some(sub) => {
            let table = ensure_table(doc, "transcode");
            table["format"] = value(sub.format.clone());
            table["sample_rate"] = value(i64::from(sub.sample_rate));
            table["bitrate_kbps"] = value(i64::from(sub.bitrate_kbps));
        }
        None => {
            doc.as_table_mut().remove("transcode");
        }
    }
}

fn ensure_table<'a>(doc: &'a mut DocumentMut, name: &str) -> &'a mut Item {
    if doc.get(name).is_none() {
        doc[name] = toml_edit::table();
    }
    &mut doc[name]
}

// ─── Mounts section ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MountsPatch {
    pub mounts: Vec<MountSubPatch>,
}

#[derive(Debug, Deserialize)]
pub struct MountSubPatch {
    pub path: String,
    pub source_password: String,
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
    /// Optional per-mount transcode override. Reuses `TranscodeSubPatch`
    /// (the same shape as the global transcode block).
    #[serde(default)]
    pub transcode: Option<TranscodeSubPatch>,
}

/// # Errors
/// Returns `WriteError::Validate` for empty/duplicate paths, empty source
/// passwords, or out-of-range per-mount transcode fields.
pub fn validate_mounts_patch(p: &MountsPatch) -> Result<(), WriteError> {
    let mut seen = std::collections::HashSet::new();
    for (idx, m) in p.mounts.iter().enumerate() {
        let prefix = format!("mounts[{idx}]");
        if m.path.trim().is_empty() {
            return Err(WriteError::Validate {
                field: format!("{prefix}.path"),
                message: "must be non-empty".into(),
            });
        }
        if !m.path.starts_with('/') {
            return Err(WriteError::Validate {
                field: format!("{prefix}.path"),
                message: "must start with \"/\"".into(),
            });
        }
        if !seen.insert(m.path.clone()) {
            return Err(WriteError::Validate {
                field: format!("{prefix}.path"),
                message: format!("duplicate mount path: {}", m.path),
            });
        }
        if m.source_password.trim().is_empty() {
            return Err(WriteError::Validate {
                field: format!("{prefix}.source_password"),
                message: "must be non-empty".into(),
            });
        }
        if let Some(0) = m.max_listeners {
            return Err(WriteError::Validate {
                field: format!("{prefix}.max_listeners"),
                message: "must be greater than 0 when set".into(),
            });
        }
        if let Some(b) = m.burst_size {
            const MAX_BURST: u32 = 16 * 1024 * 1024;
            if b > MAX_BURST {
                return Err(WriteError::Validate {
                    field: format!("{prefix}.burst_size"),
                    message: format!("must be <= {MAX_BURST} bytes (16 MiB)"),
                });
            }
        }
        if let Some(tc) = &m.transcode {
            // Reuse the global transcode validator by wrapping into the
            // same shape, but rewrite the error path to point at this mount.
            let wrap = TranscodePatch { transcode: Some(TranscodeSubPatch {
                format: tc.format.clone(),
                sample_rate: tc.sample_rate,
                bitrate_kbps: tc.bitrate_kbps,
            }) };
            if let Err(WriteError::Validate { field, message }) = validate_transcode_patch(&wrap) {
                let sub = field.strip_prefix("transcode.").unwrap_or(&field);
                return Err(WriteError::Validate {
                    field: format!("{prefix}.transcode.{sub}"),
                    message,
                });
            }
        }
    }
    Ok(())
}

// ─── Users section (under [[auth.users]]) ────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UsersPatch {
    pub users: Vec<UserSubPatch>,
}

#[derive(Debug, Deserialize)]
pub struct UserSubPatch {
    pub username: String,
    /// Already-hashed bcrypt string. The handler resolves any
    /// plaintext/sentinel values to this form before constructing the patch.
    pub password_bcrypt: String,
    /// "admin" or "operator".
    pub role: String,
}

/// # Errors
/// Returns `WriteError::Validate` for empty/duplicate usernames, empty
/// password hashes, or unknown role values.
pub fn validate_users_patch(p: &UsersPatch) -> Result<(), WriteError> {
    let mut seen = std::collections::HashSet::new();
    for (idx, u) in p.users.iter().enumerate() {
        let prefix = format!("users[{idx}]");
        if u.username.trim().is_empty() {
            return Err(WriteError::Validate {
                field: format!("{prefix}.username"),
                message: "must be non-empty".into(),
            });
        }
        if !seen.insert(u.username.clone()) {
            return Err(WriteError::Validate {
                field: format!("{prefix}.username"),
                message: format!("duplicate username: {}", u.username),
            });
        }
        if u.password_bcrypt.is_empty() {
            return Err(WriteError::Validate {
                field: format!("{prefix}.password"),
                message: "must be set when creating a user".into(),
            });
        }
        if u.role != "admin" && u.role != "operator" {
            return Err(WriteError::Validate {
                field: format!("{prefix}.role"),
                message: "must be \"admin\" or \"operator\"".into(),
            });
        }
    }
    // Always require at least one admin to remain — otherwise the operator
    // locks themselves (and everyone else) out of the management surface.
    if !p.users.iter().any(|u| u.role == "admin") {
        return Err(WriteError::Validate {
            field: "users".into(),
            message: "at least one user must have the admin role".into(),
        });
    }
    Ok(())
}

/// Replace the `[[auth.users]]` array of tables in `doc`. Top-of-file and
/// other sections' comments survive; comments inside individual user
/// entries are not preserved. The `[auth]` table itself is left alone —
/// the `source_password` and any other auth-level keys stay untouched.
pub fn apply_users_patch(doc: &mut DocumentMut, p: &UsersPatch) {
    // Ensure [auth] exists, then replace its `users` array of tables.
    let auth = ensure_table(doc, "auth");
    let auth_table = auth.as_table_mut().expect("ensure_table returned non-table");
    if p.users.is_empty() {
        auth_table.remove("users");
        return;
    }
    let mut aot = toml_edit::ArrayOfTables::new();
    for u in &p.users {
        let mut t = toml_edit::Table::new();
        t["username"] = value(u.username.clone());
        t["password_bcrypt"] = value(u.password_bcrypt.clone());
        t["role"] = value(u.role.clone());
        aot.push(t);
    }
    auth_table.insert("users", Item::ArrayOfTables(aot));
}

// ─── AutoDJs section ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AutoDjsPatch {
    pub autodjs: Vec<AutoDjSubPatch>,
}

#[derive(Debug, Deserialize)]
pub struct AutoDjSubPatch {
    pub mount: String,
    pub folder: String,
    #[serde(default = "default_true_autodj")]
    pub enabled: bool,
    /// TOML field is `loop` — the deserializer receives it as `loop_playlist`
    /// because `loop` is reserved in Rust. The writer below emits `loop`.
    #[serde(rename = "loop", default = "default_true_autodj")]
    pub loop_playlist: bool,
    #[serde(default = "default_order_str")]
    pub order: String, // "shuffle" or "sequential"
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub max_listeners: Option<u32>,
    #[serde(default)]
    pub burst_size: Option<u32>,
    /// Required — every autodj has a transcode block in the on-disk schema.
    pub transcode: TranscodeSubPatch,
}

fn default_true_autodj() -> bool {
    true
}

fn default_order_str() -> String {
    "shuffle".to_string()
}

/// # Errors
/// Returns `WriteError::Validate` for empty/duplicate mount paths, empty
/// folders, bad `order` values, or invalid per-autodj transcode fields.
pub fn validate_autodjs_patch(p: &AutoDjsPatch) -> Result<(), WriteError> {
    let mut seen = std::collections::HashSet::new();
    for (idx, a) in p.autodjs.iter().enumerate() {
        let prefix = format!("autodjs[{idx}]");
        if a.mount.trim().is_empty() {
            return Err(WriteError::Validate {
                field: format!("{prefix}.mount"),
                message: "must be non-empty".into(),
            });
        }
        if !a.mount.starts_with('/') {
            return Err(WriteError::Validate {
                field: format!("{prefix}.mount"),
                message: "must start with \"/\"".into(),
            });
        }
        if !seen.insert(a.mount.clone()) {
            return Err(WriteError::Validate {
                field: format!("{prefix}.mount"),
                message: format!("duplicate autodj mount path: {}", a.mount),
            });
        }
        if a.folder.trim().is_empty() {
            return Err(WriteError::Validate {
                field: format!("{prefix}.folder"),
                message: "must be non-empty".into(),
            });
        }
        if a.order != "shuffle" && a.order != "sequential" {
            return Err(WriteError::Validate {
                field: format!("{prefix}.order"),
                message: "must be \"shuffle\" or \"sequential\"".into(),
            });
        }
        if let Some(0) = a.max_listeners {
            return Err(WriteError::Validate {
                field: format!("{prefix}.max_listeners"),
                message: "must be greater than 0 when set".into(),
            });
        }
        if let Some(b) = a.burst_size {
            const MAX_BURST: u32 = 16 * 1024 * 1024;
            if b > MAX_BURST {
                return Err(WriteError::Validate {
                    field: format!("{prefix}.burst_size"),
                    message: format!("must be <= {MAX_BURST} bytes (16 MiB)"),
                });
            }
        }
        // Per-autodj transcode is required, not optional — always validate.
        let wrap = TranscodePatch {
            transcode: Some(TranscodeSubPatch {
                format: a.transcode.format.clone(),
                sample_rate: a.transcode.sample_rate,
                bitrate_kbps: a.transcode.bitrate_kbps,
            }),
        };
        if let Err(WriteError::Validate { field, message }) = validate_transcode_patch(&wrap) {
            let sub = field.strip_prefix("transcode.").unwrap_or(&field);
            return Err(WriteError::Validate {
                field: format!("{prefix}.transcode.{sub}"),
                message,
            });
        }
    }
    Ok(())
}

/// Replace the `[[autodjs]]` array of tables in `doc`. Top-of-file and
/// other sections' comments survive; comments inside individual
/// `[[autodjs]]` entries are not preserved.
pub fn apply_autodjs_patch(doc: &mut DocumentMut, p: &AutoDjsPatch) {
    if p.autodjs.is_empty() {
        doc.as_table_mut().remove("autodjs");
        return;
    }
    let mut aot = toml_edit::ArrayOfTables::new();
    for a in &p.autodjs {
        let mut t = toml_edit::Table::new();
        t["mount"] = value(a.mount.clone());
        t["folder"] = value(a.folder.clone());
        if !a.enabled {
            t["enabled"] = value(false);
        }
        if !a.loop_playlist {
            // Default is true; only emit when explicitly false.
            t["loop"] = value(false);
        }
        // Always emit order so the file is self-explanatory.
        t["order"] = value(a.order.clone());
        if let Some(s) = &a.name {
            t["name"] = value(s.clone());
        }
        if let Some(s) = &a.description {
            t["description"] = value(s.clone());
        }
        if let Some(s) = &a.genre {
            t["genre"] = value(s.clone());
        }
        if let Some(s) = &a.url {
            t["url"] = value(s.clone());
        }
        if let Some(n) = a.max_listeners {
            t["max_listeners"] = value(i64::from(n));
        }
        if let Some(b) = a.burst_size {
            t["burst_size"] = value(i64::from(b));
        }
        // Required nested transcode table.
        let mut sub = toml_edit::Table::new();
        sub["format"] = value(a.transcode.format.clone());
        sub["sample_rate"] = value(i64::from(a.transcode.sample_rate));
        sub["bitrate_kbps"] = value(i64::from(a.transcode.bitrate_kbps));
        sub.set_implicit(false);
        t.insert("transcode", Item::Table(sub));
        aot.push(t);
    }
    doc.insert("autodjs", Item::ArrayOfTables(aot));
}

// ─── Relays section ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RelaysPatch {
    pub relays: Vec<RelaySubPatch>,
}

#[derive(Debug, Deserialize)]
pub struct RelaySubPatch {
    pub mount: String,
    pub upstream: String,
    #[serde(default = "default_true_relay_enabled")]
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
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub max_listeners: Option<u32>,
    #[serde(default)]
    pub burst_size: Option<u32>,
    /// Optional per-relay transcode override.
    #[serde(default)]
    pub transcode: Option<TranscodeSubPatch>,
}

fn default_true_relay_enabled() -> bool {
    true
}

/// # Errors
/// Returns `WriteError::Validate` for empty/duplicate mount paths, non-
/// http(s) upstream URLs, half-set credentials, or invalid per-relay
/// transcode fields.
pub fn validate_relays_patch(p: &RelaysPatch) -> Result<(), WriteError> {
    let mut seen = std::collections::HashSet::new();
    for (idx, r) in p.relays.iter().enumerate() {
        let prefix = format!("relays[{idx}]");
        if r.mount.trim().is_empty() {
            return Err(WriteError::Validate {
                field: format!("{prefix}.mount"),
                message: "must be non-empty".into(),
            });
        }
        if !r.mount.starts_with('/') {
            return Err(WriteError::Validate {
                field: format!("{prefix}.mount"),
                message: "must start with \"/\"".into(),
            });
        }
        if !seen.insert(r.mount.clone()) {
            return Err(WriteError::Validate {
                field: format!("{prefix}.mount"),
                message: format!("duplicate relay mount path: {}", r.mount),
            });
        }
        if !(r.upstream.starts_with("http://") || r.upstream.starts_with("https://")) {
            return Err(WriteError::Validate {
                field: format!("{prefix}.upstream"),
                message: "must be an http:// or https:// URL".into(),
            });
        }
        match (&r.username, &r.password) {
            (Some(u), None) if !u.is_empty() => {
                return Err(WriteError::Validate {
                    field: format!("{prefix}.password"),
                    message: "must be set when username is set".into(),
                });
            }
            (None, Some(p_)) if !p_.is_empty() => {
                return Err(WriteError::Validate {
                    field: format!("{prefix}.username"),
                    message: "must be set when password is set".into(),
                });
            }
            _ => {}
        }
        if let Some(0) = r.max_listeners {
            return Err(WriteError::Validate {
                field: format!("{prefix}.max_listeners"),
                message: "must be greater than 0 when set".into(),
            });
        }
        if let Some(b) = r.burst_size {
            const MAX_BURST: u32 = 16 * 1024 * 1024;
            if b > MAX_BURST {
                return Err(WriteError::Validate {
                    field: format!("{prefix}.burst_size"),
                    message: format!("must be <= {MAX_BURST} bytes (16 MiB)"),
                });
            }
        }
        if let Some(tc) = &r.transcode {
            let wrap = TranscodePatch { transcode: Some(TranscodeSubPatch {
                format: tc.format.clone(),
                sample_rate: tc.sample_rate,
                bitrate_kbps: tc.bitrate_kbps,
            }) };
            if let Err(WriteError::Validate { field, message }) = validate_transcode_patch(&wrap) {
                let sub = field.strip_prefix("transcode.").unwrap_or(&field);
                return Err(WriteError::Validate {
                    field: format!("{prefix}.transcode.{sub}"),
                    message,
                });
            }
        }
    }
    Ok(())
}

/// Replace the `[[relays]]` array of tables in `doc`. Top-of-file and
/// other sections' comments survive; comments inside individual
/// `[[relays]]` entries are not preserved.
pub fn apply_relays_patch(doc: &mut DocumentMut, p: &RelaysPatch) {
    if p.relays.is_empty() {
        doc.as_table_mut().remove("relays");
        return;
    }
    let mut aot = toml_edit::ArrayOfTables::new();
    for r in &p.relays {
        let mut t = toml_edit::Table::new();
        t["mount"] = value(r.mount.clone());
        t["upstream"] = value(r.upstream.clone());
        if !r.enabled {
            // Only write the field when it differs from the serde default
            // (true) so disabled is explicit and enabled stays implicit.
            t["enabled"] = value(false);
        }
        if let Some(s) = &r.name {
            t["name"] = value(s.clone());
        }
        if let Some(s) = &r.description {
            t["description"] = value(s.clone());
        }
        if let Some(s) = &r.genre {
            t["genre"] = value(s.clone());
        }
        if let Some(s) = &r.url {
            t["url"] = value(s.clone());
        }
        if let Some(s) = &r.username {
            t["username"] = value(s.clone());
        }
        if let Some(s) = &r.password {
            t["password"] = value(s.clone());
        }
        if let Some(n) = r.max_listeners {
            t["max_listeners"] = value(i64::from(n));
        }
        if let Some(b) = r.burst_size {
            t["burst_size"] = value(i64::from(b));
        }
        if let Some(tc) = &r.transcode {
            let mut sub = toml_edit::Table::new();
            sub["format"] = value(tc.format.clone());
            sub["sample_rate"] = value(i64::from(tc.sample_rate));
            sub["bitrate_kbps"] = value(i64::from(tc.bitrate_kbps));
            sub.set_implicit(false);
            t.insert("transcode", Item::Table(sub));
        }
        aot.push(t);
    }
    doc.insert("relays", Item::ArrayOfTables(aot));
}

/// Replace the `[[mounts]]` array of tables in `doc` with the patch's
/// list. Top-of-file and other sections' comments survive; comments
/// inside individual `[[mounts]]` entries are not preserved.
pub fn apply_mounts_patch(doc: &mut DocumentMut, p: &MountsPatch) {
    if p.mounts.is_empty() {
        doc.as_table_mut().remove("mounts");
        return;
    }
    let mut aot = toml_edit::ArrayOfTables::new();
    for m in &p.mounts {
        let mut t = toml_edit::Table::new();
        t["path"] = value(m.path.clone());
        t["source_password"] = value(m.source_password.clone());
        if let Some(n) = m.max_listeners {
            t["max_listeners"] = value(i64::from(n));
        }
        if let Some(s) = &m.name {
            t["name"] = value(s.clone());
        }
        if let Some(s) = &m.description {
            t["description"] = value(s.clone());
        }
        if let Some(s) = &m.genre {
            t["genre"] = value(s.clone());
        }
        if let Some(s) = &m.url {
            t["url"] = value(s.clone());
        }
        if let Some(b) = m.burst_size {
            t["burst_size"] = value(i64::from(b));
        }
        if let Some(tc) = &m.transcode {
            let mut sub = toml_edit::Table::new();
            sub["format"] = value(tc.format.clone());
            sub["sample_rate"] = value(i64::from(tc.sample_rate));
            sub["bitrate_kbps"] = value(i64::from(tc.bitrate_kbps));
            // Render as `[mounts.transcode]` inline-style sub-table.
            sub.set_implicit(false);
            t.insert("transcode", Item::Table(sub));
        }
        aot.push(t);
    }
    doc.insert("mounts", Item::ArrayOfTables(aot));
}

/// Atomically write `contents` to `path` by writing to a sibling tempfile
/// and renaming. Caller is responsible for retrying on transient errors.
///
/// # Errors
/// Returns the underlying `io::Error` if the tempfile cannot be created,
/// written, fsync'd, or renamed. On a failed rename the tempfile is best-
/// effort cleaned up.
pub fn atomic_write(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_doc() -> &'static str {
        r#"# top-of-file comment
[server]
stream_bind = "0.0.0.0:8000"
admin_bind  = "127.0.0.1:8001"
hostname    = "localhost"

[logging]
level  = "info"
format = "pretty"

[limits]
max_listeners_global  = 500
ring_size             = 64
slow_listener_grace_s = 2
burst_size            = 65536
# Leave source_max_kbps unset for unlimited.
"#
    }

    fn make_patch() -> ServerPatch {
        ServerPatch {
            server: ServerSubPatch {
                stream_bind: "0.0.0.0:8000".parse().unwrap(),
                admin_bind: "127.0.0.1:8001".parse().unwrap(),
                hostname: "localhost".to_string(),
            },
            logging: LoggingSubPatch {
                level: "info".to_string(),
                format: "pretty".to_string(),
            },
            limits: LimitsSubPatch {
                max_listeners_global: 500,
                ring_size: 64,
                slow_listener_grace_s: 2,
                burst_size: 65_536,
                source_max_kbps: None,
            },
            auth: None,
        }
    }

    #[test]
    fn server_patch_writes_global_source_password_when_auth_set() {
        let mut doc: DocumentMut = sample_doc().parse().unwrap();
        let mut patch = make_patch();
        patch.auth = Some(AuthSubPatch { source_password: Some("letmesource".into()) });
        apply_server_patch(&mut doc, &patch);
        let out = doc.to_string();
        assert!(out.contains("[auth]"), "expected [auth] table:\n{out}");
        assert!(out.contains(r#"source_password = "letmesource""#));
    }

    #[test]
    fn server_patch_clears_global_source_password_when_auth_some_none() {
        let with_pw = sample_doc().to_string()
            + "\n[auth]\nsource_password = \"oldpw\"\n";
        let mut doc: DocumentMut = with_pw.parse().unwrap();
        let mut patch = make_patch();
        patch.auth = Some(AuthSubPatch { source_password: None });
        apply_server_patch(&mut doc, &patch);
        let out = doc.to_string();
        assert!(!out.contains("source_password"), "key should be gone:\n{out}");
    }

    #[test]
    fn server_patch_leaves_auth_table_alone_when_patch_auth_is_none() {
        let with_pw = sample_doc().to_string()
            + "\n[auth]\nsource_password = \"oldpw\"\n\n[[auth.users]]\nusername = \"admin\"\npassword_bcrypt = \"$2y$12$hash\"\n";
        let mut doc: DocumentMut = with_pw.parse().unwrap();
        let patch = make_patch(); // auth = None
        apply_server_patch(&mut doc, &patch);
        let out = doc.to_string();
        // Both keys survive untouched.
        assert!(out.contains(r#"source_password = "oldpw""#), "kept source_password");
        assert!(out.contains(r#"username = "admin""#), "kept user entry");
        assert!(out.contains("$2y$12$hash"), "kept bcrypt hash");
    }

    #[test]
    fn round_trip_preserves_comments() {
        let mut doc: DocumentMut = sample_doc().parse().unwrap();
        apply_server_patch(&mut doc, &make_patch());
        let out = doc.to_string();
        assert!(out.contains("# top-of-file comment"), "lost top comment:\n{out}");
        assert!(out.contains("# Leave source_max_kbps unset for unlimited."));
    }

    #[test]
    fn patch_only_touches_named_keys_when_unchanged() {
        let mut doc: DocumentMut = sample_doc().parse().unwrap();
        let before = doc.to_string();
        apply_server_patch(&mut doc, &make_patch());
        let after = doc.to_string();
        assert_eq!(before, after, "no-op patch should not change the document");
    }

    #[test]
    fn hostname_change_only_touches_hostname() {
        let mut doc: DocumentMut = sample_doc().parse().unwrap();
        let mut patch = make_patch();
        patch.server.hostname = "radio.example".to_string();
        apply_server_patch(&mut doc, &patch);
        let out = doc.to_string();
        assert!(
            out.contains("hostname    = \"radio.example\"")
                || out.contains("hostname = \"radio.example\""),
            "expected hostname update, got:\n{out}",
        );
        assert!(out.contains("stream_bind = \"0.0.0.0:8000\""));
        assert!(out.contains("# Leave source_max_kbps unset for unlimited."));
    }

    #[test]
    fn removes_optional_when_set_to_none() {
        let with_set = sample_doc().replace(
            "burst_size            = 65536\n",
            "burst_size            = 65536\nsource_max_kbps       = 128\n",
        );
        let mut doc: DocumentMut = with_set.parse().unwrap();
        apply_server_patch(&mut doc, &make_patch()); // patch has source_max_kbps = None
        let out = doc.to_string();
        // Match the key as a line-leading assignment so the trailing comment
        // (`# Leave source_max_kbps unset for unlimited.`) doesn't trigger a
        // false positive.
        let has_key_assignment = out
            .lines()
            .any(|line| line.trim_start().starts_with("source_max_kbps") && line.contains('='));
        assert!(!has_key_assignment, "expected key removed, got:\n{out}");
    }

    #[test]
    fn validate_rejects_zero_ring_size() {
        let mut patch = make_patch();
        patch.limits.ring_size = 0;
        let err = validate_server_patch(&patch).unwrap_err();
        match err {
            WriteError::Validate { field, .. } => assert_eq!(field, "limits.ring_size"),
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_oversized_burst() {
        let mut patch = make_patch();
        patch.limits.burst_size = 32 * 1024 * 1024;
        let err = validate_server_patch(&patch).unwrap_err();
        match err {
            WriteError::Validate { field, .. } => assert_eq!(field, "limits.burst_size"),
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_bad_logging_format() {
        let mut patch = make_patch();
        patch.logging.format = "xml".to_string();
        assert!(matches!(
            validate_server_patch(&patch),
            Err(WriteError::Validate { field, .. }) if field == "logging.format"
        ));
    }

    // ── Transcode ──────────────────────────────────────────────────────────

    fn transcode_doc_with_block() -> &'static str {
        r#"# top-of-file comment
[server]
stream_bind = "0.0.0.0:8000"
admin_bind  = "127.0.0.1:8001"
hostname    = "localhost"

[logging]
level  = "info"
format = "pretty"

[limits]
max_listeners_global  = 500
ring_size             = 64
slow_listener_grace_s = 2
burst_size            = 65536

[transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 128
"#
    }

    fn transcode_doc_without_block() -> &'static str {
        r#"[server]
stream_bind = "0.0.0.0:8000"
admin_bind  = "127.0.0.1:8001"
hostname    = "localhost"

[logging]
level  = "info"
format = "pretty"

[limits]
max_listeners_global  = 500
ring_size             = 64
slow_listener_grace_s = 2
burst_size            = 65536
"#
    }

    #[test]
    fn transcode_patch_writes_new_block_when_absent() {
        let mut doc: DocumentMut = transcode_doc_without_block().parse().unwrap();
        apply_transcode_patch(
            &mut doc,
            &TranscodePatch {
                transcode: Some(TranscodeSubPatch {
                    format: "vorbis".into(),
                    sample_rate: 48_000,
                    bitrate_kbps: 96,
                }),
            },
        );
        let out = doc.to_string();
        assert!(out.contains("[transcode]"));
        assert!(out.contains(r#"format = "vorbis""#));
        assert!(out.contains("sample_rate = 48000"));
        assert!(out.contains("bitrate_kbps = 96"));
    }

    #[test]
    fn transcode_patch_updates_existing_block_preserving_other_keys() {
        let mut doc: DocumentMut = transcode_doc_with_block().parse().unwrap();
        apply_transcode_patch(
            &mut doc,
            &TranscodePatch {
                transcode: Some(TranscodeSubPatch {
                    format: "vorbis".into(),
                    sample_rate: 44_100,
                    bitrate_kbps: 192,
                }),
            },
        );
        let out = doc.to_string();
        assert!(out.contains("# top-of-file comment"), "lost top comment");
        assert!(out.contains(r#"hostname    = "localhost""#));
        assert!(out.contains(r#"format       = "vorbis""#) || out.contains(r#"format = "vorbis""#));
        assert!(out.contains("bitrate_kbps = 192") || out.contains("bitrate_kbps  = 192"));
    }

    #[test]
    fn transcode_patch_removes_block_when_none() {
        let mut doc: DocumentMut = transcode_doc_with_block().parse().unwrap();
        apply_transcode_patch(&mut doc, &TranscodePatch { transcode: None });
        let out = doc.to_string();
        assert!(!out.contains("[transcode]"), "block should be gone:\n{out}");
        assert!(!out.contains("bitrate_kbps"));
        // Untouched content survives.
        assert!(out.contains("# top-of-file comment") || out.starts_with("[server]"));
    }

    #[test]
    fn transcode_validate_rejects_bad_format() {
        let patch = TranscodePatch {
            transcode: Some(TranscodeSubPatch {
                format: "flac".into(),
                sample_rate: 44_100,
                bitrate_kbps: 128,
            }),
        };
        assert!(matches!(
            validate_transcode_patch(&patch),
            Err(WriteError::Validate { field, .. }) if field == "transcode.format"
        ));
    }

    #[test]
    fn transcode_validate_rejects_zero_sample_rate() {
        let patch = TranscodePatch {
            transcode: Some(TranscodeSubPatch {
                format: "mp3".into(),
                sample_rate: 0,
                bitrate_kbps: 128,
            }),
        };
        assert!(matches!(
            validate_transcode_patch(&patch),
            Err(WriteError::Validate { field, .. }) if field == "transcode.sample_rate"
        ));
    }

    #[test]
    fn transcode_validate_rejects_oversized_bitrate() {
        let patch = TranscodePatch {
            transcode: Some(TranscodeSubPatch {
                format: "mp3".into(),
                sample_rate: 44_100,
                bitrate_kbps: 800,
            }),
        };
        assert!(matches!(
            validate_transcode_patch(&patch),
            Err(WriteError::Validate { field, .. }) if field == "transcode.bitrate_kbps"
        ));
    }

    #[test]
    fn transcode_validate_accepts_none() {
        let patch = TranscodePatch { transcode: None };
        assert!(validate_transcode_patch(&patch).is_ok());
    }

    // ── Mounts ─────────────────────────────────────────────────────────────

    fn doc_with_two_mounts() -> &'static str {
        r#"# top-of-file comment
[server]
stream_bind = "0.0.0.0:8000"
admin_bind  = "127.0.0.1:8001"
hostname    = "localhost"

[logging]
level  = "info"
format = "pretty"

[limits]
max_listeners_global  = 500
ring_size             = 64
slow_listener_grace_s = 2
burst_size            = 65536

[[mounts]]
path            = "/stream"
source_password = "hackme"
name            = "First"

[[mounts]]
path            = "/jazz"
source_password = "jazz"
"#
    }

    fn doc_without_mounts() -> &'static str {
        r#"[server]
stream_bind = "0.0.0.0:8000"
admin_bind  = "127.0.0.1:8001"
hostname    = "localhost"

[logging]
level  = "info"
format = "pretty"

[limits]
max_listeners_global  = 500
ring_size             = 64
slow_listener_grace_s = 2
burst_size            = 65536
"#
    }

    fn basic_mount(path: &str) -> MountSubPatch {
        MountSubPatch {
            path: path.into(),
            source_password: "pw".into(),
            max_listeners: None,
            name: None,
            description: None,
            genre: None,
            url: None,
            burst_size: None,
            transcode: None,
        }
    }

    #[test]
    fn mounts_patch_adds_mount_to_empty_doc() {
        let mut doc: DocumentMut = doc_without_mounts().parse().unwrap();
        apply_mounts_patch(
            &mut doc,
            &MountsPatch { mounts: vec![basic_mount("/m1")] },
        );
        let out = doc.to_string();
        assert!(out.contains("[[mounts]]"));
        assert!(out.contains(r#"path = "/m1""#));
    }

    #[test]
    fn mounts_patch_replaces_existing_list() {
        let mut doc: DocumentMut = doc_with_two_mounts().parse().unwrap();
        apply_mounts_patch(
            &mut doc,
            &MountsPatch {
                mounts: vec![MountSubPatch {
                    path: "/replaced".into(),
                    source_password: "newpw".into(),
                    name: Some("Replaced".into()),
                    ..basic_mount("/replaced")
                }],
            },
        );
        let out = doc.to_string();
        assert!(out.contains(r#"path = "/replaced""#));
        // Old mounts are gone.
        assert!(!out.contains(r#"path = "/stream""#));
        assert!(!out.contains(r#"path = "/jazz""#));
        // Untouched sections survive (top-of-file comment, server block).
        assert!(out.contains("# top-of-file comment"));
        assert!(out.contains(r#"hostname    = "localhost""#));
    }

    #[test]
    fn mounts_patch_with_empty_list_removes_block() {
        let mut doc: DocumentMut = doc_with_two_mounts().parse().unwrap();
        apply_mounts_patch(&mut doc, &MountsPatch { mounts: vec![] });
        let out = doc.to_string();
        assert!(!out.contains("[[mounts]]"));
        assert!(out.contains("# top-of-file comment"));
    }

    #[test]
    fn mounts_patch_writes_optional_fields_only_when_set() {
        let mut doc: DocumentMut = doc_without_mounts().parse().unwrap();
        apply_mounts_patch(
            &mut doc,
            &MountsPatch {
                mounts: vec![MountSubPatch {
                    name: Some("My Radio".into()),
                    max_listeners: Some(100),
                    ..basic_mount("/m1")
                }],
            },
        );
        let out = doc.to_string();
        assert!(out.contains(r#"name = "My Radio""#));
        assert!(out.contains("max_listeners = 100"));
        // Fields left None aren't written. (We don't check `burst_size`
        // here because the seed doc carries a `[limits].burst_size = …`
        // line that always survives.)
        assert!(!out.contains("description"));
        assert!(!out.contains("genre"));
        assert!(!out.contains("url"));
    }

    #[test]
    fn mounts_patch_writes_per_mount_transcode_block() {
        let mut doc: DocumentMut = doc_without_mounts().parse().unwrap();
        apply_mounts_patch(
            &mut doc,
            &MountsPatch {
                mounts: vec![MountSubPatch {
                    transcode: Some(TranscodeSubPatch {
                        format: "vorbis".into(),
                        sample_rate: 48_000,
                        bitrate_kbps: 192,
                    }),
                    ..basic_mount("/m1")
                }],
            },
        );
        let out = doc.to_string();
        assert!(out.contains("[mounts.transcode]"), "missing nested table:\n{out}");
        assert!(out.contains(r#"format = "vorbis""#));
        assert!(out.contains("sample_rate = 48000"));
        assert!(out.contains("bitrate_kbps = 192"));
    }

    #[test]
    fn mounts_validate_rejects_duplicate_paths() {
        let patch = MountsPatch {
            mounts: vec![basic_mount("/dup"), basic_mount("/dup")],
        };
        let err = validate_mounts_patch(&patch).unwrap_err();
        match err {
            WriteError::Validate { field, message } => {
                assert_eq!(field, "mounts[1].path");
                assert!(message.contains("/dup"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn mounts_validate_rejects_empty_path() {
        let patch = MountsPatch { mounts: vec![basic_mount("")] };
        let err = validate_mounts_patch(&patch).unwrap_err();
        match err {
            WriteError::Validate { field, .. } => assert_eq!(field, "mounts[0].path"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn mounts_validate_rejects_path_without_leading_slash() {
        let patch = MountsPatch { mounts: vec![basic_mount("stream")] };
        let err = validate_mounts_patch(&patch).unwrap_err();
        match err {
            WriteError::Validate { field, message } => {
                assert_eq!(field, "mounts[0].path");
                assert!(message.contains("/"), "message should mention /: {message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn mounts_validate_rejects_empty_source_password() {
        let mut m = basic_mount("/m1");
        m.source_password = "  ".into();
        let patch = MountsPatch { mounts: vec![m] };
        let err = validate_mounts_patch(&patch).unwrap_err();
        match err {
            WriteError::Validate { field, .. } => assert_eq!(field, "mounts[0].source_password"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn mounts_validate_rejects_bad_per_mount_transcode_format() {
        let mut m = basic_mount("/m1");
        m.transcode = Some(TranscodeSubPatch {
            format: "flac".into(),
            sample_rate: 44_100,
            bitrate_kbps: 128,
        });
        let err = validate_mounts_patch(&MountsPatch { mounts: vec![m] }).unwrap_err();
        match err {
            WriteError::Validate { field, .. } => assert_eq!(field, "mounts[0].transcode.format"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn mounts_validate_accepts_valid_list() {
        let patch = MountsPatch {
            mounts: vec![
                MountSubPatch { name: Some("A".into()), ..basic_mount("/a") },
                MountSubPatch { name: Some("B".into()), ..basic_mount("/b") },
            ],
        };
        assert!(validate_mounts_patch(&patch).is_ok());
    }

    // ── Users ──────────────────────────────────────────────────────────────

    fn basic_user(name: &str, role: &str) -> UserSubPatch {
        UserSubPatch {
            username: name.into(),
            password_bcrypt: format!("$2y$12$bcrypt-hash-for-{name}"),
            role: role.into(),
        }
    }

    #[test]
    fn users_patch_writes_array_of_tables_under_auth() {
        let mut doc: DocumentMut = doc_without_mounts().parse().unwrap();
        apply_users_patch(
            &mut doc,
            &UsersPatch {
                users: vec![basic_user("admin", "admin"), basic_user("alice", "operator")],
            },
        );
        let out = doc.to_string();
        assert!(out.contains("[[auth.users]]"));
        assert!(out.contains(r#"username = "admin""#));
        assert!(out.contains(r#"username = "alice""#));
        assert!(out.contains(r#"role = "operator""#));
    }

    #[test]
    fn users_patch_leaves_other_auth_keys_alone() {
        let seeded = doc_without_mounts().to_string()
            + "\n[auth]\nsource_password = \"keep-me\"\n";
        let mut doc: DocumentMut = seeded.parse().unwrap();
        apply_users_patch(
            &mut doc,
            &UsersPatch { users: vec![basic_user("admin", "admin")] },
        );
        let out = doc.to_string();
        assert!(out.contains(r#"source_password = "keep-me""#), "lost source_password:\n{out}");
        assert!(out.contains("[[auth.users]]"));
    }

    #[test]
    fn users_validate_rejects_duplicate_usernames() {
        let patch = UsersPatch {
            users: vec![basic_user("admin", "admin"), basic_user("admin", "operator")],
        };
        let err = validate_users_patch(&patch).unwrap_err();
        match err {
            WriteError::Validate { field, .. } => assert_eq!(field, "users[1].username"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn users_validate_rejects_bad_role() {
        let patch = UsersPatch {
            users: vec![UserSubPatch { role: "superuser".into(), ..basic_user("admin", "admin") }],
        };
        let err = validate_users_patch(&patch).unwrap_err();
        match err {
            WriteError::Validate { field, .. } => assert_eq!(field, "users[0].role"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn users_validate_requires_at_least_one_admin() {
        let patch = UsersPatch {
            users: vec![basic_user("alice", "operator"), basic_user("bob", "operator")],
        };
        let err = validate_users_patch(&patch).unwrap_err();
        match err {
            WriteError::Validate { field, message } => {
                assert_eq!(field, "users");
                assert!(message.contains("admin"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ── AutoDJs ────────────────────────────────────────────────────────────

    fn basic_autodj(mount: &str) -> AutoDjSubPatch {
        AutoDjSubPatch {
            mount: mount.into(),
            folder: "/var/lib/rustyice/music".into(),
            enabled: true,
            loop_playlist: true,
            order: "shuffle".into(),
            name: None,
            description: None,
            genre: None,
            url: None,
            max_listeners: None,
            burst_size: None,
            transcode: TranscodeSubPatch {
                format: "mp3".into(),
                sample_rate: 44_100,
                bitrate_kbps: 128,
            },
        }
    }

    #[test]
    fn autodjs_patch_writes_block_with_required_transcode() {
        let mut doc: DocumentMut = doc_without_mounts().parse().unwrap();
        apply_autodjs_patch(
            &mut doc,
            &AutoDjsPatch { autodjs: vec![basic_autodj("/auto")] },
        );
        let out = doc.to_string();
        assert!(out.contains("[[autodjs]]"));
        assert!(out.contains(r#"mount = "/auto""#));
        assert!(out.contains(r#"folder = "/var/lib/rustyice/music""#));
        assert!(out.contains(r#"order = "shuffle""#));
        assert!(out.contains("[autodjs.transcode]"));
        assert!(out.contains(r#"format = "mp3""#));
        // enabled=true / loop=true are defaults and stay implicit.
        assert!(!out.contains("enabled = true"));
        assert!(!out.contains("loop = true"));
    }

    #[test]
    fn autodjs_patch_emits_disabled_and_no_loop_explicitly() {
        let mut doc: DocumentMut = doc_without_mounts().parse().unwrap();
        let mut a = basic_autodj("/auto");
        a.enabled = false;
        a.loop_playlist = false;
        a.order = "sequential".into();
        apply_autodjs_patch(&mut doc, &AutoDjsPatch { autodjs: vec![a] });
        let out = doc.to_string();
        assert!(out.contains("enabled = false"));
        assert!(out.contains("loop = false"));
        assert!(out.contains(r#"order = "sequential""#));
    }

    #[test]
    fn autodjs_patch_with_empty_list_removes_block() {
        let seeded = doc_without_mounts().to_string()
            + "\n[[autodjs]]\nmount = \"/old\"\nfolder = \"/tmp\"\n[autodjs.transcode]\nformat = \"mp3\"\nsample_rate = 44100\nbitrate_kbps = 128\n";
        let mut doc: DocumentMut = seeded.parse().unwrap();
        apply_autodjs_patch(&mut doc, &AutoDjsPatch { autodjs: vec![] });
        let out = doc.to_string();
        assert!(!out.contains("[[autodjs]]"));
    }

    #[test]
    fn autodjs_validate_rejects_duplicate_mount_paths() {
        let patch = AutoDjsPatch {
            autodjs: vec![basic_autodj("/dup"), basic_autodj("/dup")],
        };
        let err = validate_autodjs_patch(&patch).unwrap_err();
        match err {
            WriteError::Validate { field, .. } => assert_eq!(field, "autodjs[1].mount"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn autodjs_validate_rejects_empty_folder() {
        let mut a = basic_autodj("/a");
        a.folder = "".into();
        let err = validate_autodjs_patch(&AutoDjsPatch { autodjs: vec![a] }).unwrap_err();
        match err {
            WriteError::Validate { field, .. } => assert_eq!(field, "autodjs[0].folder"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn autodjs_validate_rejects_bad_order() {
        let mut a = basic_autodj("/a");
        a.order = "random".into();
        let err = validate_autodjs_patch(&AutoDjsPatch { autodjs: vec![a] }).unwrap_err();
        match err {
            WriteError::Validate { field, message } => {
                assert_eq!(field, "autodjs[0].order");
                assert!(message.contains("shuffle"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn autodjs_validate_rejects_bad_transcode_format() {
        let mut a = basic_autodj("/a");
        a.transcode.format = "flac".into();
        let err = validate_autodjs_patch(&AutoDjsPatch { autodjs: vec![a] }).unwrap_err();
        match err {
            WriteError::Validate { field, .. } => assert_eq!(field, "autodjs[0].transcode.format"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ── Relays ─────────────────────────────────────────────────────────────

    fn basic_relay(mount: &str) -> RelaySubPatch {
        RelaySubPatch {
            mount: mount.into(),
            upstream: "http://upstream.example.com/jazz".into(),
            enabled: true,
            name: None,
            description: None,
            genre: None,
            url: None,
            username: None,
            password: None,
            max_listeners: None,
            burst_size: None,
            transcode: None,
        }
    }

    #[test]
    fn relays_patch_writes_block() {
        let mut doc: DocumentMut = doc_without_mounts().parse().unwrap();
        apply_relays_patch(
            &mut doc,
            &RelaysPatch { relays: vec![basic_relay("/jazz")] },
        );
        let out = doc.to_string();
        assert!(out.contains("[[relays]]"));
        assert!(out.contains(r#"mount = "/jazz""#));
        assert!(out.contains(r#"upstream = "http://upstream.example.com/jazz""#));
        // enabled=true is the serde default — should NOT be emitted.
        assert!(!out.contains("enabled = true"));
    }

    #[test]
    fn relays_patch_emits_enabled_false_explicitly() {
        let mut doc: DocumentMut = doc_without_mounts().parse().unwrap();
        let mut r = basic_relay("/jazz");
        r.enabled = false;
        apply_relays_patch(&mut doc, &RelaysPatch { relays: vec![r] });
        let out = doc.to_string();
        assert!(out.contains("enabled = false"));
    }

    #[test]
    fn relays_patch_writes_credentials_and_transcode() {
        let mut doc: DocumentMut = doc_without_mounts().parse().unwrap();
        let r = RelaySubPatch {
            username: Some("relay".into()),
            password: Some("secret".into()),
            transcode: Some(TranscodeSubPatch {
                format: "vorbis".into(),
                sample_rate: 48_000,
                bitrate_kbps: 96,
            }),
            ..basic_relay("/jazz")
        };
        apply_relays_patch(&mut doc, &RelaysPatch { relays: vec![r] });
        let out = doc.to_string();
        assert!(out.contains(r#"username = "relay""#));
        assert!(out.contains(r#"password = "secret""#));
        assert!(out.contains("[relays.transcode]"));
        assert!(out.contains(r#"format = "vorbis""#));
    }

    #[test]
    fn relays_patch_with_empty_list_removes_block() {
        let seeded = doc_without_mounts().to_string()
            + "\n[[relays]]\nmount = \"/old\"\nupstream = \"http://a/b\"\n";
        let mut doc: DocumentMut = seeded.parse().unwrap();
        apply_relays_patch(&mut doc, &RelaysPatch { relays: vec![] });
        let out = doc.to_string();
        assert!(!out.contains("[[relays]]"));
    }

    #[test]
    fn relays_validate_rejects_duplicate_mount_paths() {
        let patch = RelaysPatch {
            relays: vec![basic_relay("/dup"), basic_relay("/dup")],
        };
        let err = validate_relays_patch(&patch).unwrap_err();
        match err {
            WriteError::Validate { field, message } => {
                assert_eq!(field, "relays[1].mount");
                assert!(message.contains("/dup"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn relays_validate_rejects_non_http_upstream() {
        let mut r = basic_relay("/r");
        r.upstream = "ftp://example.com/x".into();
        let err = validate_relays_patch(&RelaysPatch { relays: vec![r] }).unwrap_err();
        match err {
            WriteError::Validate { field, .. } => assert_eq!(field, "relays[0].upstream"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn relays_validate_rejects_partial_credentials() {
        let mut r = basic_relay("/r");
        r.username = Some("user".into());
        // password = None
        let err = validate_relays_patch(&RelaysPatch { relays: vec![r] }).unwrap_err();
        match err {
            WriteError::Validate { field, message } => {
                assert_eq!(field, "relays[0].password");
                assert!(message.to_lowercase().contains("password") || message.to_lowercase().contains("set"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn relays_validate_accepts_valid_list() {
        let patch = RelaysPatch {
            relays: vec![
                RelaySubPatch { name: Some("A".into()), ..basic_relay("/a") },
                RelaySubPatch { name: Some("B".into()), ..basic_relay("/b") },
            ],
        };
        assert!(validate_relays_patch(&patch).is_ok());
    }

    #[test]
    fn atomic_write_writes_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        atomic_write(&path, "first\n").unwrap();
        atomic_write(&path, "second\n").unwrap();
        let read = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read, "second\n");
        assert!(
            !dir.path().join("config.toml.tmp").exists(),
            "tmp file should be cleaned up",
        );
    }
}
