use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Codec not found: {0}")]
    CodecNotFound(String),

    #[error("Failed to avcodec_alloc_context3")]
    CreateContextFailed,

    #[error("Failed to avcodec_open2: {0} {1}")]
    CodecOpenError(i32, String),

    #[error("Failed to allocate frame: {0} {1}")]
    AllocateFrameFailed(i32, String),

    #[error("Failed to encode frame: {0} {1}")]
    EncodeFrameFailed(i32, String),

    #[error("Failed to receive encoded packet: {0} {1}")]
    ReceivePacketFailed(i32, String),
}
