---
description: Speed up the slow 5% in Rust or incrementally lift an app to pure Rust using fast-n-rust (fnr).
argument-hint: "<command or goal, e.g. python app.py | whole-app>"
---

Use the `fast-n-rust` skill (`skills/fast-n-rust/SKILL.md`) and MCP tools (or `fnr` CLI) to handle: `$ARGUMENTS`.

1. If optimizing a slow workload, run `scan` first (`--cdb build/compile_commands.json` for C/C++ CMake/Meson/Ninja) and follow the Mode Advisor (`fast` 5% hotspot with `[direct ABI]` `fa_direct` / `[binary wire]` `fa_bin` vs `--batch` Rayon escalation vs `whole-app`).
2. Never ship or enable a port until parity verification (`verify` / `diff`, with `--float-tolerance` if needed) AND strict `gate` (`fmt`, `clippy -D warnings`, `cargo test`, `unsafe` SAFETY) pass; run `fnr fuzz --save-regressions` or `fnr ci` for extra confidence.
3. Prefer portable `ship` default for packages, or opt into `--native-cpu` (`enable --native-cpu`) / `--pgo` only when measured faster; report end-to-end speedup (with 95% CI & peak RSS), per-call speedup, parity counts, `fnr status` transport tier, and how to roll back (`fnr disable` or `FNR_DISABLE=1`).
