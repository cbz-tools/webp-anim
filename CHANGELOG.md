# Changelog

All notable changes to `webp-anim` are documented here.

The project is not published to crates.io yet. The first public release will
be recorded as `0.1.0` when it is published.

## Unreleased

- Added the initial sequential animated-WebP inspection, decode, resize, and
  encode API.
- Added bounded resource limits for input, canvas, frame count, duration, and
  RGBA buffers.
- Added the high-level `transcode_animated_webp` composition API.
- Added GitHub Actions CI for Rust 1.85 and stable toolchains.
- Expanded public API Rustdoc and README usage and contract documentation.
