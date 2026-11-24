---
name: fast-n-rust
description: Rewrite the 5% that's slow — or lift the whole app to pure Rust — using fast-n-rust (fnr) CLI or MCP tools. Profiles Python, JS/TS (Node, Bun, Deno), Ruby, Java, Kotlin, Go, PHP, C, C++, C#, R, and PowerShell; records real calls + edge cases; verifies byte-for-byte / float-tolerance parity; supports Rayon batching, fuzzing, shadow mode, strict clippy/fmt gate, and strangler-fig whole-app lift.
---

# fast-n-rust (`fnr`)

Use **fast-n-rust** to speed up hot code or incrementally rewrite an app into pure Rust, proven against real behavior at every step. You write the Rust; `fnr` profiles (including C/C++ `compile_commands.json` via `--cdb`), estimates FFI budget across its 3-tier transport (`fa_direct` zero-JSON C ABI, `fa_bin` compact binary wire v1, `fa_call` JSON fallback), records, builds (portable `ship` LTO default + opt-in `--native-cpu` / `--pgo`), diffs, fuzzes (differential + auto-shrinking + `--save-regressions`), benches (with 95% CI + peak RSS), gates, packages, and swaps.

## Compact MCP Server (Default)

`fnr mcp` exposes **7 intent-based tools** (`inspect`, `scaffold`, `check`, `switch`, `measure`, `ship`, `app`) to save agent context tokens, while still dispatching all 29 legacy tool names (`scan`, `port`, `verify`, `gate`, `fuzz`, `enable`, `bench`, ...). Pass `fnr mcp --all-tools` if a client expects all 29 schemas listed.

## Decision Tree: Which Mode?

1. **Speed up slow code (80% of tasks — Fast 5% Hotspot Loop)**:
   - Run `fnr scan -- <cmd>` (or `--cdb build/compile_commands.json -- <app args>`) / MCP `inspect(action="scan")`.
   - Check the **Mode Advisor** output, `[direct ABI]` / `[binary wire]` tags, and FFI budget warnings:
     - Skip functions under 2% of runtime, blocked by I/O/randomness/globals, or **already compiled** (`@njit`, Cython, `@jax.jit`).
     - A tiny function called in a tight loop crosses FFI per call and will be slower unless you port its outer caller or escalate to `--batch`.
   - Run `fnr port <id>` (or `fnr go --agent <agent> -- <cmd>`): records up to 500 real calls + shrunk/edge cases, creates `.fast/ports/<id>/` with risk + Performance Blueprint `TASK.md` and `tests/recorded_parity.rs` (`cargo test` works directly on native crates).
   - Write the Rust in `src/lib.rs` (or `src/port.rs` for C-bridge languages).
   - Run `fnr verify <id> --gate` (`--float-tolerance`, `known-diffs.jsonl`, `--min-speedup`), `fnr fuzz <id> --save-regressions` (differential oracle + automatic shrinking), then `fnr enable <id>` (add `--native-cpu` for host SIMD or `--pgo` only when benchmarked faster) and `fnr bench` / `fnr status` (or `fnr ci --fuzz 50 --bench --min-speedup 2.0` / `fnr package <id>`).
   - Reversible any time via `fnr disable <id>` or `FNR_DISABLE=1` (`FAST_ANYTHING_DISABLE=1`).

2. **Hot loop / multi-core `--batch` / strict parity / shadow rollout (`escalate` & deep units)**:
   - Read [unit-porting.md](unit-porting.md) first.
   - Promote a hotspot with `fnr escalate <id> [--batch fn:param]` (automatically disables active inline patches and auto-splices compatible C/FFI `src/port.rs` code) or scaffold directly with `fnr bridge <repo> <unit-id>`.

3. **Whole app to pure Rust (CLIs, HTTP servers, libraries)**:
   - Read [whole-app.md](whole-app.md) and [unit-porting.md](unit-porting.md) first, then follow:
     `analyze --whole-app` -> `app record` -> `app front-door` -> port every unit -> `app wire` -> `app lift`.

## Core Invariants (All Modes)

- **Never edit recordings** (`recordings.jsonl` / `cases.jsonl`) to make a failing check pass. Fix the Rust port.
- **Never ship or enable a port that fails `verify`/`diff` or `gate`** (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, no unexplained `unsafe`, no `unwrap`/`expect`/`panic!` on fallible paths).
- **Watch semantic traps**:
  - JS/TS, Java, Kotlin, and C# index UTF-16 code units; Rust `&str` indexes UTF-8 bytes.
  - Regex `\w`, `\b`, `\d` and case-folding differ between Rust `regex` (Unicode-aware) and ASCII engines; use `fancy-regex` for lookaround/backreferences.
  - Return `Err("TypeName: message")` where the original throws/raises.
  - Avoid `unsafe`. If strictly required, isolate it with a `// SAFETY:` comment.
- Clean build caches any time with `fnr clean --cache`.
