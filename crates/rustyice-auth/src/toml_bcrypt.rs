use arc_swap::ArcSwap;
use async_trait::async_trait;
use rustyice_core::config::Config;
use rustyice_core::error::AuthError;
use rustyice_core::traits::AuthBackend;
use std::collections::HashMap;
use std::sync::Arc;

pub struct TomlBcryptAuth {
    /// username → bcrypt hash. Hot-reloadable.
    users: Arc<ArcSwap<HashMap<String, String>>>,
    /// mount path → plaintext source password. Hot-reloadable.
    mount_passwords: Arc<ArcSwap<HashMap<String, String>>>,
    /// Optional global source password. When set, any source supplying it may
    /// create a dynamic mount. Hot-reloadable. `None` when no `auth.source_password`
    /// is configured — `verify_default_source` then rejects every attempt.
    default_source_password: Arc<ArcSwap<Option<String>>>,
}

impl TomlBcryptAuth {
    #[must_use]
    pub fn new(config: &Config) -> Self {
        Self {
            users: Arc::new(ArcSwap::from_pointee(user_map(config))),
            mount_passwords: Arc::new(ArcSwap::from_pointee(mount_map(config))),
            default_source_password: Arc::new(ArcSwap::from_pointee(
                config.auth.source_password.clone(),
            )),
        }
    }
}

fn user_map(config: &Config) -> HashMap<String, String> {
    config
        .auth
        .users
        .iter()
        .map(|u| (u.username.clone(), u.password_bcrypt.clone()))
        .collect()
}

fn mount_map(config: &Config) -> HashMap<String, String> {
    config
        .mounts
        .iter()
        .map(|m| (m.path.clone(), m.source_password.clone()))
        .collect()
}

#[async_trait]
impl AuthBackend for TomlBcryptAuth {
    async fn verify_admin(&self, username: &str, password: &str) -> Result<bool, AuthError> {
        let users = self.users.load_full();
        let Some(hash) = users.get(username).cloned() else {
            return Ok(false);
        };
        let password = password.to_owned();
        tokio::task::spawn_blocking(move || {
            bcrypt::verify(&password, &hash).map_err(|e| AuthError::Io(e.to_string()))
        })
        .await
        .map_err(|e| AuthError::Io(e.to_string()))?
    }

    async fn verify_source(&self, mount_path: &str, password: &str) -> Result<bool, AuthError> {
        let passwords = self.mount_passwords.load();
        Ok(passwords.get(mount_path).is_some_and(|p| p == password))
    }

    async fn verify_default_source(&self, password: &str) -> Result<bool, AuthError> {
        let configured = self.default_source_password.load();
        Ok(configured.as_deref().is_some_and(|p| p == password))
    }

    async fn reload(&self, config: &Config) -> Result<(), AuthError> {
        self.users.store(Arc::new(user_map(config)));
        self.mount_passwords.store(Arc::new(mount_map(config)));
        self.default_source_password
            .store(Arc::new(config.auth.source_password.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyice_core::config::{
        AuthConfig, Config, LimitsConfig, LogFormat, LoggingConfig, MountConfig, ServerConfig,
        UserConfig,
    };

    fn make_config(users: &[(&str, &str)], mounts: &[(&str, &str)]) -> Config {
        Config {
            server: ServerConfig {
                stream_bind: "0.0.0.0:8000".parse().unwrap(),
                admin_bind: "127.0.0.1:8001".parse().unwrap(),
                hostname: "localhost".to_string(),
            },
            logging: LoggingConfig { level: "info".to_string(), format: LogFormat::Json },
            auth: AuthConfig {
                users: users
                    .iter()
                    .map(|(u, h)| UserConfig {
                        username: u.to_string(),
                        password_bcrypt: h.to_string(),
                    })
                    .collect(),
                source_password: None,
            },
            limits: LimitsConfig {
                max_listeners_global: 500,
                ring_size: 64,
                slow_listener_grace_s: 2,
                source_max_kbps: None,
                burst_size: 65_536,
            },
            mounts: mounts
                .iter()
                .map(|(path, pw)| MountConfig {
                    path: path.to_string(),
                    source_password: pw.to_string(),
                    max_listeners: None,
                    name: None,
                    description: None,
                    genre: None,
                    url: None,
                    burst_size: None,
                    transcode: None,
                })
                .collect(),
            tls: None,
            transcode: None,
        }
    }

    #[tokio::test]
    async fn verify_admin_correct_password() {
        let hash = bcrypt::hash("testpass", 4).unwrap();
        let auth = TomlBcryptAuth::new(&make_config(&[("admin", &hash)], &[]));
        assert!(auth.verify_admin("admin", "testpass").await.unwrap());
    }

    #[tokio::test]
    async fn verify_admin_wrong_password() {
        let hash = bcrypt::hash("correct", 4).unwrap();
        let auth = TomlBcryptAuth::new(&make_config(&[("admin", &hash)], &[]));
        assert!(!auth.verify_admin("admin", "wrong").await.unwrap());
    }

    #[tokio::test]
    async fn verify_admin_unknown_user() {
        let auth = TomlBcryptAuth::new(&make_config(&[], &[]));
        assert!(!auth.verify_admin("nobody", "pass").await.unwrap());
    }

    #[tokio::test]
    async fn verify_source_correct_password() {
        let auth = TomlBcryptAuth::new(&make_config(&[], &[("/stream", "secret")]));
        assert!(auth.verify_source("/stream", "secret").await.unwrap());
    }

    #[tokio::test]
    async fn verify_source_wrong_password() {
        let auth = TomlBcryptAuth::new(&make_config(&[], &[("/stream", "secret")]));
        assert!(!auth.verify_source("/stream", "wrong").await.unwrap());
    }

    #[tokio::test]
    async fn verify_source_unknown_mount() {
        let auth = TomlBcryptAuth::new(&make_config(&[], &[]));
        assert!(!auth.verify_source("/nothere", "anything").await.unwrap());
    }

    fn make_config_with_default_source(default_pw: Option<&str>) -> Config {
        let mut cfg = make_config(&[], &[]);
        cfg.auth.source_password = default_pw.map(str::to_string);
        cfg
    }

    #[tokio::test]
    async fn verify_default_source_rejects_when_unset() {
        let auth = TomlBcryptAuth::new(&make_config(&[], &[]));
        assert!(!auth.verify_default_source("anything").await.unwrap());
    }

    #[tokio::test]
    async fn verify_default_source_accepts_match() {
        let auth = TomlBcryptAuth::new(&make_config_with_default_source(Some("global")));
        assert!(auth.verify_default_source("global").await.unwrap());
    }

    #[tokio::test]
    async fn verify_default_source_rejects_mismatch() {
        let auth = TomlBcryptAuth::new(&make_config_with_default_source(Some("global")));
        assert!(!auth.verify_default_source("wrong").await.unwrap());
    }

    #[tokio::test]
    async fn reload_updates_default_source_password() {
        let auth = TomlBcryptAuth::new(&make_config_with_default_source(Some("old")));
        assert!(auth.verify_default_source("old").await.unwrap());
        auth.reload(&make_config_with_default_source(Some("new")))
            .await
            .unwrap();
        assert!(!auth.verify_default_source("old").await.unwrap());
        assert!(auth.verify_default_source("new").await.unwrap());
    }

    #[tokio::test]
    async fn reload_updates_credentials() {
        let old_hash = bcrypt::hash("old", 4).unwrap();
        let auth = TomlBcryptAuth::new(&make_config(&[("admin", &old_hash)], &[("/stream", "oldpw")]));
        assert!(auth.verify_admin("admin", "old").await.unwrap());
        assert!(auth.verify_source("/stream", "oldpw").await.unwrap());

        let new_hash = bcrypt::hash("new", 4).unwrap();
        let new_config = make_config(&[("admin", &new_hash)], &[("/stream", "newpw")]);
        auth.reload(&new_config).await.unwrap();

        assert!(!auth.verify_admin("admin", "old").await.unwrap());
        assert!(auth.verify_admin("admin", "new").await.unwrap());
        assert!(!auth.verify_source("/stream", "oldpw").await.unwrap());
        assert!(auth.verify_source("/stream", "newpw").await.unwrap());
    }
}
