mod decoder;
mod encoder;
mod error;
mod pipeline;
mod resampler;
mod vorbis_encoder;

pub use decoder::StreamDecoder;
pub use encoder::{Encoder, LameEncoder};
pub use error::TranscodeError;
pub use pipeline::TranscodePipeline;
pub use vorbis_encoder::VorbisEncoder;

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<TranscodePipeline>();
};
