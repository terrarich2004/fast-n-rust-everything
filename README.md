<h1 align="center">fast-n-rust (<code>fnr</code>)</h1>

<p align="center">
  <b>Rewrite the 5% that's slow — or lift the whole app to pure Rust.</b><br>
  Proven against real recorded behavior at every step. Built for humans and AI coding agents.
</p>

<p align="center">
  <a href="docs/README_RU.md">Русский</a> · <a href="#quick-start">Quick Start</a> · <a href="#why-fast-n-rust">Comparison</a> · <a href="#languages">14 Languages</a> · <a href="#mcp-server--agents">MCP & Agents</a> · <a href="NOTICE">Attribution (MIT)</a>
</p>

---

**`fast-n-rust`** (`fnr`) merges the best of [**`erkamyaman/rust-anything`**](https://github.com/erkamyaman/rust-anything) and [**`EL4CTEO/fast-anything`**](https://github.com/EL4CTEO/fast-anything) into a single Cargo workspace, unified CLI (`fast-n-rust` + short alias `fnr`), and threaded MCP server — then adds cross-engine synergies (`1 + 1 = 3`) and fixes upstream bugs.

- Need an instant 10–100× speedup on a slow function without touching the rest of your app? Use the **Fast 5% Hotspot Loop** (`fnr go`, `fnr scan`, `fnr port`, `fnr verify --gate`, `fnr enable`).
- Outgrew a single hotspot and want multi-core **Rayon `--batch`**, property **fuzzing**, **shadow traffic**, or **100% pure-Rust strangler-fig lift**? Run **`fnr escalate <id>`** or **`fnr app record` -> `app front-door` -> `app wire` -> `app lift`**.

## Why `fast-n-rust`?

| Capability | `fast-anything` | `rust-anything` | **`fast-n-rust` (`fnr`)** |
| :--- | :---: | :---: | :---: |
| Fast 5% hotspot profiler + 500 real-call recording + shrink/edge synthesis | ✅ | Partial | ✅ |
| Inline reversible 1-function swap (`enable` / `disable`) | ✅ | TS/Py/PS | ✅ (all 12 hotspot langs) |
| C++, R, Bun, Deno support | ✅ | ❌ | ✅ |
| PowerShell, Android JNI/NDK, `wasm-bindgen`, npm/PyPI `package` | ❌ | ✅ | ✅ |
| Multi-core Rayon `--batch` classes | ❌ | ✅ | ✅ |
| Whole-app strangler-fig (`app record` -> `front-door` -> `wire` -> `lift`) | ❌ | ✅ | ✅ |
| Property `fuzz`, `shadow` mode, `known-diffs`, `--float-tolerance` | ❌ | ✅ | ✅ |
| **Mode Advisor** (`fast` vs `--batch` vs `whole-app`) + FFI cost budget on `scan` | ❌ | ❌ | ✅ |
| **3-Tier Transport (`fa_direct` + `fa_bin` binary wire + `fa_call` JSON)** | ❌ | Partial (C/Go) | ✅ |
| **Portable `ship` LTO default + opt-in `--native-cpu` & `--pgo`** | ❌ | ❌ | ✅ |
| **Native Differential Fuzzing + Shrinking + `--save-regressions`** | ❌ | Unit-only | ✅ |
| **Unified `fnr status` (hotspots + transport/profile + `rust/` units)** | ❌ | ❌ | ✅ |
| **C/C++ `compile_commands.json` (`--cdb`) support** (CMake/Meson/Ninja) | ❌ | ❌ | ✅ |
| **Standalone `cargo test` parity harness (`tests/recorded_parity.rs`) + `fnr ci`** | ❌ | ❌ | ✅ |
| **Statistical `bench` (95% CI, CV%, Mann-Whitney p-value, peak RSS)** | ❌ | ❌ | ✅ |
| **`TASK.md` enriched with static risks, Performance Blueprint & `crates.toml`** | ❌ | ❌ | ✅ |
| **`fnr escalate <id>`** (promote `.fast/` port + deps to `rust/` unit crate) | ❌ | ❌ | ✅ |
| **Strict `--gate` (`fmt`, `clippy -D warnings`, `test`, `unsafe`) on hotspots** | ❌ | Unit-only | ✅ |
| **`fnr clean`** (`--cache`, `--ports`, `--rust`, `--all`, `--dry-run`) | ❌ (#76) | Partial | ✅ |
| Token-efficient 7-tool Compact MCP default (`--all-tools` for 30 legacy tools) | 8 tools | 23 tools | ✅ (7 compact / 30 full) |
| Upstream bug fixes (`rust-anything` #288–#291, #300–#301; `fast-anything` #76) | — | — | ✅ |

## When `fnr` helps — and when it doesn't

- **Big wins (10–200×):** CPU-heavy loops, parsing, fuzzy matching, scoring, compression, data pipelines, and CLI tools you want to lift to a standalone Rust binary (`fnr app`).
- **Won't speed up:** SQL/database queries, external HTTP/LLM API calls, or `async` network waits. `fnr scan` diagnoses these honestly (`optimize_io_first` / `extract_pure_helper`) instead of blindly pushing a Rust rewrite.
- **Requirement:** Compiling ports locally requires a Rust toolchain (`rustc`/`cargo`) and a system C linker (`cc` / Xcode CLT / VS Build Tools). `fnr doctor` checks only your current project's stack by default (use `--all` for all 14 languages).

## Quick Start

### 1. Install

**Prebuilt binaries (Linux / macOS / Windows — fastest, no Rust wait for CLI install):**
```sh
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/terrarich2004/fast-n-rust-everything/main/install.sh | sh

# Windows (PowerShell)
irm https://raw.githubusercontent.com/terrarich2004/fast-n-rust-everything/main/install.ps1 | iex
```

**With Rust / Cargo:**
```sh
cargo install --git https://github.com/terrarich2004/fast-n-rust-everything fast-n-rust
```

**From a local clone:**
```sh
cargo install --path crates/cli
```

### 2. Use it from your IDE chat (recommended)

```sh
fnr setup        # registers MCP + skills for Cursor, Claude Code, Antigravity, Windsurf, VS Code Copilot, Codex, Gemini, OpenCode
fnr doctor       # checks Rust + only the runtimes used in the current project (pass --all for all 14 languages)
```

Then open your project in your IDE and ask your AI coding agent:
> *"Profile and speed up this project with fast-n-rust"*

### 3. Or run from the terminal

```sh
fnr go --agent claude --strict -- python3 app.py
```

Or step by step (works with `python3`, `uv run`, `pytest`, `uvicorn`, `node`, `npm run`, `pnpm`, `yarn`, `tsx`, `ts-node`, etc.):

```sh
fnr scan -- python3 app.py                         # profiles app + FFI budget + bottleneck diagnosis
fnr scan --duration 10 -- uvicorn main:app        # timed profiling for HTTP servers & bots
fnr scan -- npm run bench                          # unwraps package.json scripts automatically
fnr scan --cdb build/compile_commands.json -- arg  # CMake/Meson/Ninja C/C++ projects
fnr port                                           # records up to 500 calls + Performance Blueprint + cargo test suite
# (write Rust in .fast/ports/<id>/src/lib.rs or src/port.rs; run `cargo test` inside the crate anytime)
fnr verify --gate --float-tolerance 1e-9           # parity + strict fmt/clippy/test/unsafe gate
fnr fuzz --cases 100 --save-regressions          # differential/property fuzz + auto-shrunk regressions
fnr enable                                         # portable Ship LTO (add `--native-cpu` or `--pgo`) & swap
fnr bench && fnr status                            # 95% CI + peak RSS benchmark, port matrix & deploy readiness audit
fnr commands                                       # list all 30 commands (batching, CI, packaging, whole-app lift)
```

### Measured E2E Benchmarks (Linux x86_64, Ryzen 7 4800HS)

| Example & Workload | Transport | Before | After | Speedup | Peak RSS / Notes |
| :--- | :--- | :---: | :---: | :---: | :--- |
| `examples/fuzzy-dedupe` (Python, 700 names) | PyO3 native | 4894.1ms | 23.4ms | **209.31×** | 13.6 MB RSS (`--pgo`: 25.2ms) |
| `examples/fuzzy-dedupe-ts` (Node, 2,000 names) | napi-rs native | 3379.0ms | 119.3ms | **28.32×** | 75.6 MB RSS |
| `examples/fuzzy-dedupe-cs` (C# .NET 10, 4,000 names) | `fa_bin` wire v1 | 3961.0ms | 186.3ms | **21.26×** | 38.7 MB RSS (28.8× per-call) |
| `examples/fuzzy-dedupe-java` (Java 25 FFM, 4,000 names) | `fa_bin` wire v1 | 4185.4ms | 1417.8ms | **2.95×** | 225.5 MB RSS (20.8× per-call; ~1.3s is `java App.java` compile/JVM startup) |
| `examples/fuzzy-dedupe-cpp` (C++, 6,000 names) | `fa_bin` wire v1 | 3941.7ms | 362.8ms | **10.87×** | **7.8 MB RSS** (matches pure C++; down from 25.6 MB JSON DOM) |
| `examples/mandelbrot-c` (C, 8 zooms × 2,000 iters) | `fa_direct` ABI | 6077.2ms | 163.0ms | **37.27×** | `--native-cpu` (`ship`: 204.9ms / 29.64×, `--pgo --native-cpu`: 177.5ms) |
| `examples/micro-ffi-c` (C, 300k tiny calls) | `fa_direct` vs `fa_call` | 54.92ms (C) | 56.95ms | 0.96× vs C | **3.23× faster** than JSON `fa_call` (`183.77ms` / `612.6ns/call`); blocked by safety guard unless `--force` |

**How to read these numbers (in plain terms):**
- **Why each language gets a different input size (`700` → `6,000` names):** Every example is scaled so the original app runs for ~3–5 seconds. Compiled `C++ -O2` checks `6,000` names (~18M pairs — **73× more work** than Python's `700` names / ~245k pairs), so `362.8ms` in C++ is solving a 73× larger workload than `23.4ms` in Python.
- **Why `mandelbrot-c` starts at `6077ms` instead of `431ms`:** This benchmark runs ~14× more iterations than older `fast-anything` tables, and Rust with 4-wide ILP + work-stealing beats `clang -O2` by **37.27×** (`163.0ms`).
- **Why Java takes `1417.8ms` end-to-end (`2.95×`):** The Rust function itself is **20.8× faster**, but `fnr bench` measures the entire process from launch to exit — and `java App.java` spends ~1.3s starting the JVM and compiling `.java` files on every run. Long-running servers or precompiled `.jar` apps get the full **20.8×** win.
- **Why `micro-ffi-c` is `0.96×`:** It is an intentional safety stress test. Calling a tiny 180ns C function 300,000 times one by one across FFI cannot beat inline C, so `fnr` automatically blocks enabling it and tells you to port the outer loop instead — while showing `fa_direct` is **3.23× faster** than JSON FFI.

> **Verification scope**: Live end-to-end verified on Linux x86_64 across all 6 runtimes installed on this host (C, C++, Python, TypeScript/Node, Java 25, C# .NET 10). Go, PHP, Ruby, R, and Kotlin adapters are verified via workspace unit tests; Windows/macOS were not tested on this host. `--native-cpu` gives a real SIMD win on CPU-bound kernels (`+20.4%` on `mandelbrot-c`), while `--pgo` did not improve these microbenchmarks (`177.5ms` vs `163.0ms` on C; `462.2ms` vs `362.8ms` on C++; `25.2ms` vs `23.4ms` on Python).

### 2. Escalate to Rayon `--batch`, fuzzing, or whole-app pure Rust

```sh
# Promote a hotspot port into a full `rust/<unit>-rs` crate with Rayon batching & imported calls:
fnr escalate app.dedupe --batch dedupe:names
fnr record rust/app-rs --inputs rust/app-rs/recorded-inputs.jsonl
fnr build rust/app-rs && fnr diff rust/app-rs && fnr fuzz rust/app-rs && fnr gate rust/app-rs

# Or migrate an entire CLI / HTTP server / library incrementally to standalone Rust:
fnr analyze . --whole-app
fnr app record . -- python3 cli.py
fnr app front-door .
fnr app wire . <unit> --command <name>
fnr app lift .
```

### Instant Kill Switches & Deploying Enabled Ports

- Hotspot swap: `FNR_DISABLE=1` (also honors `FAST_ANYTHING_DISABLE=1`), or `fnr disable --all`
- Unit / shadow swap: `RUST_ANYTHING_MODE=old` or `RUST_ANYTHING_UNIT_<NAME>=original` or `FNR_DISABLE=1`
- **Deploying to Docker / CI / PaaS:** `.fast/lib/` binaries are compiled for your local OS/arch and gitignored. Commit `.fast/ports/` sources, run `fnr build --deploy` in your `Dockerfile`/build step, and set `FNR_STRICT=1` so missing compiled ports fail loudly instead of silently falling back. (`fnr status` and `fnr enable` run an automatic deploy-readiness check; for JS/TS serverless/Edge hosts like Vercel/Netlify, prefer a container/VPS or a WASM crate via `fnr bridge --kind wasm`.)
- `FNR_LIB_DIR=/path/to/libs` overrides `.fast/lib` lookup at runtime.

## Languages

| Language | Fast 5% Hotspot Engine | Deep Unit + Whole-App Engine |
| :--- | :---: | :---: |
| **Python 3.10+** (`python`, `uv`, `poetry`) | PyO3 | PyO3, shadow, fuzz, lift |
| **JS / TypeScript** (Node 22+, Bun, Deno 2+) | napi-rs | napi-rs, wasm, shadow, fuzz, lift |
| **Go 1.22+** | C ABI + dlopen/loader | cgo + C ABI, shadow, lift |
| **Java 22+** (javac, Maven, Gradle) | C ABI + FFM API | JNI, shadow, lift |
| **Kotlin / JVM** (+ Android NDK/JNI) | C ABI + FFM API | JNI + Android ABI/APK test, lift |
| **Ruby 3.3+** | C ABI + Fiddle | magnus / C ABI, shadow, lift |
| **C** (`gcc`, `clang`, `icx`) | C ABI + dlopen | C ABI, shadow, lift |
| **C++** (`clang++`, `g++`, `icpx`) | C ABI + dlopen | App record/replay |
| **C# / .NET 8+** | C ABI + NativeLibrary | P/Invoke, app record/replay |
| **PHP 8.1+** (`extension=ffi`) | C ABI + FFI | FFI, app record/replay |
| **R 4.0+** (`Rscript`) | C ABI + `.C()` | — |
| **PowerShell 7.4+** (`pwsh`) | — | P/Invoke + Reflection.Emit, lift |

## MCP Server & Agents

Built-in threaded JSON-RPC 2.0 MCP server (`fast-n-rust mcp` / `fnr mcp`) with non-blocking `ping`, `notifications/cancelled`, per-thread isolated `notifications/progress`, per-workspace serialization of mutating calls, full tool `annotations`, structured error hints, **7 compact orchestrator tools by default** (`inspect`, `scaffold`, `check`, `switch`, `measure`, `ship`, `app` — cutting agent schema token usage by ~75%, with `--all-tools` exposing all **30 fine-grained tools**), **4 prompts** (`fast_optimize`, `port_unit`, `escalate_port`, `whole_app_rewrite`), and **5 MCP resources** (`fnr://guide/skill`, `fnr://guide/unit-porting`, `fnr://guide/whole-app`, `fnr://project/status`, `fnr://project/port-map`).

```sh
fnr setup                                  # auto-detects & configures installed agents + progressive-disclosure skills
fnr setup --agent claude --dry-run         # preview changes safely
fnr setup --remove                         # remove cleanly
fnr completions zsh > ~/.zfunc/_fnr        # shell completions (bash, zsh, fish, powershell, elvish)
```

Try it in 10 seconds on the bundled example:

```sh
fnr -C examples/fuzzy-dedupe scan -- python3 app.py
```

Agent presets for `fnr go --agent <name>`: `claude`, `codex`, `gemini`, `opencode`, `aider`, or any custom command reading a prompt from stdin.

## Build Cache Management

```sh
fnr clean --cache --dry-run   # inspect shared Cargo cache + .fast/target sizes
fnr clean --cache             # wipe Cargo build caches (fast-n-rust/target + .fast/target)
fnr clean --all               # clean all build caches + rust/*/target
fnr clean --ports             # remove .fast/ (blocks if any port is enabled unless --force)
```

## License & Attribution

MIT License — see [LICENSE](LICENSE) and [NOTICE](NOTICE).  
Built with deep appreciation for:
- [**erkamyaman/rust-anything**](https://github.com/erkamyaman/rust-anything) (Erkam Yaman, MIT)
- [**EL4CTEO/fast-anything**](https://github.com/EL4CTEO/fast-anything) (EL4CTEO, MIT)
