//! Shared vocabulary for every Pravera crate: identifiers, error types, media
//! descriptions and the quality profiles that steer the video pipeline.
//!
//! Nothing in here may depend on a platform, a transport or a UI toolkit — if a
//! type needs any of those, it belongs one layer up.

pub mod audio;
pub mod connect_code;
pub mod device_id;
pub mod error;
pub mod lifecycle;
pub mod media;
pub mod paths;
pub mod permission;
pub mod profile;
pub mod telemetry;

pub use audio::{AudioCodec, AudioFormat};
pub use device_id::DeviceId;
pub use error::{Error, Result};
pub use media::{Codec, FrameFormat, PixelFormat, Rect, Resolution};
pub use permission::Permission;
pub use profile::QualityProfile;
pub use telemetry::for_log;
