# Contributing to `fast-n-rust` (`fnr`)

Thanks for helping improve `fast-n-rust`!

## Repository Layout

- `crates/fast-engine` (`fnr-fast`) — Fast 5% hotspot profiler, 500-call recorder, 3-tier FFI bridge (`fa_direct` / `fa_bin` / `fa_call`), inline swapping, and interleaved statistical benchmarker.
- `crates/migrate-engine` (`fnr-migrate`) — AST analyzer (`oxc` + `tree-sitter`), multi-language unit bridges, Rayon `--batch`, fuzz/diff/shadow/gate, and whole-app strangler-fig lift.
- `crates/cli` (`fast-n-rust` / `fnr`) — Unified CLI, cross-engine synergy (`synergy.rs`), threaded MCP server (`mcp.rs`), `setup`, `doctor`, and `clean`.
- `skills/fast-n-rust/` — Progressive-disclosure workflow skills embedded into the binary for AI coding agents.
- `examples/` — Self-contained benchmark & smoke-test projects across supported languages.
- `npm/fast-n-rust/` — Node launcher and postinstall release downloader (`npx fast-n-rust`).

## Development & Verification

Before opening a pull request, run the full workspace checks from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node --test npm/fast-n-rust/bin/fast-n-rust.test.js
```

## Guidelines

- Keep `crates/cli` and `crates/migrate-engine` free of `unsafe` (`#![forbid(unsafe_code)]` via Cargo lints). Any `unsafe` inside `crates/fast-engine` or generated FFI crates must be minimal and documented with a `// SAFETY:` comment.
- Do not commit `.fast/`, `.fnr/`, `.rust-anything/`, `.gradle/`, `__pycache__/`, or `target/` artifacts (test examples against temporary copies).
- Preserve compatibility with `FNR_*` as well as upstream `FAST_ANYTHING_*` and `RUST_ANYTHING_*` environment variables and manifests.
