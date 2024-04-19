use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Codec not found: {0}")]
    CodecNotFound(String),

    #[error("Failed to avcodec_alloc_context3")]
    CreateContextFailed,

    #[error("Failed to avcodec_open2: {0}")]
    CodecOpenError(i32),

    #[error("Failed to allocate frame: {0}")]
    AllocateFrameFailed(i32),

    #[error("Failed to encoder frame: {0}")]
    EncodeFrameFailed(i32),

    #[error("Failed to receive encoded packet: {0}")]
    ReceivePacketFailed(i32),
}
