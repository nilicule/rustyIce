#![warn(clippy::pedantic)]

pub mod mp3;
pub mod vorbis;
pub use mp3::{mp3_frame_size, scan_bitrate_bps, Mp3Codec};
pub use vorbis::{ogg_page_size, scan_nominal_bitrate_bps, OggHeaderCapture, VorbisCodec};
