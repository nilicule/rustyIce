#![warn(clippy::pedantic)]

pub mod decode;
pub mod playlist;
pub mod tags;

#[cfg(feature = "test-fixtures")]
pub mod test_fixtures;

pub use decode::{FileDecoder, PcmChunk};
pub use playlist::{Order, scan};
pub use tags::display_title;
