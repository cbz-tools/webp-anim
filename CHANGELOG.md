# Changelog

All notable changes to `webp-anim` are documented here.

## Unreleased

## 0.1.1 - 2026-08-12

- Added `AnimationDecoder::reset` to reuse a decoder from the start of its
  stored sequence.
- Added `AnimationDecoder::frame_durations` to read frame-duration metadata
  without decoding RGBA pixels.
- Strengthened Demux metadata validation for frame counts, frame ordering,
  durations, and configured limits.
- Improved native decoder cleanup on initialization error paths.

## 0.1.0 - 2026-08-01

- Added the initial sequential animated-WebP inspection, decode, resize, and
  encode API.
- Added bounded resource limits for input, canvas, frame count, duration, and
  RGBA buffers.
- Added the high-level `transcode_animated_webp` composition API.
- Added GitHub Actions CI for Rust 1.85 and stable toolchains.
- Expanded public API Rustdoc and README usage and contract documentation.
