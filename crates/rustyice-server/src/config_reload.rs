use arc_swap::ArcSwap;
use rustyice_core::{config::Config, mount::MountRegistry, traits::AuthBackend};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Watches for SIGHUP and reloads hot-reloadable config fields.
/// Runs until `shutdown` is cancelled.
pub async fn watch_sighup(
    config_path: PathBuf,
    config: Arc<ArcSwap<Config>>,
    auth: Arc<dyn AuthBackend + Send + Sync>,
    mounts: MountRegistry,
    autodjs: Arc<AutoDjRegistry>,
    shutdown: CancellationToken,
) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                error!("failed to register SIGHUP handler: {e}");
                return;
            }
        };

        loop {
            tokio::select! {
                _ = sighup.recv() => {
                    info!("SIGHUP received, reloading config from {}", config_path.display());
                    do_reload(&config_path, &config, &auth, &mounts, &autodjs, &shutdown).await;
                }
                () = shutdown.cancelled() => {
                    info!("config reload task shutting down");
                    break;
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        shutdown.cancelled().await;
    }
}

async fn do_reload(
    path: &std::path::Path,
    config: &Arc<ArcSwap<Config>>,
    auth: &Arc<dyn AuthBackend + Send + Sync>,
    mounts: &MountRegistry,
    autodjs: &Arc<AutoDjRegistry>,
    shutdown: &CancellationToken,
) {
    let new_cfg = match rustyice_core::config::load(path) {
        Ok(c) => c,
        Err(e) => {
            error!("config reload failed (keeping old config): {e}");
            return;
        }
    };

    let old = config.load();

    if old.server.stream_bind != new_cfg.server.stream_bind {
        warn!("stream_bind changed — restart required for this to take effect");
    }
    if old.server.admin_bind != new_cfg.server.admin_bind {
        warn!("admin_bind changed — restart required for this to take effect");
    }
    if old.limits.ring_size != new_cfg.limits.ring_size {
        warn!("ring_size changed — restart required for this to take effect");
    }

    if let Err(e) = auth.reload(&new_cfg).await {
        error!("auth reload error: {e}");
    }

    for mount_cfg in &new_cfg.mounts {
        if let Some(mount) = mounts.get(&mount_cfg.path) {
            let old_info = mount.info.load_full();
            let new_info = rustyice_core::mount::MountInfo {
                path: old_info.path.clone(),
                codec: old_info.codec.clone(),
                source_password: mount_cfg.source_password.clone(),
                max_listeners: mount_cfg.max_listeners,
                metadata: rustyice_core::mount::MountMetadata {
                    name: mount_cfg.name.clone(),
                    description: mount_cfg.description.clone(),
                    genre: mount_cfg.genre.clone(),
                    url: mount_cfg.url.clone(),
                },
            };
            mount.info.store(Arc::new(new_info));
        }
    }

    // ── AutoDJ diff ────────────────────────────────────────────────────────
    let old_cfg = config.load_full();
    let new_paths: std::collections::HashSet<&str> =
        new_cfg.autodjs.iter().map(|a| a.mount.as_str()).collect();

    // Removed entries: cancel + drop mount.
    for old in &old_cfg.autodjs {
        if !new_paths.contains(old.mount.as_str()) {
            autodjs.cancel(&old.mount).await;
            let _ = mounts.remove(&old.mount);
            info!(mount = %old.mount, "autodj removed via SIGHUP");
        }
    }

    // New entries, and respawn-required changes.
    for new in &new_cfg.autodjs {
        let old = old_cfg.autodjs.iter().find(|a| a.mount == new.mount);
        let needs_respawn = match old {
            None => new.enabled,
            Some(o) => {
                o.enabled != new.enabled
                    || o.folder != new.folder
                    || o.loop_playlist != new.loop_playlist
                    || o.order != new.order
                    || o.transcode != new.transcode
                    || o.burst_size != new.burst_size
            }
        };

        if needs_respawn {
            autodjs.cancel(&new.mount).await;

            // Ensure mount exists; if not, register it now (new entry).
            if mounts.get(&new.mount).is_none() {
                let bus = Arc::new(crate::bus::TokioBroadcastBus::new(
                    new_cfg.limits.ring_size,
                    new.burst_size.unwrap_or(new_cfg.limits.burst_size) as usize,
                ));
                let codec_seed = match new.transcode.format {
                    rustyice_core::config::TranscodeFormat::Mp3 => {
                        rustyice_core::types::CodecId::MP3
                    }
                    rustyice_core::config::TranscodeFormat::Vorbis => {
                        rustyice_core::types::CodecId::VORBIS
                    }
                };
                mounts.add(Arc::new(rustyice_core::mount::ActiveMount::new(
                    rustyice_core::mount::MountInfo {
                        path: new.mount.clone(),
                        codec: codec_seed,
                        source_password: String::new(),
                        max_listeners: new.max_listeners,
                        metadata: rustyice_core::mount::MountMetadata {
                            name: new.name.clone(),
                            description: new.description.clone(),
                            genre: new.genre.clone(),
                            url: new.url.clone(),
                        },
                    },
                    bus,
                )));
            }

            if new.enabled {
                if let Some(mount) = mounts.get(&new.mount) {
                    let cancel = shutdown.child_token();
                    let player =
                        rustyice_autodj::AutoDjPlayer::from_config(new, mount, cancel.clone());
                    let handle = player.spawn();
                    autodjs.insert(new.clone(), cancel, handle).await;
                    info!(mount = %new.mount, "autodj spawned via SIGHUP");
                }
            } else {
                info!(mount = %new.mount, "autodj disabled via SIGHUP");
            }
        } else {
            // Metadata-only update: live-swap MountInfo.
            if let Some(mount) = mounts.get(&new.mount) {
                let old_info = mount.info.load_full();
                let new_info = rustyice_core::mount::MountInfo {
                    path: old_info.path.clone(),
                    codec: old_info.codec.clone(),
                    source_password: old_info.source_password.clone(),
                    max_listeners: new.max_listeners,
                    metadata: rustyice_core::mount::MountMetadata {
                        name: new.name.clone(),
                        description: new.description.clone(),
                        genre: new.genre.clone(),
                        url: new.url.clone(),
                    },
                };
                mount.info.store(Arc::new(new_info));
            }
        }
    }

    config.store(Arc::new(new_cfg));
    info!("config reloaded successfully");
}

use rustyice_core::config::AutoDjConfig;
use std::collections::HashMap;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

/// Live set of running AutoDJ tasks, keyed by mount path. Shared between
/// `main` (initial spawn) and the SIGHUP reloader (diff + respawn).
#[derive(Default)]
pub struct AutoDjRegistry {
    inner: AsyncMutex<HashMap<String, AutoDjEntry>>,
}

struct AutoDjEntry {
    #[allow(dead_code)]
    cfg: AutoDjConfig,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl AutoDjRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(
        &self,
        cfg: AutoDjConfig,
        cancel: CancellationToken,
        handle: JoinHandle<()>,
    ) {
        let mut g = self.inner.lock().await;
        g.insert(cfg.mount.clone(), AutoDjEntry { cfg, cancel, handle });
    }

    pub async fn cancel(&self, mount: &str) {
        let entry = { self.inner.lock().await.remove(mount) };
        if let Some(e) = entry {
            e.cancel.cancel();
            let _ = e.handle.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyice_core::{
        config::{AuthConfig, LimitsConfig, LogFormat, LoggingConfig, MountConfig, ServerConfig},
        mount::{ActiveMount, MountInfo, MountMetadata},
        traits::BroadcastBus,
        types::{CodecId, StreamPacket},
    };
    use std::{pin::Pin, sync::Arc};

    struct NullBus;
    impl BroadcastBus for NullBus {
        fn publish(&self, _: Arc<StreamPacket>) {}
        fn subscribe(
            &self,
        ) -> Pin<Box<dyn futures::Stream<Item = Arc<StreamPacket>> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
        fn subscriber_count(&self) -> usize {
            0
        }
    }

    fn base_config() -> Config {
        Config {
            server: ServerConfig {
                stream_bind: "0.0.0.0:8000".parse().unwrap(),
                admin_bind: "127.0.0.1:8001".parse().unwrap(),
                hostname: "localhost".to_string(),
            },
            logging: LoggingConfig { level: "info".to_string(), format: LogFormat::Json },
            auth: AuthConfig { users: vec![], source_password: None },
            limits: LimitsConfig {
                max_listeners_global: 500,
                ring_size: 64,
                slow_listener_grace_s: 2,
                source_max_kbps: None,
                burst_size: 65_536,
            },
            mounts: vec![MountConfig {
                path: "/stream".to_string(),
                source_password: "oldpw".to_string(),
                max_listeners: None,
                name: Some("Old Name".to_string()),
                description: None,
                genre: None,
                url: None,
                burst_size: None,
                transcode: None,
            }],
            tls: None,
            transcode: None,
            autodjs: vec![],
        }
    }

    #[tokio::test]
    async fn reload_updates_mount_metadata() {
        let _config = Arc::new(ArcSwap::from_pointee(base_config()));
        let registry = MountRegistry::new();

        registry.add(Arc::new(ActiveMount::new(
            MountInfo {
                path: "/stream".to_string(),
                codec: CodecId::MP3,
                source_password: "oldpw".to_string(),
                max_listeners: None,
                metadata: MountMetadata {
                    name: Some("Old Name".to_string()),
                    ..Default::default()
                },
            },
            Arc::new(NullBus),
        )));

        let mut new_cfg = base_config();
        new_cfg.mounts[0].source_password = "newpw".to_string();
        new_cfg.mounts[0].name = Some("New Name".to_string());

        let mount = registry.get("/stream").unwrap();
        assert_eq!(mount.info.load().metadata.name.as_deref(), Some("Old Name"));

        let mount_cfg = &new_cfg.mounts[0];
        let old_info = mount.info.load_full();
        let new_info = MountInfo {
            path: old_info.path.clone(),
            codec: old_info.codec.clone(),
            source_password: mount_cfg.source_password.clone(),
            max_listeners: mount_cfg.max_listeners,
            metadata: MountMetadata {
                name: mount_cfg.name.clone(),
                description: mount_cfg.description.clone(),
                genre: mount_cfg.genre.clone(),
                url: mount_cfg.url.clone(),
            },
        };
        mount.info.store(Arc::new(new_info));

        assert_eq!(mount.info.load().source_password, "newpw");
        assert_eq!(mount.info.load().metadata.name.as_deref(), Some("New Name"));
    }

    #[tokio::test]
    async fn reload_preserves_current_title() {
        let _config = Arc::new(ArcSwap::from_pointee(base_config()));
        let registry = MountRegistry::new();

        registry.add(Arc::new(ActiveMount::new(
            MountInfo {
                path: "/stream".to_string(),
                codec: CodecId::MP3,
                source_password: "oldpw".to_string(),
                max_listeners: None,
                metadata: MountMetadata {
                    name: Some("Old Name".to_string()),
                    ..Default::default()
                },
            },
            Arc::new(NullBus),
        )));

        // Admin sets a runtime title before reload happens.
        let mount = registry.get("/stream").unwrap();
        mount.current_title.store(Arc::new(Some("Now Playing".to_string())));

        // Mimic the body of `do_reload` for the single mount: build a new
        // MountInfo and store it.
        let mut new_cfg = base_config();
        new_cfg.mounts[0].name = Some("Reloaded Name".to_string());
        let mount_cfg = &new_cfg.mounts[0];
        let old_info = mount.info.load_full();
        let new_info = MountInfo {
            path: old_info.path.clone(),
            codec: old_info.codec.clone(),
            source_password: mount_cfg.source_password.clone(),
            max_listeners: mount_cfg.max_listeners,
            metadata: MountMetadata {
                name: mount_cfg.name.clone(),
                description: mount_cfg.description.clone(),
                genre: mount_cfg.genre.clone(),
                url: mount_cfg.url.clone(),
            },
        };
        mount.info.store(Arc::new(new_info));

        // Title survives.
        let snap = mount.current_title.load();
        assert_eq!(snap.as_deref(), Some("Now Playing"));
        // Name was reloaded.
        assert_eq!(mount.info.load().metadata.name.as_deref(), Some("Reloaded Name"));
    }
}
