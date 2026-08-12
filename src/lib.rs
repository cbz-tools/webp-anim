#![warn(missing_docs)]

//! Inspect, decode, resize, and encode animated WebP images.
//!
//! The crate exposes lightweight frame-duration metadata inspection, sequential
//! full-canvas RGBA decoding, reusable animation-frame resizing, and sequential
//! animated-WebP encoding. [`AnimationDecoder::frame_durations`] reads stored
//! durations without decoding pixels or advancing the decoder, while
//! [`AnimationDecoder::reset`] reuses a decoder from the start of its sequence.
//! Frame durations are passed through without playback-time normalization.
//!
//! The high-level [`transcode_animated_webp`] function composes those stages for
//! one stored animation sequence. Applications that need different resource,
//! compression, or output-selection policies can use the lower-level types
//! directly.

/// Lower-level WebP animation decoder and encoder primitives.
pub mod codec;
/// WebP container classification and metadata inspection.
pub mod inspect;
/// Shared data types describing canvases, frames, and animation metadata.
pub mod model;
/// Reusable aspect-ratio-preserving RGBA resize plans and workspaces.
pub mod resize;
/// The sequential decode-resize-encode convenience pipeline.
pub mod transcode;

pub use codec::decode::{AnimationDecoder, DecodeError, DecodeLimits};
pub use codec::encode::{
    AnimationEncoder, AnimationEncoderOptions, AnimationMuxOverrides, EncodeError,
    EncoderConfigOverrides,
};
pub use inspect::{InspectError, InspectLimits, WebpKind, inspect, is_animated_webp_fast};
pub use model::{
    AnimationFrame, AnimationInfo, BackgroundColor, CanvasSize, LoopCount, StaticWebpInfo,
};
pub use resize::{ResizeError, ResizeFilter, ResizeOptions, ResizePlan, ResizeWorkspace};
pub use transcode::{
    AnimationTranscodeOptions, TranscodeError, TranscodedAnimation, transcode_animated_webp,
};
