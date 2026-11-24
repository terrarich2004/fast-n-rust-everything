---
description: Speed up the slow 5% in Rust or incrementally lift an app to pure Rust using fast-n-rust (fnr).
argument-hint: "<command or goal, e.g. python app.py | whole-app>"
---

Use the `fast-n-rust` skill (`skills/fast-n-rust/SKILL.md`) and MCP tools (or `fnr` CLI) to handle: `$ARGUMENTS`.

1. If the project is a web app, bundler (`next`, `vite`), or server/bot with no standalone CLI benchmark command, do NOT run `next dev` or an idle server — create a short `.fast/bench_driver.{py,ts}` exercising the slow code on realistic data OR pass `--duration 4 --load "<client cmd>"` to `scan`.
2. Inspect `advice.mode` and `advice.diagnosis`: if `optimize_io_first`, report the DB/network/async bottleneck honestly instead of forcing Rust; if `no_workload_exercised`, pass `--load` or a driver script; if `extract_pure_helper`, extract the pure compute block first and re-scan; otherwise follow `fast` vs `--batch` Rayon escalation vs `whole-app`.
3. Never ship or enable a port until parity verification (`verify` / `diff`, with `--float-tolerance` if needed) AND strict `gate` (`fmt`, `clippy -D warnings`, `cargo test`, `unsafe` SAFETY) pass; after `enable`, run `tsc --noEmit` (if `tsconfig.json` exists) and project tests.
4. Report end-to-end speedup (95% CI & peak RSS), per-call speedup, parity counts, `fnr status`, deploy readiness (`fnr deploy --write` for Docker/CI, or `--kind wasm` for browser/Vercel), and rollback (`fnr disable` or `FNR_DISABLE=1`).
