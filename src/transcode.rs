use std::{error::Error, fmt, time::Duration};

use crate::{
    AnimationDecoder, AnimationEncoder, AnimationEncoderOptions, AnimationInfo,
    AnimationMuxOverrides, CanvasSize, DecodeError, DecodeLimits, EncodeError,
    EncoderConfigOverrides, ResizeError, ResizeOptions, ResizePlan,
};

/// Compression and resize inputs for [`transcode_animated_webp`].
///
/// The source animation's loop count and raw background color are always
/// copied to the output. Product-level quality defaults, output-selection
/// policies, and process-wide resource budgets belong to the caller.
#[derive(Clone, Debug)]
pub struct AnimationTranscodeOptions {
    /// Limits for the one input animation sequence.
    pub decode_limits: DecodeLimits,
    /// Full-canvas resize operation derived from the source animation canvas.
    pub resize: ResizeOptions,
    /// Explicit per-frame libwebp configuration overrides.
    pub encoder_config: EncoderConfigOverrides,
    /// Explicit libwebp animation-mux configuration overrides.
    pub animation: AnimationMuxOverrides,
}

impl AnimationTranscodeOptions {
    /// Creates a request with the supplied resize operation and no compression
    /// overrides beyond libwebp's initialized defaults.
    pub fn new(resize: ResizeOptions) -> Self {
        Self {
            decode_limits: DecodeLimits::default(),
            resize,
            encoder_config: EncoderConfigOverrides::default(),
            animation: AnimationMuxOverrides::default(),
        }
    }
}

/// Result of a sequential animated-WebP transcode.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscodedAnimation {
    /// Newly encoded animated WebP bytes.
    pub bytes: Vec<u8>,
    /// Metadata read from the stored source sequence and retained by encoding.
    pub input: AnimationInfo,
    /// Canvas dimensions of the encoded sequence.
    pub output_canvas: CanvasSize,
    /// Number of frames decoded and encoded.
    pub frame_count: u32,
    /// Sum of the source frame durations passed through to the encoder.
    pub total_duration: Duration,
}

/// Decodes, resizes, and re-encodes exactly one stored animated WebP sequence.
///
/// Frames are decoded and encoded in stored order. The operation holds one
/// decoded frame and one reusable resize destination at a time; it does not
/// buffer the whole animation. This convenience API always encodes output,
/// including when the resize operation is a no-op. Callers decide whether a
/// source should be passed through or whether an encoded result should replace
/// it.
pub fn transcode_animated_webp(
    input: &[u8],
    options: AnimationTranscodeOptions,
) -> Result<TranscodedAnimation, TranscodeError> {
    let mut decoder =
        AnimationDecoder::new(input, options.decode_limits).map_err(TranscodeError::Decode)?;
    let source = *decoder.info();
    let resize = ResizePlan::new(source.canvas, options.resize).map_err(TranscodeError::Resize)?;
    let mut workspace = resize.workspace().map_err(TranscodeError::Resize)?;

    let mut encoder_options = AnimationEncoderOptions::from_animation_info(source);
    encoder_options.config = options.encoder_config;
    encoder_options.animation = options.animation;
    let mut encoder = AnimationEncoder::new(resize.destination(), encoder_options)
        .map_err(TranscodeError::Encode)?;

    let mut frame_count = 0_u32;
    let mut total_duration = Duration::ZERO;
    while let Some(frame) = decoder.next_frame().map_err(TranscodeError::Decode)? {
        total_duration = total_duration
            .checked_add(frame.duration)
            .ok_or(TranscodeError::DurationOverflow)?;
        let mut rgba = frame.rgba;
        workspace
            .transform_rgba(&mut rgba)
            .map_err(TranscodeError::Resize)?;
        encoder
            .add_rgba(workspace.pixels(), frame.duration)
            .map_err(TranscodeError::Encode)?;
        frame_count = frame_count
            .checked_add(1)
            .ok_or(TranscodeError::FrameCountOverflow)?;
    }
    if frame_count != source.frame_count {
        return Err(TranscodeError::FrameCountMismatch {
            decoded: frame_count,
            declared: source.frame_count,
        });
    }

    let bytes = encoder.finish().map_err(TranscodeError::Encode)?;
    Ok(TranscodedAnimation {
        bytes,
        input: source,
        output_canvas: resize.destination(),
        frame_count,
        total_duration,
    })
}

/// Failure while transcoding an animated WebP sequence.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscodeError {
    /// The source animation could not be decoded.
    Decode(DecodeError),
    /// The source or destination resize operation could not be created/applied.
    Resize(ResizeError),
    /// The destination encoder could not be created or finished.
    Encode(EncodeError),
    /// The accumulated frame durations overflowed [`Duration`].
    DurationOverflow,
    /// The number of processed frames overflowed `u32`.
    FrameCountOverflow,
    /// The decoder produced a different number of frames than the source metadata declared.
    FrameCountMismatch {
        /// Number of frames actually produced by the decoder.
        decoded: u32,
        /// Number of frames declared by the source metadata.
        declared: u32,
    },
}

impl fmt::Display for TranscodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "animated WebP decode failed: {error}"),
            Self::Resize(error) => write!(f, "animated WebP resize failed: {error}"),
            Self::Encode(error) => write!(f, "animated WebP encode failed: {error}"),
            Self::DurationOverflow => f.write_str("animated WebP duration overflows Duration"),
            Self::FrameCountOverflow => f.write_str("animated WebP frame count overflows u32"),
            Self::FrameCountMismatch { decoded, declared } => write!(
                f,
                "decoder produced {decoded} frames; source declared {declared}"
            ),
        }
    }
}

impl Error for TranscodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::Resize(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::DurationOverflow | Self::FrameCountOverflow | Self::FrameCountMismatch { .. } => {
                None
            }
        }
    }
}
