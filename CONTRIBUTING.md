# Contributing

## Development requirements

- Rust 1.85 or newer
- Rust 2024 Edition

The project uses the same checks locally and in CI:

```text
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo rustdoc --lib --locked -- -D missing_docs
cargo test --locked
cargo publish --dry-run --locked
```

The publish check requires a clean working tree. Before the first commit, use
`--allow-dirty` only for local package inspection.

## Change guidelines

- Keep animation metadata and frame durations explicit.
- Preserve the sequential, bounded-memory processing model.
- Document public API behavior, limits, and error conditions.
- Update [README.md](README.md) and [CHANGELOG.md](CHANGELOG.md) when a
  user-visible behavior or public API changes.
- Avoid adding application-specific playback, archive, or GUI policy to this
  library.
