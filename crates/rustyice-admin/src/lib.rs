#![warn(clippy::pedantic)]

pub mod api;
pub mod metrics;
pub mod router;
pub mod state;

pub use router::build_admin_router;
pub use state::{AdminState, ListenerMap};
