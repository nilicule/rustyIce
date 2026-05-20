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
        }
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
