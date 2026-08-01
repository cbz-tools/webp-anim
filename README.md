# webp-anim

[![CI](https://github.com/cbz-tools/webp-anim/actions/workflows/ci.yml/badge.svg)](https://github.com/cbz-tools/webp-anim/actions/workflows/ci.yml)
[![Documentation](https://docs.rs/webp-anim/badge.svg)](https://docs.rs/webp-anim)

`webp-anim` is a reusable foundation for sequential, bounded-memory processing
of animated WebP frame sequences.

It preserves animation semantics while frames are inspected, decoded, resized,
transformed, and re-encoded: frame order, source durations, loop count, and ANIM
background color remain explicit parts of the processing contract.

The input is a complete WebP byte slice; decoded frames are processed one at a
time instead of being retained as a complete RGBA sequence.

Unlike a general-purpose WebP codec wrapper, this crate focuses on the
animation-aware processing pipeline above the codec layer. It keeps other
application concerns out of the crate: archive handling, GUI frameworks,
playback queues, and application-specific output policies are not included.

## Positioning

The crate is intended to sit between a WebP codec implementation and an
application-level viewer, converter, or optimizer:

```text
WebP codec
    ↓
webp-anim
    inspect → sequential decode → bounded frame transform → ordered encode
    ↓
consuming application
```

## API at a glance

- `inspect` classifies a complete WebP byte slice as static or animated and
  returns canvas and animation metadata without decoding every frame.
- `AnimationDecoder` reads one stored animated sequence frame by frame as
  composited, full-canvas RGBA buffers.
- `ResizePlan` derives one aspect-ratio-preserving resize operation;
  `ResizeWorkspace` reuses its destination allocation across frames.
- `AnimationEncoder` accepts full-canvas RGBA frames in presentation order
  and writes an animated WebP byte vector.
- `transcode_animated_webp` composes those stages for the common sequential
  decode-resize-encode path.

The modules under `codec`, `inspect`, `model`, `resize`, and `transcode`
are public for applications that need the lower-level types. The same primary
types are re-exported at the crate root for the common case.

## Features

- Inspect static and animated WebP containers.
- Decode one stored animation sequence frame by frame.
- Return composited full-canvas RGBA frames.
- Preserve source frame durations without playback-time normalization.
- Resize animation frames with a reusable plan and workspace.
- Encode animated WebP frames in presentation order.
- Preserve animation loop count and ANIM background color when transcoding.
- Apply configurable input, canvas, frame-count, and RGBA buffer limits.
- Avoid retaining the complete decoded RGBA frame sequence in the processing path.

The decoder processes one stored animation sequence and reports its loop count,
but does not replay frames according to that count. Playback and repetition
policy belong to the consuming application.

## Input and output contract

All processing APIs accept a complete WebP byte slice. `inspect` accepts both
static and animated WebP images. `AnimationDecoder`, `AnimationEncoder`, and
`transcode_animated_webp` operate on exactly one stored animated sequence;
they do not implement playback, loop replay, or application-level frame
queues.

Decoded frames are owned, tightly packed RGBA8 buffers in row-major,
full-canvas order. A frame for a `width × height` canvas therefore contains
`width * height * 4` bytes. The decoder returns the source frame duration
without applying a playback minimum or other normalization.

The encoder requires durations that are an exact whole number of milliseconds.
It converts each duration to the cumulative WebP timestamp and flushes the
last frame duration when `AnimationEncoder::finish` is called. Every frame
must use the encoder canvas and the exact RGBA buffer length.

`AnimationInfo::loop_count` and `AnimationInfo::background_color` describe the
stored WebP animation metadata. `transcode_animated_webp` copies both values
to the output. `BackgroundColor::raw` is preserved verbatim; this crate does
not assign a channel order or color-space interpretation to it.

## Usage

Add the dependency:

```toml
[dependencies]
webp-anim = "0.1"
```

Inspect an input:

```rust
use webp_anim::{InspectLimits, WebpKind, inspect};

fn classify(input: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    match inspect(input, InspectLimits::default())? {
        WebpKind::Static(info) => {
            println!("static WebP: {}x{}", info.canvas.width, info.canvas.height);
        }
        WebpKind::Animated(info) => {
            println!(
                "animated WebP: {}x{}, {} frames",
                info.canvas.width, info.canvas.height, info.frame_count
            );
        }
    }
    Ok(())
}
```

Process an animation one frame at a time:

```rust
use webp_anim::{AnimationDecoder, DecodeLimits};

fn process_frames(input: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut decoder = AnimationDecoder::new(input, DecodeLimits::default())?;
    let info = *decoder.info();

    println!("{} frames on a {}x{} canvas", info.frame_count, info.canvas.width, info.canvas.height);

    while let Some(frame) = decoder.next_frame()? {
        // Transform or consume this frame before requesting the next one.
        let _rgba = frame.rgba;
        let _duration = frame.duration;
    }

    Ok(())
}
```

The decoder owns only the input bytes and the current decoding state; callers
can consume or hand off each decoded frame without collecting the complete
animation in memory.

Resize and transcode one animation sequence:

```rust
use webp_anim::{
    AnimationTranscodeOptions, CanvasSize, ResizeOptions, transcode_animated_webp,
};

fn transcode(input: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let options = AnimationTranscodeOptions::new(ResizeOptions::contain(CanvasSize {
        width: 1600,
        height: 2560,
    }));
    Ok(transcode_animated_webp(input, options)?.bytes)
}
```

`transcode_animated_webp` always produces an encoded result, including when
the resize operation is a no-op. The caller decides whether the encoded bytes
should replace the original input, for example by comparing output sizes.

For a keep-or-replace policy, the consuming application can make that decision
after the transcode:

```rust
let result = transcode_animated_webp(input, options)?;
let output = if result.bytes.len() < input.len() {
    result.bytes
} else {
    input.to_vec()
};
```

## Semantics and limits

Frames are returned as composited, full-canvas RGBA buffers. Frame durations are
the source durations and are not adjusted for an application's minimum playback
delay. The source loop count and ANIM background color are represented as
explicit values and are retained by the transcode path.

The decoder applies configurable resource limits before and during processing.
Applications should choose limits appropriate for their own input trust model
and process-wide memory budget. The limits do not claim to bound allocations
inside libwebp itself.

The default decoder and inspection limits are:

| Limit | Default |
| --- | ---: |
| Input bytes | 256 MiB |
| Canvas pixels | 100,000,000 |
| Stored frame count | 10,000 |
| Total source duration | 1 hour (decoder only) |
| RGBA bytes per frame | 400 MiB |

`ResizeOptions::max_output_rgba_bytes` adds a destination-buffer limit for a
resize plan. `DecodeLimits::for_trusted_input` and
`InspectLimits::for_trusted_input` relax the crate-level checks, but do not
bypass libwebp or platform allocation limits. Do not use them as a substitute
for an application-wide resource policy when processing untrusted input.

Each fallible stage returns a typed error: `InspectError`, `DecodeError`,
`ResizeError`, `EncodeError`, or `TranscodeError`. The fast classifier
`is_animated_webp_fast` is intended only for routing and returns `false` for
malformed input; use `inspect` when the reason for rejection matters.

## Low-level sequential pipeline

Use a reusable `ResizePlan` and `ResizeWorkspace` when processing many
frames. The workspace retains its resize state and destination buffer, while
the caller can hand each output frame directly to an `AnimationEncoder`:

```rust
use std::time::Duration;

use webp_anim::{
    AnimationEncoder, AnimationEncoderOptions, CanvasSize, LoopCount,
    BackgroundColor, ResizeOptions, ResizePlan,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = CanvasSize { width: 320, height: 240 };
    let plan = ResizePlan::new(
        source,
        ResizeOptions::contain(CanvasSize { width: 160, height: 120 }),
    )?;
    let mut workspace = plan.workspace()?;
    let mut encoder = AnimationEncoder::new(
        plan.destination(),
        AnimationEncoderOptions::new(LoopCount::Infinite, BackgroundColor { raw: 0 }),
    )?;

    // For each decoded full-canvas frame:
    let mut rgba = vec![0; source.rgba_bytes().unwrap()];
    let duration = Duration::from_millis(100);
    workspace.transform_rgba(&mut rgba)?;
    encoder.add_rgba(workspace.pixels(), duration)?;

    let _encoded_webp = encoder.finish()?;
    Ok(())
}
```

The example shows the ownership and buffer contract; a real application would
replace the placeholder `rgba` buffer with each frame returned by
`AnimationDecoder::next_frame`.

## Scope

This crate provides an animation-aware processing foundation. Playback control,
GUI integration, worker lifecycle, archive orchestration, product presets, and
application-wide concurrency or memory policies remain in the consuming
application.

`transcode_animated_webp` is one convenience composition of the lower-level
primitives, not the only processing pipeline. Applications with different
policies can combine `AnimationDecoder`, `ResizePlan`, `ResizeWorkspace`, and
`AnimationEncoder` directly.

## Reference integrations

The following public applications provide real-world integration examples:

- [cbz-tools-viewer](https://github.com/cbz-tools/cbz-tools-viewer)
- [cbz-tools-optimizer](https://github.com/cbz-tools/cbz-tools-optimizer)

These projects show application-level playback and optimization policies while
`webp-anim` remains independent of their archive, GUI, and product-specific
code.

## Requirements

- Rust 1.85 or newer
- Rust 2024 Edition

The crate uses `libwebp-sys` for WebP decoding and encoding.

## License

Licensed under the [MIT License](LICENSE-MIT).
