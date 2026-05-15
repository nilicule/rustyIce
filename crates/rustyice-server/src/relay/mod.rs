//! Outbound-pull relay mounts. Each `[[relays]]` entry in the config gets a
//! tokio task that connects to an upstream Icecast-compatible URL via HTTP
//! GET and re-broadcasts the bytes to local listeners through the existing
//! `BroadcastBus` + `IcecastIngest` pipeline.

pub mod backoff;

pub use backoff::Backoff;
