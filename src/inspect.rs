use std::{error::Error, fmt};

use libwebp_sys::WebPGetInfo;

use crate::{
    codec::decode::{AnimationDecoder, DecodeError, DecodeLimits},
    model::{CanvasSize, StaticWebpInfo},
};

const RIFF_HEADER_LEN: usize = 12;
const RIFF_CHUNK_HEADER_LEN: usize = 8;
const WEBP_ANIMATION_FLAG: u8 = 0x02;

/// Resource limits used while classifying a WebP container and reading its
/// animation metadata.
///
/// The [`Default`] values mirror [`DecodeLimits::default`]. Metadata inspection
/// does not decode the complete frame sequence, but animated inputs are still
/// checked against the canvas, frame-count, and per-frame RGBA limits.
#[derive(Clone, Debug)]
pub struct InspectLimits {
    /// Maximum number of input bytes accepted by [`inspect`].
    pub max_input_bytes: usize,
    /// Maximum number of pixels in the image canvas.
    pub max_canvas_pixels: u64,
    /// Maximum number of frames reported for an animated input.
    pub max_frame_count: u32,
    /// Maximum number of bytes in one full-canvas RGBA frame.
    pub max_frame_rgba_bytes: usize,
}

impl Default for InspectLimits {
    fn default() -> Self {
        let decode = DecodeLimits::default();
        Self {
            max_input_bytes: decode.max_input_bytes,
            max_canvas_pixels: decode.max_canvas_pixels,
            max_frame_count: decode.max_frame_count,
            max_frame_rgba_bytes: decode.max_frame_rgba_bytes,
        }
    }
}

impl InspectLimits {
    /// Relaxes crate-level metadata inspection limits for trusted input.
    ///
    /// This does not bypass libwebp or platform allocation limits.
    pub const fn for_trusted_input() -> Self {
        Self {
            max_input_bytes: usize::MAX,
            max_canvas_pixels: u64::MAX,
            max_frame_count: u32::MAX,
            max_frame_rgba_bytes: usize::MAX,
        }
    }
}

/// The kind and metadata of a valid WebP image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebpKind {
    /// A non-animated WebP image and its canvas dimensions.
    Static(StaticWebpInfo),
    /// One stored animated WebP sequence and its metadata.
    Animated(crate::model::AnimationInfo),
}

/// Failure while classifying a WebP container or reading its metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InspectError {
    /// The input is larger than the configured byte limit.
    InputTooLarge {
        /// Actual input length in bytes.
        actual: usize,
        /// Configured maximum input length in bytes.
        maximum: usize,
    },
    /// The input does not begin with a RIFF/WebP container header.
    InvalidContainer,
    /// The RIFF/WebP container ends before its declared chunks do.
    TruncatedContainer,
    /// libwebp rejected the image payload as invalid WebP data.
    InvalidWebp,
    /// A configured inspection limit was exceeded.
    LimitExceeded {
        /// Name of the exceeded limit.
        limit: &'static str,
        /// Observed value.
        actual: u64,
        /// Configured maximum value.
        maximum: u64,
    },
    /// Animated metadata inspection failed while constructing a decoder.
    Animation(DecodeError),
}

impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { actual, maximum } => {
                write!(
                    f,
                    "input is {actual} bytes, exceeding the {maximum}-byte limit"
                )
            }
            Self::InvalidContainer => f.write_str("input is not a RIFF/WebP container"),
            Self::TruncatedContainer => {
                f.write_str("WebP RIFF container is truncated or malformed")
            }
            Self::InvalidWebp => f.write_str("input is not a valid WebP image"),
            Self::LimitExceeded {
                limit,
                actual,
                maximum,
            } => {
                write!(f, "{limit} is {actual}, exceeding the {maximum} limit")
            }
            Self::Animation(error) => write!(f, "failed to inspect animated WebP: {error}"),
        }
    }
}

impl Error for InspectError {}

/// Inspects container kind and metadata without decoding its complete frame sequence.
pub fn inspect(input: &[u8], limits: InspectLimits) -> Result<WebpKind, InspectError> {
    if input.len() > limits.max_input_bytes {
        return Err(InspectError::InputTooLarge {
            actual: input.len(),
            maximum: limits.max_input_bytes,
        });
    }
    let animated = animation_flag(input)?;

    let mut width = 0_i32;
    let mut height = 0_i32;
    // SAFETY: `input` stays live for the call and the width/height pointers are writable.
    if unsafe { WebPGetInfo(input.as_ptr(), input.len(), &mut width, &mut height) } == 0 {
        return Err(InspectError::InvalidWebp);
    }
    let canvas = CanvasSize {
        width: u32::try_from(width).map_err(|_| InspectError::InvalidWebp)?,
        height: u32::try_from(height).map_err(|_| InspectError::InvalidWebp)?,
    };
    enforce_canvas_limit(canvas, limits.max_canvas_pixels)?;

    if !animated {
        return Ok(WebpKind::Static(StaticWebpInfo { canvas }));
    }

    let decode_limits = DecodeLimits {
        max_input_bytes: limits.max_input_bytes,
        max_canvas_pixels: limits.max_canvas_pixels,
        max_frame_count: limits.max_frame_count,
        max_frame_rgba_bytes: limits.max_frame_rgba_bytes,
        ..DecodeLimits::default()
    };
    let decoder = AnimationDecoder::new(input, decode_limits).map_err(InspectError::Animation)?;
    Ok(WebpKind::Animated(*decoder.info()))
}

/// Cheap boolean classification for routing. Use [`inspect`] when malformed input must be reported.
pub fn is_animated_webp_fast(input: &[u8]) -> bool {
    animation_flag(input).unwrap_or(false)
}

fn animation_flag(input: &[u8]) -> Result<bool, InspectError> {
    if input.len() < RIFF_HEADER_LEN {
        return Err(InspectError::TruncatedContainer);
    }
    if &input[..4] != b"RIFF" || &input[8..RIFF_HEADER_LEN] != b"WEBP" {
        return Err(InspectError::InvalidContainer);
    }
    let riff_size =
        usize::try_from(u32::from_le_bytes(input[4..8].try_into().unwrap())).unwrap_or(usize::MAX);
    let container_end = riff_size
        .checked_add(8)
        .ok_or(InspectError::TruncatedContainer)?;
    if container_end > input.len() {
        return Err(InspectError::TruncatedContainer);
    }

    let mut offset = RIFF_HEADER_LEN;
    while offset < container_end {
        let header_end = offset
            .checked_add(RIFF_CHUNK_HEADER_LEN)
            .ok_or(InspectError::TruncatedContainer)?;
        if header_end > container_end {
            return Err(InspectError::TruncatedContainer);
        }
        let chunk = &input[offset..offset + 4];
        let length = usize::try_from(u32::from_le_bytes(
            input[offset + 4..header_end].try_into().unwrap(),
        ))
        .unwrap_or(usize::MAX);
        let payload = header_end;
        let padded_length = length
            .checked_add(length & 1)
            .ok_or(InspectError::TruncatedContainer)?;
        let next = payload
            .checked_add(padded_length)
            .ok_or(InspectError::TruncatedContainer)?;
        if next > container_end {
            return Err(InspectError::TruncatedContainer);
        }
        if chunk == b"VP8X" {
            if length < 10 {
                return Err(InspectError::TruncatedContainer);
            }
            return Ok(input[payload] & WEBP_ANIMATION_FLAG != 0);
        }
        offset = next;
    }
    Ok(false)
}

fn enforce_canvas_limit(canvas: CanvasSize, maximum: u64) -> Result<(), InspectError> {
    let actual = canvas.pixel_count().ok_or(InspectError::InvalidWebp)?;
    if actual > maximum {
        return Err(InspectError::LimitExceeded {
            limit: "canvas pixels",
            actual,
            maximum,
        });
    }
    Ok(())
}
