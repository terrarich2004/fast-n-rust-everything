---
name: fast-n-rust
description: Rewrite the 5% that's slow — or lift the whole app to pure Rust — using fast-n-rust (fnr) CLI or MCP tools. Profiles Python, JS/TS (Node, Bun, Deno), Ruby, Java, Kotlin, Go, PHP, C, C++, C#, R, and PowerShell; records real calls + edge cases; verifies byte-for-byte / float-tolerance parity; supports Rayon batching, fuzzing, shadow mode, strict clippy/fmt gate, Docker/CI deploy scaffolding, WASM, and strangler-fig whole-app lift.
---

# fast-n-rust (`fnr`)

Use **fast-n-rust** to speed up slow CPU code or turn a script/CLI into a standalone Rust binary, proven against real recorded behavior at every step. You write the Rust; `fnr` profiles (including C/C++ `compile_commands.json` via `--cdb`), estimates FFI budget across its 3-tier transport (`fa_direct` zero-JSON C ABI, `fa_bin` compact binary wire v1, `fa_call` JSON fallback), records real calls, builds (portable `ship` LTO default + opt-in `--native-cpu` / `--pgo`), diffs, fuzzes, benches (95% CI + peak RSS), gates, scaffolds Docker/CI deploy configs, and swaps cleanly.

## Compact MCP Server (Default)

`fnr mcp` exposes **7 intent-based tools** (`inspect`, `scaffold`, `check`, `switch`, `measure`, `ship`, `app`) to save agent context tokens, while still dispatching all 30 legacy tool names (`scan`, `port`, `verify`, `gate`, `ci`, `fuzz`, `enable`, `bench`, `deploy`, ...). Pass `fnr mcp --all-tools` if a client expects all 30 schemas listed.

## Autonomous IDE Rules (For Web Apps, Bots & Non-CLI Projects)

When a user asks to speed up a **Next.js, Vite, React, FastAPI, Django, Express, or Telegram/Discord bot** project from IDE chat:

1. **Never ask the user to invent a CLI benchmark command, and never run `next dev` or an idle server without traffic.**
   - **Option A (fastest & cleanest)**: Write a small 15–25 line driver script `.fast/bench_driver.py` (or `.fast/bench_driver.ts`) that imports the suspected slow function/handler and calls it with realistic sample inputs for ~1–2 seconds, then run `fnr scan -- python3 .fast/bench_driver.py` (or `fnr scan -- node .fast/bench_driver.ts`). Clean up `.fast/bench_driver.*` once finished, or leave it in `.fast/` (which is gitignored).
   - **Option B (live HTTP server)**: Run `fnr scan --duration 4 --load "curl -s http://127.0.0.1:PORT/..." -- <server-cmd>` (`--duration` and `--load` are automatically persisted for `port` and `bench`).
2. **Post-enable integrity check**: After `fnr enable <id>`, run `npx tsc --noEmit` if `tsconfig.json` exists, plus the project's existing test suite, and report `fnr status`.
3. **Browser / Vercel / Docker readiness**:
   - If targeting browser/client code or Vercel/Netlify/Cloudflare Edge, native `.node` addons will not load: escalate the port to WASM (`fnr escalate <id>` + `fnr bridge . <unit> --kind wasm`).
   - If deploying to Docker/VPS/GitHub Actions, run `fnr deploy --write` (or MCP `ship(action="deploy", write=true)`) to generate `Dockerfile.fnr` and `.github/workflows/fnr-deploy.yml`, or relay any `deploy` warnings from `fnr status`.

## Decision Tree: Which Mode?

1. **Speed up slow code (80% of tasks — Fast 5% Hotspot Loop)**:
   - Run `fnr scan -- <cmd>` (add `--duration N` + `--load "<client cmd>"` for servers/bots, or `--cdb build/compile_commands.json -- <app args>` for C/C++) / MCP `inspect(action="scan")`. Package/tool runners (`npm run`, `pnpm`, `yarn`, `tsx`, `ts-node`, `uv run`, `pytest`, `uvicorn`) are unwrapped automatically.
   - Check `advice.mode` and `advice.diagnosis`:
     - **`optimize_io_first`**: The bottleneck is I/O or `async` waits (DB/network/LLM API). Be honest with the user — **do NOT force a Rust port**; recommend caching, query batching, or parallelizing I/O instead.
     - **`no_workload_exercised`**: The timed server/bot sat idle with 0 calls recorded. Do NOT switch to whole-app migration — re-scan with `--load` or a `.fast/bench_driver` script.
     - **`extract_pure_helper`**: A top hotspot is blocked by a missing `if __name__ == "__main__":` / `if (import.meta.main)` guard, `mutates self/this`, or a pure sync loop inside an `async` handler. Extract that pure compute helper first, then re-run `scan`.
     - **`fast_hotspot` / `batch_hot_loop`**: Skip functions under 2% of runtime, blocked by I/O/randomness/globals, or **already compiled** (`@njit`, Cython, `@jax.jit`). A tiny function called in a tight loop crosses FFI per call and will be slower unless you port its outer caller or escalate to `--batch`.
   - Run `fnr port <id>`: records up to 500 real calls + shrunk/edge cases, creates `.fast/ports/<id>/` with risk + Performance Blueprint `TASK.md` and `tests/recorded_parity.rs`.
   - Write the Rust in `src/lib.rs` (or `src/port.rs` for C-bridge languages).
   - Run `fnr verify <id> --gate` (`--float-tolerance`, `known-diffs.jsonl`, `--min-speedup`), `fnr fuzz <id> --save-regressions`, then `fnr enable <id>` and `fnr bench` / `fnr status`.
   - Reversible any time via `fnr disable <id>` or `FNR_DISABLE=1` (`FAST_ANYTHING_DISABLE=1`). Use `FNR_STRICT=1` in CI/Docker to fail fast if the compiled port is missing.

2. **Hot loop / multi-core `--batch` / WASM / strict parity (`escalate` & deep units)**:
   - Read [unit-porting.md](unit-porting.md) first.
   - Promote a hotspot with `fnr escalate <id> [--batch fn:param]` or scaffold directly with `fnr bridge <repo> <unit-id> [--kind wasm]`.

3. **1-Prompt Standalone Binary / Whole App to Pure Rust (CLIs, HTTP servers, utilities)**:
   - When the user asks to turn a Python/JS script or CLI into a single fast standalone binary (`.exe` / native binary without Python/Node), read [whole-app.md](whole-app.md) and [unit-porting.md](unit-porting.md) and execute end-to-end:
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
