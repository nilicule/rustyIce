use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("unsupported operation for this codec")]
    Unsupported,
    #[error("invalid encoded data: {0}")]
    InvalidData(String),
}

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("mount already has an active source")]
    MountBusy,
    #[error("ingest cancelled")]
    Cancelled,
}

#[derive(Debug, Error)]
pub enum OutputError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("listener was too slow and was disconnected")]
    SlowListener,
    #[error("output cancelled")]
    Cancelled,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("backend I/O error: {0}")]
    Io(String),
    #[error("reload failed: {0}")]
    ReloadFailed(String),
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}
