use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Codec not found: {0}")]
    CodecNotFound(String),

    #[error("Codec is not an encoder: {0}")]
    CodecIsNotEncoder(&'static str),

    #[error("Codec is not a decoder: {0}")]
    CodecIsNotDecoder(&'static str),

    #[error("Failed to avcodec_alloc_context3")]
    CreateContextFailed,

    #[error("Failed to avcodec_open2: {0} {1}")]
    CodecOpenError(i32, String),

    #[error("Failed to allocate frame: {0} {1}")]
    AllocateFrameFailed(i32, String),

    #[error("Failed to encode frame: {0} {1}")]
    EncodeFrameFailed(i32, String),

    #[error("Failed to decode packet: {0} {1}")]
    DecodePacketFailed(i32, String),

    #[error("Failed to receive encoded packet: {0} {1}")]
    ReceivePacketFailed(i32, String),

    #[error("Failed to receive decoded frame: {0} {1}")]
    ReceiveFrameFailed(i32, String),

    #[error("Failed to allocate memory: {0}")]
    AlllocateFailed(&'static str),
}
