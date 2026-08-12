use std::{error::Error, fmt, time::Duration};

use libwebp_sys::{
    WEBP_CSP_MODE, WebPAnimDecoder, WebPAnimDecoderDelete, WebPAnimDecoderGetDemuxer,
    WebPAnimDecoderGetInfo, WebPAnimDecoderGetNext, WebPAnimDecoderHasMoreFrames,
    WebPAnimDecoderNewInternal, WebPAnimDecoderOptions, WebPAnimDecoderOptionsInitInternal,
    WebPAnimDecoderReset, WebPAnimInfo, WebPData, WebPDemuxGetFrame, WebPDemuxNextFrame,
    WebPDemuxReleaseIterator, WebPGetDemuxABIVersion, WebPIterator,
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

struct RawDecoderGuard(*mut WebPAnimDecoder);

impl RawDecoderGuard {
    fn into_raw(mut self) -> *mut WebPAnimDecoder {
        let decoder = self.0;
        self.0 = std::ptr::null_mut();
        decoder
    }
}

impl Drop for RawDecoderGuard {
    fn drop(&mut self) {
        // SAFETY: the pointer is returned by libwebp and is released at most once;
        // `into_raw` nulls it before ownership is transferred to `AnimationDecoder`.
        unsafe {
            if !self.0.is_null() {
                WebPAnimDecoderDelete(self.0);
            }
        }
    }
}

struct DemuxIteratorGuard {
    iterator: WebPIterator,
    initialized: bool,
}

impl DemuxIteratorGuard {
    fn new() -> Self {
        // SAFETY: `WebPIterator` is a C struct whose fields are initialized by
        // `WebPDemuxGetFrame`; zeroed storage is valid for that writable input.
        Self {
            iterator: unsafe { std::mem::zeroed() },
            initialized: false,
        }
    }

    fn mark_initialized(&mut self) {
        self.initialized = true;
    }
}

impl Drop for DemuxIteratorGuard {
    fn drop(&mut self) {
        // SAFETY: the iterator storage remains valid for the guard lifetime;
        // libwebp requires releasing the iterator after the demux attempt and
        // before the borrowed demuxer can be used or destroyed.
        if self.initialized {
            // SAFETY: `initialized` is set only after WebPDemuxGetFrame succeeds.
            unsafe { WebPDemuxReleaseIterator(&mut self.iterator) };
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
        let raw_decoder = unsafe { WebPAnimDecoderNewInternal(&data, &options, demux_abi) };
        if raw_decoder.is_null() {
            return Err(DecodeError::DecoderCreation);
        }
        let decoder_guard = RawDecoderGuard(raw_decoder);

        // SAFETY: libwebp writes `raw_info` when given a valid decoder.
        let mut raw_info: WebPAnimInfo = unsafe { std::mem::zeroed() };
        // SAFETY: `decoder_guard.0` is non-null and `raw_info` is valid writable storage.
        if unsafe { WebPAnimDecoderGetInfo(decoder_guard.0, &mut raw_info) } == 0 {
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
            decoder: decoder_guard.into_raw(),
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

    /// Returns one source display duration per stored frame, in frame order.
    ///
    /// This reads animation-container metadata only: it does not decode RGBA
    /// pixels or advance the sequential decoder. The configured total-duration
    /// limit is applied and may cause this method to return an error.
    pub fn frame_durations(&self) -> Result<Vec<Duration>, DecodeError> {
        // SAFETY: `decoder` is non-null and remains valid for this value's lifetime;
        // libwebp returns a borrowed demuxer owned by that decoder.
        let demuxer = unsafe { WebPAnimDecoderGetDemuxer(self.decoder) };
        if demuxer.is_null() {
            return Err(DecodeError::DecoderInfo);
        }

        let mut iterator = DemuxIteratorGuard::new();
        // SAFETY: `demuxer` is non-null and borrowed for the guard lifetime;
        // `iterator` is valid writable storage for libwebp to initialize.
        if unsafe { WebPDemuxGetFrame(demuxer, 1, &mut iterator.iterator) } == 0 {
            return Err(DecodeError::DecoderInfo);
        }
        iterator.mark_initialized();

        let frame_count = usize::try_from(self.info.frame_count)
            .map_err(|_| DecodeError::InvalidAnimationInfo)?;
        if u32::try_from(iterator.iterator.num_frames).ok() != Some(self.info.frame_count)
            || iterator.iterator.frame_num != 1
        {
            return Err(DecodeError::InvalidAnimationInfo);
        }

        let mut durations = Vec::with_capacity(frame_count);
        let mut total_duration = Duration::ZERO;
        loop {
            let actual_count = durations.len().saturating_add(1);
            if actual_count > frame_count {
                return Err(DecodeError::InvalidAnimationInfo);
            }
            if i32::try_from(actual_count).ok() != Some(iterator.iterator.frame_num) {
                return Err(DecodeError::InvalidAnimationInfo);
            }

            let duration_ms = iterator.iterator.duration;
            if duration_ms < 0 {
                return Err(DecodeError::InvalidTimestamp);
            }
            let duration = Duration::from_millis(
                u64::try_from(duration_ms).map_err(|_| DecodeError::InvalidTimestamp)?,
            );
            total_duration = total_duration
                .checked_add(duration)
                .ok_or(DecodeError::InvalidTimestamp)?;
            let total_duration_ms = u64::try_from(total_duration.as_millis())
                .map_err(|_| DecodeError::InvalidTimestamp)?;
            enforce_limit(
                "total duration in milliseconds",
                total_duration_ms,
                self.max_total_duration
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
            )?;
            durations.push(duration);

            // SAFETY: the iterator was initialized successfully and its
            // borrowed demuxer remains alive; libwebp advances only its
            // iterator state and reports the end through the return value.
            if unsafe { WebPDemuxNextFrame(&mut iterator.iterator) } == 0 {
                break;
            }
        }

        if durations.len() != frame_count {
            return Err(DecodeError::InvalidAnimationInfo);
        }
        Ok(durations)
    }

    /// Resets this decoder to the first frame of its stored animation sequence.
    ///
    /// The decoder, input allocation, immutable metadata, and configured limits
    /// are preserved; only libwebp's sequence state and this wrapper's timing
    /// accumulator are reset.
    /// This is a sequence reset, not a random-seek operation; the next
    /// [`Self::next_frame`] call starts at the first stored frame.
    pub fn reset(&mut self) {
        // SAFETY: `decoder` is non-null and remains valid for this value's lifetime;
        // libwebp resets only the native decoder sequence state.
        unsafe { WebPAnimDecoderReset(self.decoder) };
        self.previous_timestamp_ms = 0;
        self.total_duration = Duration::ZERO;
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
