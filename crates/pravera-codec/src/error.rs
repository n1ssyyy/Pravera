//! What can go wrong while compressing or decompressing a frame.

use pravera_core::{Codec, PixelFormat, Resolution};

pub type Result<T, E = CodecError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// This build cannot encode or decode that codec. Distinct from a failure:
    /// it is the answer to "can you", not "did you".
    #[error("{} is not available in this build", .0.name())]
    Unsupported(Codec),

    #[error("cannot encode {resolution}: {reason}")]
    BadDimensions {
        resolution: Resolution,
        reason: &'static str,
    },

    /// The encoder was handed pixels in a layout it does not read.
    #[error("{0:?} is not an encoder input format")]
    UnsupportedInput(PixelFormat),

    /// A frame whose buffer does not match the geometry it claims. Caught at
    /// the boundary because every conversion below this point indexes that
    /// buffer arithmetically.
    #[error(
        "frame buffer is {actual} bytes, but {resolution} at stride {stride} needs {expected}"
    )]
    Malformed {
        resolution: Resolution,
        stride: usize,
        expected: usize,
        actual: usize,
    },

    #[error("encoder: {0}")]
    Encode(String),

    #[error("decoder: {0}")]
    Decode(String),

    #[error("could not start the {codec} codec: {reason}")]
    Init { codec: &'static str, reason: String },
}

impl From<CodecError> for pravera_core::Error {
    fn from(error: CodecError) -> Self {
        pravera_core::Error::Codec(error.to_string())
    }
}
