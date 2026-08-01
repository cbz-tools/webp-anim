use std::{error::Error, fmt, time::Duration};

use libwebp_sys::{
    WEBP_CSP_MODE, WebPAnimDecoder, WebPAnimDecoderDelete, WebPAnimDecoderGetInfo,
    WebPAnimDecoderGetNext, WebPAnimDecoderHasMoreFrames, WebPAnimDecoderNewInternal,
    WebPAnimDecoderOptions, WebPAnimDecoderOptionsInitInternal, WebPAnimInfo, WebPData,
    WebPGetDemuxABIVersion,
};

use crate::{
    inspect::is_animated_webp_fast,
    model::{AnimationFrame, AnimationInfo, BackgroundColor, CanvasSize, LoopCount},
};

/// Per-animation limits applied before and while decoding.
///
/// The [`Default`] values are intended for ordinary untrusted inputs. Use
/// [`Self::for_trusted_input`] only when the caller has already established an
/// appropriate process-wide memory and workload policy.
#[derive(Clone, Debug)]
pub struct DecodeLimits {
    /// Maximum number of input bytes accepted by [`AnimationDecoder::new`].
    pub max_input_bytes: usize,
    /// Maximum number of pixels in the decoded animation canvas.
    pub max_canvas_pixels: u64,
    /// Maximum number of frames in the stored animation sequence.
    pub max_frame_count: u32,
    /// Maximum sum of source frame durations observed while decoding.
    pub max_total_duration: Duration,
    /// Maximum number of bytes in one full-canvas RGBA frame.
    pub max_frame_rgba_bytes: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 256 * 1024 * 1024,
            max_canvas_pixels: 100_000_000,
            max_frame_count: 10_000,
            max_total_duration: Duration::from_secs(60 * 60),
            max_frame_rgba_bytes: 400 * 1024 * 1024,
        }
    }
}

impl DecodeLimits {
    /// Relaxes crate-level resource limits for trusted input.
    ///
    /// This does not bypass libwebp or platform allocation limits.
    pub const fn for_trusted_input() -> Self {
        Self {
            max_input_bytes: usize::MAX,
            max_canvas_pixels: u64::MAX,
            max_frame_count: u32::MAX,
            max_total_duration: Duration::MAX,
            max_frame_rgba_bytes: usize::MAX,
        }
    }
}

/// Failure to create or read an animation decoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// The input is larger than the configured byte limit.
    InputTooLarge {
        /// Actual input length in bytes.
        actual: usize,
        /// Configured maximum input length in bytes.
        maximum: usize,
    },
    /// The input is a valid WebP image but does not contain animation data.
    NotAnimatedWebp,
    /// libwebp could not initialize its decoder options.
    DecoderOptionsInitialization,
    /// libwebp could not create an animation decoder.
    DecoderCreation,
    /// libwebp could not provide animation metadata.
    DecoderInfo,
    /// The animation metadata could not be represented by this crate's types.
    InvalidAnimationInfo,
    /// A canvas or frame size overflowed the host address space.
    FrameSizeOverflow,
    /// A configured resource limit was exceeded.
    LimitExceeded {
        /// Name of the exceeded limit.
        limit: &'static str,
        /// Observed value.
        actual: u64,
        /// Configured maximum value.
        maximum: u64,
    },
    /// libwebp failed to decode the next frame.
    FrameDecode,
    /// Frame timestamps were not monotonic or could not be accumulated.
    InvalidTimestamp,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { actual, maximum } => {
                write!(
                    f,
                    "input is {actual} bytes, exceeding the {maximum}-byte limit"
                )
            }
            Self::NotAnimatedWebp => f.write_str("input is not an animated WebP"),
            Self::DecoderOptionsInitialization => {
                f.write_str("failed to initialize WebP decoder options")
            }
            Self::DecoderCreation => f.write_str("failed to create WebP animation decoder"),
            Self::DecoderInfo => f.write_str("failed to read WebP animation information"),
            Self::InvalidAnimationInfo => f.write_str("WebP animation information is invalid"),
            Self::FrameSizeOverflow => {
                f.write_str("WebP animation frame size overflows the host address space")
            }
            Self::LimitExceeded {
                limit,
                actual,
                maximum,
            } => {
                write!(f, "{limit} is {actual}, exceeding the {maximum} limit")
            }
            Self::FrameDecode => f.write_str("failed to decode WebP animation frame"),
            Self::InvalidTimestamp => f.write_str("WebP animation timestamps are invalid"),
        }
    }
}

impl Error for DecodeError {}

/// Stateful decoder for exactly one stored animation sequence.
pub struct AnimationDecoder {
    // The C decoder borrows this allocation for its complete lifetime.
    _input: Vec<u8>,
    decoder: *mut WebPAnimDecoder,
    info: AnimationInfo,
    frame_rgba_bytes: usize,
    previous_timestamp_ms: i32,
    total_duration: Duration,
    max_total_duration: Duration,
}

impl Drop for AnimationDecoder {
    fn drop(&mut self) {
        // SAFETY: `decoder` is created only by libwebp and released exactly once here.
        unsafe {
            if !self.decoder.is_null() {
                WebPAnimDecoderDelete(self.decoder);
            }
        }
    }
}

impl AnimationDecoder {
    /// Creates a decoder for one stored animated WebP sequence.
    ///
    /// The input bytes are copied so the returned decoder owns the data needed
    /// by libwebp. Frames are returned as composited, full-canvas RGBA buffers.
    /// The decoder does not replay the sequence according to its loop count.
    pub fn new(input: &[u8], limits: DecodeLimits) -> Result<Self, DecodeError> {
        if input.len() > limits.max_input_bytes {
            return Err(DecodeError::InputTooLarge {
                actual: input.len(),
                maximum: limits.max_input_bytes,
            });
        }
        if !is_animated_webp_fast(input) {
            return Err(DecodeError::NotAnimatedWebp);
        }

        let input = input.to_vec();
        // SAFETY: libwebp initializes every field before the options are read.
        let mut options: WebPAnimDecoderOptions = unsafe { std::mem::zeroed() };
        let demux_abi = WebPGetDemuxABIVersion();
        // SAFETY: `options` is a valid writable pointer and the ABI is supplied by libwebp.
        if unsafe { WebPAnimDecoderOptionsInitInternal(&mut options, demux_abi) } == 0 {
            return Err(DecodeError::DecoderOptionsInitialization);
        }
        options.color_mode = WEBP_CSP_MODE::MODE_RGBA;
        options.use_threads = 1;

        let data = WebPData {
            bytes: input.as_ptr(),
            size: input.len(),
        };
        // SAFETY: `input` is moved into `Self`, keeping `data.bytes` valid until the decoder drops.
        let decoder = unsafe { WebPAnimDecoderNewInternal(&data, &options, demux_abi) };
        if decoder.is_null() {
            return Err(DecodeError::DecoderCreation);
        }

        // SAFETY: libwebp writes `raw_info` when given a valid decoder.
        let mut raw_info: WebPAnimInfo = unsafe { std::mem::zeroed() };
        // SAFETY: `decoder` is non-null and `raw_info` is valid writable storage.
        if unsafe { WebPAnimDecoderGetInfo(decoder, &mut raw_info) } == 0 {
            // SAFETY: creation succeeded, so the decoder must be released on this early return.
            unsafe { WebPAnimDecoderDelete(decoder) };
            return Err(DecodeError::DecoderInfo);
        }

        let canvas = CanvasSize {
            width: raw_info.canvas_width,
            height: raw_info.canvas_height,
        };
        let pixel_count = canvas.pixel_count().ok_or(DecodeError::FrameSizeOverflow)?;
        enforce_limit("canvas pixels", pixel_count, limits.max_canvas_pixels)?;
        enforce_limit(
            "frame count",
            u64::from(raw_info.frame_count),
            u64::from(limits.max_frame_count),
        )?;
        let frame_rgba_bytes = canvas.rgba_bytes().ok_or(DecodeError::FrameSizeOverflow)?;
        enforce_limit(
            "RGBA bytes per frame",
            u64::try_from(frame_rgba_bytes).unwrap_or(u64::MAX),
            u64::try_from(limits.max_frame_rgba_bytes).unwrap_or(u64::MAX),
        )?;

        let loop_count = match raw_info.loop_count {
            0 => LoopCount::Infinite,
            value => LoopCount::Finite(
                std::num::NonZeroU16::new(
                    u16::try_from(value).map_err(|_| DecodeError::InvalidAnimationInfo)?,
                )
                .expect("non-zero loop count"),
            ),
        };
        Ok(Self {
            _input: input,
            decoder,
            info: AnimationInfo {
                canvas,
                frame_count: raw_info.frame_count,
                loop_count,
                background_color: BackgroundColor {
                    raw: raw_info.bgcolor,
                },
            },
            frame_rgba_bytes,
            previous_timestamp_ms: 0,
            total_duration: Duration::ZERO,
            max_total_duration: limits.max_total_duration,
        })
    }

    /// Returns metadata for the stored animation sequence.
    pub fn info(&self) -> &AnimationInfo {
        &self.info
    }

    /// Returns whether unread frames remain in the stored sequence.
    pub fn has_more_frames(&self) -> bool {
        // SAFETY: the decoder is valid until `Drop`; this query does not advance it.
        unsafe { WebPAnimDecoderHasMoreFrames(self.decoder) != 0 }
    }

    /// Returns `None` only after every frame in the stored sequence was read.
    pub fn next_frame(&mut self) -> Result<Option<AnimationFrame>, DecodeError> {
        if !self.has_more_frames() {
            return Ok(None);
        }

        let mut rgba = std::ptr::null_mut();
        let mut timestamp_ms = 0_i32;
        // SAFETY: libwebp writes both output pointers for a valid decoder state.
        let ok = unsafe { WebPAnimDecoderGetNext(self.decoder, &mut rgba, &mut timestamp_ms) };
        if ok == 0 || rgba.is_null() {
            return Err(DecodeError::FrameDecode);
        }
        let duration_ms = timestamp_ms
            .checked_sub(self.previous_timestamp_ms)
            .ok_or(DecodeError::InvalidTimestamp)?;
        let duration = Duration::from_millis(
            u64::try_from(duration_ms).map_err(|_| DecodeError::InvalidTimestamp)?,
        );
        let total_duration = self
            .total_duration
            .checked_add(duration)
            .ok_or(DecodeError::InvalidTimestamp)?;
        enforce_limit(
            "total duration in milliseconds",
            total_duration.as_millis().try_into().unwrap_or(u64::MAX),
            self.max_total_duration
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        )?;

        // SAFETY: libwebp returns a full-canvas RGBA buffer with the checked size above.
        let rgba = unsafe { std::slice::from_raw_parts(rgba, self.frame_rgba_bytes) }.to_vec();
        self.previous_timestamp_ms = timestamp_ms;
        self.total_duration = total_duration;

        Ok(Some(AnimationFrame {
            rgba,
            canvas: self.info.canvas,
            duration,
        }))
    }
}

fn enforce_limit(limit: &'static str, actual: u64, maximum: u64) -> Result<(), DecodeError> {
    if actual > maximum {
        return Err(DecodeError::LimitExceeded {
            limit,
            actual,
            maximum,
        });
    }
    Ok(())
}
