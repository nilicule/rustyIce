use async_trait::async_trait;
use rustyice_core::config::Config;
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
