use std::{num::NonZeroU16, time::Duration};

/// Pixel dimensions of a full animation canvas.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanvasSize {
    /// Canvas width in pixels.
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
}

impl CanvasSize {
    /// Returns `width * height`, or `None` if the multiplication overflows.
    pub fn pixel_count(self) -> Option<u64> {
        u64::from(self.width).checked_mul(u64::from(self.height))
    }

    /// Returns the tightly packed RGBA8 buffer size, or `None` on overflow.
    pub fn rgba_bytes(self) -> Option<usize> {
        self.pixel_count()
            .and_then(|pixels| pixels.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
    }
}

/// WebP loop count without exposing the file format's `0 == infinite` sentinel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopCount {
    /// The animation repeats indefinitely according to the WebP loop value.
    Infinite,
    /// A finite, non-zero loop count stored by WebP.
    Finite(NonZeroU16),
}

/// Background colour stored verbatim in libwebp's `bgcolor` field.
///
/// This crate deliberately does not assign a channel order or color-space
/// interpretation to this value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackgroundColor {
    /// Raw 32-bit value stored in the WebP ANIM background-color field.
    pub raw: u32,
}

/// Semantics associated with one stored animation sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnimationInfo {
    /// Full canvas dimensions shared by every composited frame.
    pub canvas: CanvasSize,
    /// Number of frames in the stored animation sequence.
    pub frame_count: u32,
    /// Loop policy stored in the source animation.
    pub loop_count: LoopCount,
    /// Raw ANIM background color stored in the source animation.
    pub background_color: BackgroundColor,
}

/// Information available for a non-animated WebP image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaticWebpInfo {
    /// Canvas dimensions of the static image.
    pub canvas: CanvasSize,
}

/// A composited, full-canvas RGBA frame and its unmodified source duration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnimationFrame {
    /// Tightly packed RGBA8 pixels in row-major, full-canvas order.
    pub rgba: Vec<u8>,
    /// Canvas dimensions of [`Self::rgba`].
    pub canvas: CanvasSize,
    /// Source frame duration, without playback-time normalization.
    pub duration: Duration,
}
