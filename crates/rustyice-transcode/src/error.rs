#[derive(Debug, thiserror::Error)]
pub enum TranscodeError {
    #[error("decoder init failed: {0}")]
    DecoderInit(String),
    #[error("encoder init failed: {0}")]
    EncoderInit(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("encode error: {0}")]
    Encode(String),
    #[error("resample error: {0}")]
    Resample(String),
}
