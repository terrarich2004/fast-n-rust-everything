# Changelog

## v0.2.3 — 2026-11-24

Focus of this release: make `fnr` work out of the box for day-to-day Python, TypeScript, and server projects instead of tripping on setup, strict `tsc`, or hanging runs.

### Fixed
- **Timed servers & bots no longer hang on `fnr port` or `fnr bench`:** `--duration` and `--load` are now saved in `.fast/state.json`. Both Python and Node/TS recording respect `--duration` (and stop early once enough samples are captured), and `fnr bench` either benchmarks the server against `--load` or exits with the per-call speedup instead of waiting forever.
- **Idle servers no longer get told to rewrite everything to Rust:** if a timed scan catches zero calls from your code, Mode Advisor now reports `no_workload_exercised` and points to `--load` or a small driver script.
- **TypeScript `strict: true` compatibility on `fnr enable`:** removed untyped arrow functions and `unknown` catch-property access from generated shims, added typed wrappers for `.ts` methods, and switched top-level `export function f` declarations to reversible `export let f = function f` (with byte-for-byte restore on `fnr disable`).
- **`fnr doctor` false positive on PowerShell:** `install.ps1` and non-app folders (`examples`, `docs`, `npm`, `skills`) next to `Cargo.toml` or `package.json` no longer mark the repo as a PowerShell project on Linux/macOS. Missing `cargo`/`cc` hints now show concrete OS-specific install commands.
- **GUI IDE `PATH` lookups:** `fnr` and `fnr mcp` now append `~/.cargo/bin`, `~/.local/bin`, and Homebrew paths on startup so Cursor, Antigravity, Windsurf, and Claude Desktop find `cargo` and `cc`.
- **Symlinked project roots:** resolved symlinks when matching Python and Node project files during `scan` and `profile` (fixes `/tmp` -> `/private/tmp` on macOS and symlinked workspaces).

### Added
- **`--load "<client cmd>"` for `scan` and `go`:** drives traffic against HTTP servers or bots automatically during `scan`, `port`, and `bench`.
- **Typed TypeScript structs & call-based inference:** `fnr port` extracts plain TS `interface` / object `type` definitions into `#[napi(object)]` structs, infers default parameter types as `Option<T>`, and falls back to recorded call types when JS annotations are missing.
- **`fnr deploy` (and MCP `ship(action="deploy")`):** previews or writes `Dockerfile.fnr` (with `fnr build --deploy` and `FNR_STRICT=1`) and `.github/workflows/fnr-deploy.yml`, and warns with WASM conversion steps when targeting Vercel/Netlify/Edge.
- **Installer `PATH` updates & clearer guidance:** `install.sh` and `install.ps1` add the install folder to `PATH` automatically, bundler errors (`next`/`vite`) and IDE skills instruct agents to synthesize a `.fast/bench_driver` or pass `--load`, and READMEs lead with practical workflows.
