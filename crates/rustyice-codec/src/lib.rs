#![warn(clippy::pedantic)]

pub mod mp3;
pub use mp3::{scan_bitrate_bps, Mp3Codec};
