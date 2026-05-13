#![warn(clippy::pedantic)]

pub mod mp3;
pub use mp3::{mp3_frame_size, scan_bitrate_bps, Mp3Codec};
