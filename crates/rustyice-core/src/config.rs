use std::net::SocketAddr;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    pub logging: LoggingConfig,
    pub auth: AuthConfig,
    pub limits: LimitsConfig,
    #[serde(default)]
    pub mounts: Vec<MountConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<TranscodeConfig>,
}

impl Config {
    /// Returns the effective transcode config for `mount`: per-mount takes precedence over global.
    #[must_use]
    pub fn effective_transcode<'a>(&'a self, mount: &'a MountConfig) -> Option<&'a TranscodeConfig> {
        mount.transcode.as_ref().or(self.transcode.as_ref())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub stream_bind: SocketAddr,
    pub admin_bind: SocketAddr,
    pub hostname: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Pretty,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AuthConfig {
    #[serde(default)]
    pub users: Vec<UserConfig>,
    /// If set, any source authenticating with this password may create a new
    /// mount on a path not present in `[[mounts]]`. The mount lives for the
    /// duration of the source connection and is removed when the source
    /// disconnects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_password: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserConfig {
    pub username: String,
    pub password_bcrypt: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LimitsConfig {
    pub max_listeners_global: u32,
    pub ring_size: usize,
    pub slow_listener_grace_s: u64,
    /// Cap source ingestion rate (kbits/sec). Unset = unlimited.
    /// Useful when pushing files: limits reads so TCP backpressure slows the
    /// sender to real-time. Live source clients (Liquidsoap, Butt) send at
    /// their own bitrate and are unaffected as long as they stay below the cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_max_kbps: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MountConfig {
    pub path: String,
    pub source_password: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_listeners: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcode: Option<TranscodeConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TranscodeFormat {
    Mp3,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TranscodeConfig {
    pub format: TranscodeFormat,
    pub sample_rate: u32,
    pub bitrate_kbps: u32,
}

/// Reserved for v2 ACME / Let's Encrypt support. Parsed but unused in v1.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TlsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acme_domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_cache_dir: Option<std::path::PathBuf>,
}

/// Load and parse the config file at the given path.
///
/// # Errors
/// Returns [`crate::error::ConfigError`] if the file cannot be read or parsed.
pub fn load(path: &std::path::Path) -> Result<Config, crate::error::ConfigError> {
    let contents = std::fs::read_to_string(path)?;
    parse_str(&contents)
}

/// Parse a TOML config from a string.
///
/// # Errors
/// Returns [`crate::error::ConfigError`] if the TOML cannot be parsed.
pub fn parse_str(toml_text: &str) -> Result<Config, crate::error::ConfigError> {
    let config = toml::from_str(toml_text)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_CONFIG: &str = r#"
[server]
stream_bind = "0.0.0.0:8000"
admin_bind  = "127.0.0.1:8001"
hostname    = "localhost"

[logging]
level  = "info"
format = "json"

[auth]
[[auth.users]]
username        = "admin"
password_bcrypt = "$2b$12$placeholder000000000000000000000"

[limits]
max_listeners_global  = 500
ring_size             = 64
slow_listener_grace_s = 2

[transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 128

[[mounts]]
path            = "/stream"
source_password = "hackme"
max_listeners   = 100
name            = "My Radio"
description     = "The best radio"
genre           = "Electronic"
url             = "https://example.com"

[mounts.transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 192
"#;

    const MINIMAL_CONFIG: &str = r#"
[server]
stream_bind = "0.0.0.0:8000"
admin_bind  = "127.0.0.1:8001"
hostname    = "localhost"

[logging]
level  = "info"
format = "pretty"

[auth]

[limits]
max_listeners_global  = 100
ring_size             = 32
slow_listener_grace_s = 5
"#;

    #[test]
    fn parses_full_config() {
        let cfg: Config = toml::from_str(FULL_CONFIG).unwrap();
        assert_eq!(cfg.server.hostname, "localhost");
        assert_eq!(cfg.auth.users.len(), 1);
        assert_eq!(cfg.auth.users[0].username, "admin");
        assert_eq!(cfg.mounts.len(), 1);
        assert_eq!(cfg.mounts[0].path, "/stream");
        assert_eq!(cfg.mounts[0].max_listeners, Some(100));
        assert_eq!(cfg.mounts[0].name.as_deref(), Some("My Radio"));
    }

    #[test]
    fn parses_minimal_config_no_mounts() {
        let cfg: Config = toml::from_str(MINIMAL_CONFIG).unwrap();
        assert!(cfg.mounts.is_empty());
        assert!(cfg.auth.users.is_empty());
        assert_eq!(cfg.limits.ring_size, 32);
    }

    #[test]
    fn mount_optional_fields_default_to_none() {
        let src = r#"
[server]
stream_bind = "0.0.0.0:8000"
admin_bind  = "127.0.0.1:8001"
hostname    = "localhost"
[logging]
level = "info"
format = "json"
[auth]
[limits]
max_listeners_global  = 500
ring_size             = 64
slow_listener_grace_s = 2
[[mounts]]
path            = "/radio"
source_password = "secret"
"#;
        let cfg: Config = toml::from_str(src).unwrap();
        assert!(cfg.mounts[0].name.is_none());
        assert!(cfg.mounts[0].description.is_none());
        assert!(cfg.mounts[0].max_listeners.is_none());
    }

    #[test]
    fn log_format_json_and_pretty() {
        let cfg: Config = toml::from_str(FULL_CONFIG).unwrap();
        assert_eq!(cfg.logging.format, LogFormat::Json);
        let cfg2: Config = toml::from_str(MINIMAL_CONFIG).unwrap();
        assert_eq!(cfg2.logging.format, LogFormat::Pretty);
    }

    #[test]
    fn stream_bind_parses_as_socket_addr() {
        let cfg: Config = toml::from_str(FULL_CONFIG).unwrap();
        assert_eq!(cfg.server.stream_bind.port(), 8000);
        assert_eq!(cfg.server.admin_bind.port(), 8001);
        assert!(cfg.server.admin_bind.ip().is_loopback());
    }

    #[test]
    fn config_round_trips_through_toml() {
        let cfg: Config = toml::from_str(FULL_CONFIG).unwrap();
        let serialized = toml::to_string(&cfg).unwrap();
        let cfg2: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg.server.hostname, cfg2.server.hostname);
        assert_eq!(cfg.mounts[0].path, cfg2.mounts[0].path);
    }

    const BASE_CONFIG: &str = r#"
[server]
stream_bind = "0.0.0.0:8000"
admin_bind  = "127.0.0.1:8001"
hostname    = "localhost"

[logging]
level  = "info"
format = "json"

[auth]

[limits]
max_listeners_global  = 500
ring_size             = 64
slow_listener_grace_s = 2
"#;

    #[test]
    fn transcode_config_parses_global() {
        let src = format!(
            r#"{}
[transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 128
"#,
            BASE_CONFIG
        );
        let cfg: Config = toml::from_str(&src).unwrap();
        let tc = cfg.transcode.as_ref().unwrap();
        assert_eq!(tc.format, TranscodeFormat::Mp3);
        assert_eq!(tc.sample_rate, 44100);
        assert_eq!(tc.bitrate_kbps, 128);
    }

    #[test]
    fn transcode_config_per_mount_overrides_global() {
        let src = format!(
            r#"{}
[transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 128

[[mounts]]
path            = "/stream"
source_password = "secret"

[mounts.transcode]
format       = "mp3"
sample_rate  = 22050
bitrate_kbps = 64
"#,
            BASE_CONFIG
        );
        let cfg: Config = toml::from_str(&src).unwrap();
        let effective = cfg.effective_transcode(&cfg.mounts[0]).unwrap();
        assert_eq!(effective.sample_rate, 22050);
        assert_eq!(effective.bitrate_kbps, 64);
    }

    #[test]
    fn transcode_config_falls_back_to_global() {
        let src = format!(
            r#"{}
[transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 128

[[mounts]]
path            = "/stream"
source_password = "secret"
"#,
            BASE_CONFIG
        );
        let cfg: Config = toml::from_str(&src).unwrap();
        let effective = cfg.effective_transcode(&cfg.mounts[0]).unwrap();
        assert_eq!(effective.sample_rate, 44100);
        assert_eq!(effective.bitrate_kbps, 128);
    }

    #[test]
    fn transcode_config_none_when_absent() {
        let src = format!(
            r#"{}
[[mounts]]
path            = "/stream"
source_password = "secret"
"#,
            BASE_CONFIG
        );
        let cfg: Config = toml::from_str(&src).unwrap();
        assert!(cfg.effective_transcode(&cfg.mounts[0]).is_none());
    }
}
