use std::{error::Error, ffi::CStr, fmt, time::Duration};

use libwebp_sys::{
    WebPAnimEncoder, WebPAnimEncoderAdd, WebPAnimEncoderAssemble, WebPAnimEncoderDelete,
    WebPAnimEncoderGetError, WebPAnimEncoderNewInternal, WebPAnimEncoderOptions,
    WebPAnimEncoderOptionsInitInternal, WebPConfig, WebPData, WebPDataClear, WebPGetMuxABIVersion,
    WebPPicture, WebPPictureFree, WebPPictureImportRGBA, WebPValidateConfig,
};

use crate::model::{AnimationFrame, AnimationInfo, BackgroundColor, CanvasSize, LoopCount};

/// Compression and animation metadata for an [`AnimationEncoder`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationEncoderOptions {
    /// Loop policy written to the output animation.
    pub loop_count: LoopCount,
    /// Raw ANIM background color written to the output animation.
    pub background_color: BackgroundColor,
    /// Explicit overrides for libwebp's per-frame encoding configuration.
    /// Unspecified fields retain libwebp's initialized defaults.
    pub config: EncoderConfigOverrides,
    /// Explicit overrides for libwebp's animation-mux configuration.
    /// Unspecified fields retain libwebp's initialized defaults.
    pub animation: AnimationMuxOverrides,
}

/// Optional per-frame configuration passed to libwebp.
///
/// Applications should choose their product policy explicitly. `None` means to
/// retain the corresponding value supplied by `WebPConfig::new()`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EncoderConfigOverrides {
    /// Lossy quality in libwebp's inclusive `0.0..=100.0` range.
    pub quality: Option<f32>,
    /// Encode frames losslessly when `Some(true)` or lossy when `Some(false)`.
    pub lossless: Option<bool>,
    /// libwebp encoding method in the inclusive `0..=6` range.
    pub method: Option<u8>,
    /// Use libwebp's high-quality (and slower) RGB-to-YUV conversion.
    pub use_sharp_yuv: Option<bool>,
    /// Let libwebp select the in-loop filter strength per frame.
    pub autofilter: Option<bool>,
    /// Alpha compression quality in the inclusive `0..=100` range.
    pub alpha_quality: Option<u8>,
    /// libwebp preprocessing mode (`0` disables it). Mode 2 introduces
    /// dithering, which can create temporal noise in animation.
    pub preprocessing: Option<u8>,
    /// Ask libwebp to use its internal encoder threading when available.
    pub thread_level: Option<bool>,
    /// Fixed in-loop filtering settings.
    pub filter_strength: Option<i32>,
    /// Fixed in-loop filter sharpness in libwebp's `0..=7` range.
    pub filter_sharpness: Option<i32>,
    /// In-loop filter type in libwebp's `0..=1` range.
    pub filter_type: Option<i32>,
}

/// Optional animation-mux configuration passed to libwebp.
///
/// `kmin` and `kmax` form one setting: provide both to choose a key-frame
/// policy, or neither to retain libwebp's initialized behavior. The canonical
/// `(0, 0)` disables inserted key frames and `(0, 1)` makes every frame a key
/// frame. For `kmax >= 2`, libwebp requires `kmin < kmax`,
/// `kmin >= kmax / 2 + 1`, and a range no wider than 30 frames.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AnimationMuxOverrides {
    /// Let libwebp favor a smaller animation over encoding speed.
    pub minimize_size: Option<bool>,
    /// Allow libwebp to choose lossless or lossy encoding for each frame.
    pub allow_mixed: Option<bool>,
    /// Minimum and maximum distance between animation key frames.
    pub kmin: Option<i32>,
    /// Maximum distance between animation key frames.
    pub kmax: Option<i32>,
}

impl AnimationEncoderOptions {
    /// Creates options with explicit animation metadata and no encoding overrides.
    pub const fn new(loop_count: LoopCount, background_color: BackgroundColor) -> Self {
        Self {
            loop_count,
            background_color,
            config: EncoderConfigOverrides {
                quality: None,
                lossless: None,
                method: None,
                use_sharp_yuv: None,
                autofilter: None,
                alpha_quality: None,
                preprocessing: None,
                thread_level: None,
                filter_strength: None,
                filter_sharpness: None,
                filter_type: None,
            },
            animation: AnimationMuxOverrides {
                minimize_size: None,
                allow_mixed: None,
                kmin: None,
                kmax: None,
            },
        }
    }

    /// Starts with animation metadata copied from a decoded source sequence.
    pub const fn from_animation_info(info: AnimationInfo) -> Self {
        Self::new(info.loop_count, info.background_color)
    }
}

impl Default for AnimationEncoderOptions {
    fn default() -> Self {
        // libwebp 0.9.6 initializes these mux metadata values to loop_count =
        // 0 and bgcolor = 0xffffffff. No product compression or key-frame
        // policy is selected here.
        Self::new(LoopCount::Infinite, BackgroundColor { raw: 0xffff_ffff })
    }
}

/// Sequential animated-WebP encoder for full-canvas RGBA frames.
///
/// [`Self::add_frame`] accepts frames in presentation order. It converts each
/// frame's duration to the cumulative millisecond timestamp required by WebP,
/// then [`Self::finish`] writes the final timestamp needed to retain the last
/// frame's duration.
pub struct AnimationEncoder {
    encoder: *mut WebPAnimEncoder,
    canvas: CanvasSize,
    config: WebPConfig,
    next_timestamp_ms: i32,
    frame_count: u32,
}

impl Drop for AnimationEncoder {
    fn drop(&mut self) {
        // SAFETY: `encoder` is created only by libwebp and released exactly once here.
        unsafe {
            if !self.encoder.is_null() {
                WebPAnimEncoderDelete(self.encoder);
            }
        }
    }
}

impl AnimationEncoder {
    /// Creates an encoder for full-canvas RGBA frames of the given dimensions.
    ///
    /// Frames must be added in presentation order and must use exactly
    /// `canvas.width * canvas.height * 4` bytes. At least one frame is required
    /// before [`Self::finish`] can produce output.
    pub fn new(canvas: CanvasSize, options: AnimationEncoderOptions) -> Result<Self, EncodeError> {
        let width =
            i32::try_from(canvas.width).map_err(|_| EncodeError::InvalidCanvasSize(canvas))?;
        let height =
            i32::try_from(canvas.height).map_err(|_| EncodeError::InvalidCanvasSize(canvas))?;
        if width == 0 || height == 0 || canvas.rgba_bytes().is_none() {
            return Err(EncodeError::InvalidCanvasSize(canvas));
        }

        validate_options(&options)?;
        let mut config = WebPConfig::new().map_err(|_| EncodeError::ConfigInitialization)?;
        apply_config_overrides(&mut config, options.config);
        // SAFETY: `config` was initialized by libwebp and remains valid for the call.
        if unsafe { WebPValidateConfig(&config) } == 0 {
            return Err(EncodeError::LibwebpConfigRejected);
        }

        let mux_abi = WebPGetMuxABIVersion();
        // SAFETY: libwebp initializes every field before the options are read.
        let mut encoder_options: WebPAnimEncoderOptions = unsafe { std::mem::zeroed() };
        // SAFETY: the options pointer is writable and the ABI comes from libwebp.
        if unsafe { WebPAnimEncoderOptionsInitInternal(&mut encoder_options, mux_abi) } == 0 {
            return Err(EncodeError::EncoderOptionsInitialization);
        }
        encoder_options.anim_params.loop_count = match options.loop_count {
            LoopCount::Infinite => 0,
            LoopCount::Finite(count) => i32::from(count.get()),
        };
        encoder_options.anim_params.bgcolor = options.background_color.raw;
        apply_mux_overrides(&mut encoder_options, options.animation);

        // SAFETY: dimensions and initialized options remain valid for this construction call.
        let encoder =
            unsafe { WebPAnimEncoderNewInternal(width, height, &encoder_options, mux_abi) };
        if encoder.is_null() {
            return Err(EncodeError::EncoderCreation);
        }

        Ok(Self {
            encoder,
            canvas,
            config,
            next_timestamp_ms: 0,
            frame_count: 0,
        })
    }

    /// Returns the canvas dimensions required by every frame.
    pub const fn canvas(&self) -> CanvasSize {
        self.canvas
    }

    /// Returns the number of frames accepted so far.
    pub const fn frame_count(&self) -> u32 {
        self.frame_count
    }

    /// Adds one full-canvas frame in presentation order.
    pub fn add_frame(&mut self, frame: &AnimationFrame) -> Result<(), EncodeError> {
        if frame.canvas != self.canvas {
            return Err(EncodeError::UnexpectedFrameCanvas {
                actual: frame.canvas,
                expected: self.canvas,
            });
        }
        self.add_rgba(&frame.rgba, frame.duration)
    }

    /// Adds one full-canvas RGBA frame without allocating an [`AnimationFrame`].
    pub fn add_rgba(&mut self, rgba: &[u8], duration: Duration) -> Result<(), EncodeError> {
        let expected = self
            .canvas
            .rgba_bytes()
            .ok_or(EncodeError::InvalidCanvasSize(self.canvas))?;
        if rgba.len() != expected {
            return Err(EncodeError::InvalidFrameBufferLength {
                actual: rgba.len(),
                expected,
            });
        }
        let duration_ms = duration_to_millis(duration)?;
        let end_timestamp_ms = self
            .next_timestamp_ms
            .checked_add(duration_ms)
            .ok_or(EncodeError::TimestampOverflow)?;

        let mut picture = Picture::from_rgba(self.canvas, rgba)?;
        // SAFETY: encoder, picture, and config are valid; libwebp consumes the frame during this call.
        if unsafe {
            WebPAnimEncoderAdd(
                self.encoder,
                &mut picture.0,
                self.next_timestamp_ms,
                &self.config,
            )
        } == 0
        {
            return Err(EncodeError::Libwebp(encoder_error(self.encoder)));
        }

        self.next_timestamp_ms = end_timestamp_ms;
        self.frame_count = self
            .frame_count
            .checked_add(1)
            .ok_or(EncodeError::FrameCountOverflow)?;
        Ok(())
    }

    /// Flushes the final frame duration and returns the encoded WebP bytes.
    pub fn finish(self) -> Result<Vec<u8>, EncodeError> {
        if self.frame_count == 0 {
            return Err(EncodeError::NoFrames);
        }
        // SAFETY: the null picture with the final timestamp signals end-of-stream to libwebp.
        if unsafe {
            WebPAnimEncoderAdd(
                self.encoder,
                std::ptr::null_mut(),
                self.next_timestamp_ms,
                std::ptr::null(),
            )
        } == 0
        {
            return Err(EncodeError::Libwebp(encoder_error(self.encoder)));
        }
        let mut encoded = WebPData::default();
        // SAFETY: libwebp initializes `encoded` on success for this valid encoder.
        if unsafe { WebPAnimEncoderAssemble(self.encoder, &mut encoded) } == 0 {
            return Err(EncodeError::Libwebp(encoder_error(self.encoder)));
        }
        // SAFETY: libwebp allocated exactly `encoded.size` bytes and ownership is released below.
        let output = unsafe { std::slice::from_raw_parts(encoded.bytes, encoded.size) }.to_vec();
        // SAFETY: `encoded` is initialized by libwebp and this frees its output allocation exactly once.
        unsafe { WebPDataClear(&mut encoded) };
        Ok(output)
    }
}

/// Failure to create or use an [`AnimationEncoder`].
#[derive(Clone, Debug, PartialEq)]
pub enum EncodeError {
    /// The encoder canvas has a zero dimension or cannot be represented by libwebp.
    InvalidCanvasSize(CanvasSize),
    /// libwebp could not initialize its encoding configuration.
    ConfigInitialization,
    /// The lossy quality override is outside `0.0..=100.0` or is not finite.
    InvalidQuality {
        /// Rejected quality value.
        value: f32,
    },
    /// The encoding method is outside libwebp's `0..=6` range.
    InvalidMethod {
        /// Rejected method value.
        value: u8,
    },
    /// The alpha quality is outside libwebp's `0..=100` range.
    InvalidAlphaQuality {
        /// Rejected alpha quality value.
        value: u8,
    },
    /// The preprocessing mode is outside libwebp's `0..=2` range.
    InvalidPreprocessing {
        /// Rejected preprocessing mode.
        value: u8,
    },
    /// The filter strength is outside libwebp's `0..=100` range.
    InvalidFilterStrength {
        /// Rejected filter strength.
        value: i32,
    },
    /// The filter sharpness is outside libwebp's `0..=7` range.
    InvalidFilterSharpness {
        /// Rejected filter sharpness.
        value: i32,
    },
    /// The filter type is outside libwebp's `0..=1` range.
    InvalidFilterType {
        /// Rejected filter type.
        value: i32,
    },
    /// Only one of `kmin` and `kmax` was supplied.
    IncompleteKeyframeInterval {
        /// Supplied minimum key-frame interval.
        kmin: Option<i32>,
        /// Supplied maximum key-frame interval.
        kmax: Option<i32>,
    },
    /// `kmin` is negative or is not smaller than `kmax`.
    InvalidKeyframeInterval {
        /// Supplied minimum key-frame interval.
        kmin: i32,
        /// Supplied maximum key-frame interval.
        kmax: i32,
    },
    /// A special key-frame mode was not expressed as `(0, 0)` or `(0, 1)`.
    NonCanonicalKeyframeMode {
        /// Supplied minimum key-frame interval.
        kmin: i32,
        /// Supplied maximum key-frame interval.
        kmax: i32,
    },
    /// `kmin` is below libwebp's required minimum for the selected `kmax`.
    KeyframeIntervalBelowMinimum {
        /// Supplied minimum key-frame interval.
        kmin: i32,
        /// Required minimum key-frame interval.
        minimum: i32,
        /// Supplied maximum key-frame interval.
        kmax: i32,
    },
    /// The key-frame interval is wider than libwebp permits.
    KeyframeIntervalTooWide {
        /// Supplied minimum key-frame interval.
        kmin: i32,
        /// Supplied maximum key-frame interval.
        kmax: i32,
        /// Maximum permitted `kmax - kmin` span.
        maximum_span: i32,
    },
    /// Key-frame intervals cannot be combined with `minimize_size = Some(true)`.
    KeyframeIntervalIgnoredByMinimizeSize,
    /// libwebp rejected an otherwise locally valid configuration.
    LibwebpConfigRejected,
    /// libwebp could not initialize animation encoder options.
    EncoderOptionsInitialization,
    /// libwebp could not create the animation encoder.
    EncoderCreation,
    /// [`AnimationEncoder::finish`] was called before adding a frame.
    NoFrames,
    /// A frame canvas differs from the encoder canvas.
    UnexpectedFrameCanvas {
        /// Canvas supplied by the caller.
        actual: CanvasSize,
        /// Canvas required by the encoder.
        expected: CanvasSize,
    },
    /// A frame buffer is not tightly packed RGBA8 for the encoder canvas.
    InvalidFrameBufferLength {
        /// Actual frame buffer size in bytes.
        actual: usize,
        /// Required frame buffer size in bytes.
        expected: usize,
    },
    /// A frame duration is not an exact whole number of milliseconds.
    NonMillisecondDuration(Duration),
    /// Cumulative frame duration exceeds libwebp's timestamp range.
    TimestampOverflow,
    /// The number of added frames exceeds `u32`.
    FrameCountOverflow,
    /// libwebp could not initialize a frame picture.
    PictureInitialization,
    /// libwebp could not import RGBA pixels into a frame picture.
    PictureImport,
    /// libwebp reported an encoding error message.
    Libwebp(String),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCanvasSize(size) => write!(
                f,
                "canvas has invalid dimensions {}x{}",
                size.width, size.height
            ),
            Self::ConfigInitialization => {
                f.write_str("failed to initialize WebP encoder configuration")
            }
            Self::InvalidQuality { value } => {
                write!(f, "quality {value} is outside libwebp's 0.0..=100.0 range")
            }
            Self::InvalidMethod { value } => {
                write!(f, "method {value} is outside libwebp's 0..=6 range")
            }
            Self::InvalidAlphaQuality { value } => {
                write!(f, "alpha quality {value} is outside libwebp's 0..=100 range")
            }
            Self::InvalidPreprocessing { value } => {
                write!(f, "preprocessing mode {value} is outside libwebp's 0..=2 range")
            }
            Self::InvalidFilterStrength { value } => {
                write!(f, "filter strength {value} is outside libwebp's 0..=100 range")
            }
            Self::InvalidFilterSharpness { value } => {
                write!(f, "filter sharpness {value} is outside libwebp's 0..=7 range")
            }
            Self::InvalidFilterType { value } => {
                write!(f, "filter type {value} is outside libwebp's 0..=1 range")
            }
            Self::IncompleteKeyframeInterval { kmin, kmax } => write!(
                f,
                "key-frame interval requires both kmin and kmax (got kmin={kmin:?}, kmax={kmax:?})"
            ),
            Self::InvalidKeyframeInterval { kmin, kmax } => write!(
                f,
                "key-frame interval requires 0 <= kmin < kmax (got kmin={kmin}, kmax={kmax})"
            ),
            Self::NonCanonicalKeyframeMode { kmin, kmax } => write!(
                f,
                "key-frame special modes must use (kmin, kmax) = (0, 0) to disable insertion or (0, 1) for every frame (got {kmin}, {kmax})"
            ),
            Self::KeyframeIntervalBelowMinimum {
                kmin,
                minimum,
                kmax,
            } => write!(
                f,
                "key-frame interval requires kmin >= kmax / 2 + 1; got kmin={kmin}, kmax={kmax}, minimum={minimum}"
            ),
            Self::KeyframeIntervalTooWide {
                kmin,
                kmax,
                maximum_span,
            } => write!(
                f,
                "key-frame interval span kmax - kmin must not exceed {maximum_span}; got kmin={kmin}, kmax={kmax}"
            ),
            Self::KeyframeIntervalIgnoredByMinimizeSize => f.write_str(
                "key-frame interval cannot be set when minimize_size is enabled because libwebp disables key-frame insertion",
            ),
            Self::LibwebpConfigRejected => {
                f.write_str("libwebp rejected the validated encoder configuration")
            }
            Self::EncoderOptionsInitialization => {
                f.write_str("failed to initialize WebP animation encoder options")
            }
            Self::EncoderCreation => f.write_str("failed to create WebP animation encoder"),
            Self::NoFrames => f.write_str("an animated WebP requires at least one frame"),
            Self::UnexpectedFrameCanvas { actual, expected } => write!(
                f,
                "frame canvas {}x{} does not match encoder canvas {}x{}",
                actual.width, actual.height, expected.width, expected.height
            ),
            Self::InvalidFrameBufferLength { actual, expected } => {
                write!(f, "frame buffer is {actual} bytes; expected {expected}")
            }
            Self::NonMillisecondDuration(duration) => write!(
                f,
                "frame duration {duration:?} is not an exact number of milliseconds"
            ),
            Self::TimestampOverflow => {
                f.write_str("cumulative frame duration exceeds libwebp's timestamp range")
            }
            Self::FrameCountOverflow => f.write_str("animation frame count overflows u32"),
            Self::PictureInitialization => f.write_str("failed to initialize a WebP frame picture"),
            Self::PictureImport => f.write_str("failed to import RGBA frame pixels into libwebp"),
            Self::Libwebp(error) => write!(f, "libwebp animation encoding failed: {error}"),
        }
    }
}

impl Error for EncodeError {}

fn validate_options(options: &AnimationEncoderOptions) -> Result<(), EncodeError> {
    let config = options.config;
    if let Some(value) = config.quality {
        if !value.is_finite() || !(0.0..=100.0).contains(&value) {
            return Err(EncodeError::InvalidQuality { value });
        }
    }
    if let Some(value) = config.method {
        if value > 6 {
            return Err(EncodeError::InvalidMethod { value });
        }
    }
    if let Some(value) = config.alpha_quality {
        if value > 100 {
            return Err(EncodeError::InvalidAlphaQuality { value });
        }
    }
    if let Some(value) = config.preprocessing {
        if value > 2 {
            return Err(EncodeError::InvalidPreprocessing { value });
        }
    }
    if let Some(value) = config.filter_strength {
        if !(0..=100).contains(&value) {
            return Err(EncodeError::InvalidFilterStrength { value });
        }
    }
    if let Some(value) = config.filter_sharpness {
        if !(0..=7).contains(&value) {
            return Err(EncodeError::InvalidFilterSharpness { value });
        }
    }
    if let Some(value) = config.filter_type {
        if !(0..=1).contains(&value) {
            return Err(EncodeError::InvalidFilterType { value });
        }
    }

    let animation = options.animation;
    match (animation.kmin, animation.kmax) {
        (None, None) => Ok(()),
        (Some(_), Some(_)) if animation.minimize_size == Some(true) => {
            Err(EncodeError::KeyframeIntervalIgnoredByMinimizeSize)
        }
        (Some(0), Some(0) | Some(1)) => Ok(()),
        (Some(kmin), Some(kmax)) if kmax <= 1 => {
            Err(EncodeError::NonCanonicalKeyframeMode { kmin, kmax })
        }
        (Some(kmin), Some(kmax)) if kmin < 0 || kmin >= kmax => {
            Err(EncodeError::InvalidKeyframeInterval { kmin, kmax })
        }
        (Some(kmin), Some(kmax)) => {
            let minimum = kmax / 2 + 1;
            if kmin < minimum {
                Err(EncodeError::KeyframeIntervalBelowMinimum {
                    kmin,
                    minimum,
                    kmax,
                })
            } else if kmax - kmin > 30 {
                Err(EncodeError::KeyframeIntervalTooWide {
                    kmin,
                    kmax,
                    maximum_span: 30,
                })
            } else {
                Ok(())
            }
        }
        (kmin, kmax) => Err(EncodeError::IncompleteKeyframeInterval { kmin, kmax }),
    }
}

fn apply_config_overrides(config: &mut WebPConfig, overrides: EncoderConfigOverrides) {
    if let Some(value) = overrides.quality {
        config.quality = value;
    }
    if let Some(value) = overrides.lossless {
        config.lossless = i32::from(value);
    }
    if let Some(value) = overrides.method {
        config.method = i32::from(value);
    }
    if let Some(value) = overrides.use_sharp_yuv {
        config.use_sharp_yuv = i32::from(value);
    }
    if let Some(value) = overrides.autofilter {
        config.autofilter = i32::from(value);
    }
    if let Some(value) = overrides.alpha_quality {
        config.alpha_quality = i32::from(value);
    }
    if let Some(value) = overrides.preprocessing {
        config.preprocessing = i32::from(value);
    }
    if let Some(value) = overrides.thread_level {
        config.thread_level = i32::from(value);
    }
    if let Some(value) = overrides.filter_strength {
        config.filter_strength = value;
    }
    if let Some(value) = overrides.filter_sharpness {
        config.filter_sharpness = value;
    }
    if let Some(value) = overrides.filter_type {
        config.filter_type = value;
    }
}

fn apply_mux_overrides(options: &mut WebPAnimEncoderOptions, overrides: AnimationMuxOverrides) {
    if let Some(value) = overrides.minimize_size {
        options.minimize_size = i32::from(value);
    }
    if let Some(value) = overrides.allow_mixed {
        options.allow_mixed = i32::from(value);
    }
    if let Some(value) = overrides.kmin {
        options.kmin = value;
    }
    if let Some(value) = overrides.kmax {
        options.kmax = value;
    }
}

struct Picture(WebPPicture);

impl Picture {
    fn from_rgba(canvas: CanvasSize, rgba: &[u8]) -> Result<Self, EncodeError> {
        let width =
            i32::try_from(canvas.width).map_err(|_| EncodeError::InvalidCanvasSize(canvas))?;
        let height =
            i32::try_from(canvas.height).map_err(|_| EncodeError::InvalidCanvasSize(canvas))?;
        let stride = width
            .checked_mul(4)
            .ok_or(EncodeError::InvalidCanvasSize(canvas))?;
        let mut picture = WebPPicture::new().map_err(|_| EncodeError::PictureInitialization)?;
        picture.use_argb = 1;
        picture.width = width;
        picture.height = height;
        // SAFETY: `rgba` has the validated full-canvas length, and libwebp copies it before returning.
        if unsafe { WebPPictureImportRGBA(&mut picture, rgba.as_ptr(), stride) } == 0 {
            // SAFETY: libwebp may have allocated picture data before reporting failure.
            unsafe { WebPPictureFree(&mut picture) };
            return Err(EncodeError::PictureImport);
        }
        Ok(Self(picture))
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: libwebp initialized this picture; freeing is idempotent for its allocated members.
        unsafe { WebPPictureFree(&mut self.0) };
    }
}

fn duration_to_millis(duration: Duration) -> Result<i32, EncodeError> {
    let milliseconds = duration.as_millis();
    if Duration::from_millis(u64::try_from(milliseconds).unwrap_or(u64::MAX)) != duration {
        return Err(EncodeError::NonMillisecondDuration(duration));
    }
    i32::try_from(milliseconds).map_err(|_| EncodeError::TimestampOverflow)
}

fn encoder_error(encoder: *mut WebPAnimEncoder) -> String {
    // SAFETY: the error pointer, if non-null, is owned by the live encoder.
    let error = unsafe { WebPAnimEncoderGetError(encoder) };
    if error.is_null() {
        "unknown error".to_owned()
    } else {
        // SAFETY: libwebp returns a NUL-terminated error message for this encoder.
        unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    }
}
