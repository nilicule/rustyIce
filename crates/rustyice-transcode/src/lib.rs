mod decoder;
mod encoder;
mod error;
mod pipeline;
mod resampler;

pub use encoder::LameEncoder;
pub use error::TranscodeError;
pub use pipeline::TranscodePipeline;

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<TranscodePipeline>();
};
