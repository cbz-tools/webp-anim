use std::{error::Error, fmt};

use fast_image_resize::{
    FilterType, PixelType, ResizeAlg, ResizeOptions as FirResizeOptions, Resizer,
    images::Image as FirImage,
};

use crate::model::{AnimationFrame, CanvasSize};

/// Resampling filter used by a [`ResizePlan`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ResizeFilter {
    /// Blend nearby source pixels. Fast and smooth.
    #[default]
    Bilinear,
    /// A sharper bicubic filter with a small risk of haloing around hard edges.
    CatmullRom,
    /// A high-detail sinc filter. Best for visual comparison, but slowest and
    /// can ring around very high-contrast edges.
    Lanczos3,
}

/// Constraints used to derive a full-canvas resize operation.
///
/// The resulting canvas always fits within `maximum` and retains the source
/// aspect ratio. A plan never adds padding or crops pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResizeOptions {
    /// Maximum destination canvas dimensions.
    pub maximum: CanvasSize,
    /// Whether a source smaller than `maximum` may be enlarged.
    pub allow_upscale: bool,
    /// Resampling filter used by the resize operation.
    pub filter: ResizeFilter,
    /// Upper bound for the transformed RGBA buffer.
    pub max_output_rgba_bytes: usize,
}

impl ResizeOptions {
    /// Creates a contain-style resize with no upscaling and bilinear filtering.
    pub const fn contain(maximum: CanvasSize) -> Self {
        Self {
            maximum,
            allow_upscale: false,
            filter: ResizeFilter::Bilinear,
            max_output_rgba_bytes: usize::MAX,
        }
    }
}

/// A validated, reusable full-canvas RGBA resize operation.
///
/// Build one plan per source canvas, then apply it to each composited frame of
/// an animation. [`Self::transform_frame`] preserves the frame duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResizePlan {
    source: CanvasSize,
    destination: CanvasSize,
    filter: ResizeFilter,
}

impl ResizePlan {
    /// Validates resize options and derives the destination canvas.
    ///
    /// The destination preserves the source aspect ratio, fits within the
    /// configured maximum, and never crops or pads pixels.
    pub fn new(source: CanvasSize, options: ResizeOptions) -> Result<Self, ResizeError> {
        validate_size("source", source)?;
        validate_size("maximum", options.maximum)?;

        let destination = destination_size(source, options.maximum, options.allow_upscale);
        let output_bytes = destination
            .rgba_bytes()
            .ok_or(ResizeError::OutputSizeOverflow)?;
        if output_bytes > options.max_output_rgba_bytes {
            return Err(ResizeError::OutputTooLarge {
                actual: output_bytes,
                maximum: options.max_output_rgba_bytes,
            });
        }

        Ok(Self {
            source,
            destination,
            filter: options.filter,
        })
    }

    /// Returns the source canvas expected by this plan.
    pub const fn source(&self) -> CanvasSize {
        self.source
    }

    /// Returns the destination canvas produced by this plan.
    pub const fn destination(&self) -> CanvasSize {
        self.destination
    }

    /// Returns whether the plan preserves the source dimensions.
    pub const fn is_noop(&self) -> bool {
        self.source.width == self.destination.width && self.source.height == self.destination.height
    }

    /// One-shot convenience API for one complete, tightly packed RGBA canvas.
    ///
    /// It allocates a workspace and output buffer for this call. For sequential
    /// animation frames, create one [`ResizeWorkspace`] with [`Self::workspace`]
    /// and reuse it instead.
    pub fn transform_rgba(&self, source_rgba: &[u8]) -> Result<Vec<u8>, ResizeError> {
        let mut source = source_rgba.to_vec();
        let mut workspace = self.workspace()?;
        workspace.transform_rgba(&mut source)?;
        Ok(workspace.pixels().to_vec())
    }

    /// Creates reusable resize state for sequential animation frames.
    ///
    /// Reuse this workspace for the full animation to retain its resizer and
    /// destination buffer between frames.
    pub fn workspace(&self) -> Result<ResizeWorkspace, ResizeError> {
        let destination_bytes = self
            .destination
            .rgba_bytes()
            .ok_or(ResizeError::OutputSizeOverflow)?;
        Ok(ResizeWorkspace {
            plan: *self,
            resizer: Resizer::new(),
            destination: vec![0; destination_bytes],
        })
    }

    /// One-shot convenience API that transforms a decoded frame and preserves
    /// its duration.
    ///
    /// It allocates for every call. For animation processing, prefer a reused
    /// [`ResizeWorkspace`] and construct the output frame at the call site.
    pub fn transform_frame(&self, frame: AnimationFrame) -> Result<AnimationFrame, ResizeError> {
        if frame.canvas != self.source {
            return Err(ResizeError::UnexpectedFrameCanvas {
                actual: frame.canvas,
                expected: self.source,
            });
        }
        Ok(AnimationFrame {
            rgba: self.transform_rgba(&frame.rgba)?,
            canvas: self.destination,
            duration: frame.duration,
        })
    }
}

/// Reusable RGBA resize buffers and processor for sequential animation frames.
///
/// The workspace owns its destination buffer. [`Self::pixels`] remains valid
/// until the next successful resize or until the workspace is dropped.
pub struct ResizeWorkspace {
    plan: ResizePlan,
    resizer: Resizer,
    destination: Vec<u8>,
}

impl ResizeWorkspace {
    /// Resizes a full-canvas RGBA source into this workspace's reusable
    /// destination buffer.
    ///
    /// [`fast_image_resize::images::Image::from_slice_u8`] requires a mutable
    /// buffer for every image view, including the source view. This method uses
    /// that contract, but directs its output to the workspace destination rather
    /// than modifying `source_rgba`. The destination allocation and resizer are
    /// retained for the next frame.
    pub fn transform_rgba(&mut self, source_rgba: &mut [u8]) -> Result<(), ResizeError> {
        let expected = self
            .plan
            .source
            .rgba_bytes()
            .ok_or(ResizeError::SourceSizeOverflow)?;
        if source_rgba.len() != expected {
            return Err(ResizeError::InvalidSourceBufferLength {
                actual: source_rgba.len(),
                expected,
            });
        }
        if self.plan.is_noop() {
            self.destination.copy_from_slice(source_rgba);
            return Ok(());
        }
        let has_transparency = source_rgba.chunks_exact(4).any(|pixel| pixel[3] != 255);
        let source = FirImage::from_slice_u8(
            self.plan.source.width,
            self.plan.source.height,
            source_rgba,
            PixelType::U8x4,
        )
        .map_err(|error| ResizeError::ImageResize(error.to_string()))?;
        let mut destination = FirImage::from_slice_u8(
            self.plan.destination.width,
            self.plan.destination.height,
            &mut self.destination,
            PixelType::U8x4,
        )
        .map_err(|error| ResizeError::ImageResize(error.to_string()))?;
        let filter = match self.plan.filter {
            ResizeFilter::Bilinear => FilterType::Bilinear,
            ResizeFilter::CatmullRom => FilterType::CatmullRom,
            ResizeFilter::Lanczos3 => FilterType::Lanczos3,
        };
        let options = FirResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(filter))
            .use_alpha(has_transparency);
        self.resizer
            .resize(&source, &mut destination, &options)
            .map_err(|error| ResizeError::ImageResize(error.to_string()))?;
        Ok(())
    }

    /// Returns the destination pixels from the most recent successful resize.
    ///
    /// The next successful [`Self::transform_rgba`] call overwrites this buffer.
    pub fn pixels(&self) -> &[u8] {
        &self.destination
    }
}

/// Failure to create or apply a [`ResizePlan`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResizeError {
    /// A source or maximum canvas has a zero dimension.
    InvalidCanvasSize {
        /// Which input was invalid.
        which: &'static str,
        /// Invalid dimensions.
        size: CanvasSize,
    },
    /// The source RGBA byte count overflowed the host address space.
    SourceSizeOverflow,
    /// The destination RGBA byte count overflowed the host address space.
    OutputSizeOverflow,
    /// The destination exceeds `ResizeOptions::max_output_rgba_bytes`.
    OutputTooLarge {
        /// Actual destination buffer size in bytes.
        actual: usize,
        /// Configured maximum destination buffer size in bytes.
        maximum: usize,
    },
    /// The source buffer length does not match the plan's source canvas.
    InvalidSourceBufferLength {
        /// Actual source buffer size in bytes.
        actual: usize,
        /// Required source buffer size in bytes.
        expected: usize,
    },
    /// A frame's canvas does not match the plan's source canvas.
    UnexpectedFrameCanvas {
        /// Canvas received from the caller.
        actual: CanvasSize,
        /// Canvas required by the plan.
        expected: CanvasSize,
    },
    /// The underlying RGBA resizer rejected the operation.
    ImageResize(String),
}

impl fmt::Display for ResizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCanvasSize { which, size } => write!(
                f,
                "{which} canvas has invalid dimensions {}x{}",
                size.width, size.height
            ),
            Self::SourceSizeOverflow => {
                f.write_str("source RGBA size overflows the host address space")
            }
            Self::OutputSizeOverflow => {
                f.write_str("output RGBA size overflows the host address space")
            }
            Self::OutputTooLarge { actual, maximum } => write!(
                f,
                "output is {actual} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::InvalidSourceBufferLength { actual, expected } => {
                write!(f, "source buffer is {actual} bytes; expected {expected}")
            }
            Self::UnexpectedFrameCanvas { actual, expected } => write!(
                f,
                "frame canvas {}x{} does not match plan source {}x{}",
                actual.width, actual.height, expected.width, expected.height
            ),
            Self::ImageResize(message) => write!(f, "RGBA resize failed: {message}"),
        }
    }
}

impl Error for ResizeError {}

fn validate_size(which: &'static str, size: CanvasSize) -> Result<(), ResizeError> {
    if size.width == 0 || size.height == 0 {
        return Err(ResizeError::InvalidCanvasSize { which, size });
    }
    Ok(())
}

fn destination_size(source: CanvasSize, maximum: CanvasSize, allow_upscale: bool) -> CanvasSize {
    let width_limited = u64::from(maximum.width) * u64::from(source.height)
        <= u64::from(maximum.height) * u64::from(source.width);
    let (numerator, denominator) = if width_limited {
        (maximum.width, source.width)
    } else {
        (maximum.height, source.height)
    };
    if !allow_upscale && numerator >= denominator {
        return source;
    }
    CanvasSize {
        width: u32::try_from(
            (u64::from(source.width) * u64::from(numerator) / u64::from(denominator)).max(1),
        )
        .expect("scaled width fits u32"),
        height: u32::try_from(
            (u64::from(source.height) * u64::from(numerator) / u64::from(denominator)).max(1),
        )
        .expect("scaled height fits u32"),
    }
}
