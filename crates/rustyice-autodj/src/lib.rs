#![warn(clippy::pedantic)]

pub mod playlist;
pub mod tags;

#[cfg(feature = "test-fixtures")]
pub mod test_fixtures;

pub use playlist::{Order, scan};
pub use tags::display_title;
