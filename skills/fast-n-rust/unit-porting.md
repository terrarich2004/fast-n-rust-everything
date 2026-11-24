# Deep Unit & Rayon Batch Porting (`fnr`)

Read this guide whenever working on `rust/<unit>` crates, `fnr escalate`, `--batch`, `shadow`, `diff`, or `gate`.

## 0. Profile & Analyze Before Writing Code

```sh
fnr profile <repo> -- node bench.mjs          # or python3, ruby, java, go test
fnr analyze <repo> -o plan.md                 # human plan
fnr analyze <repo> --json                     # machine plan (start_here, keep, blocked, crates)
```

- **Already compiled code** (`@njit`, Cython, `@jax.jit`) already runs as machine code: `profile` flags it — warn the user before porting it for speed.
- **Respect `keep`, `blocked`, and `fit: none` / `unknown` dependencies**: UI/framework glue stays in the source language. Do not silently hand-roll a missing library; present the `start_here` wave and confirm with the user first.

## 1. Scaffold & Record Ground Truth

```sh
fnr bridge <repo> <unit-id>                   # creates <repo>/rust/<name>; never overwrites port.rs
fnr record <crate>                            # records edge cases + semantic traps -> cases.jsonl
```

- **Record existing test suites** when available:
  `fnr record <crate> --from-tests -- pytest` (or `-- npx jest`).
- **Domain inputs**: pass real samples via `fnr record <crate> --inputs real.jsonl` (`{"fn": "...", "args": [...]}` per line).
- **TypeScript module resolution**: if Node cannot execute TS directly (path aliases, extensionless imports), pass `--module dist/x.mjs` or `--node-arg=--import=tsx`.
- Write your port in `src/port.rs` only (`src/lib.rs` and language wrappers are generated). After changing any signature in `port.rs`, run `fnr refresh <crate>`.

## 2. Language Bridge Rules (`src/port.rs`)

- **Read every risk** `analyze` reports at `file:line` before coding.
- **Python numbers**: ints passed through must return as ints, not floats. Use `crate::pynum::Num` for int-or-float parameters/returns instead of `serde_json::Value`.
- **UTF-16 string offsets** (JS/TS, Java, Kotlin, C#): use `src/utf16.rs` helpers rather than Rust UTF-8 byte offsets.
- **Errors**: return `Err("TypeName: message")` so Python/Ruby/Java raise that exception type and Go returns the `error` string. Never panic (`panic!` never matches a recorded exception).
- **C units**: inputs recorded as `crash:` represent original undefined behavior; any non-crashing result passes `diff`. Never panic across the C ABI boundary.
- **Ordered objects/maps**: use `indexmap::IndexMap` when insertion order reaches serialized output.
- **Regex semantics**: use `fancy-regex` for lookaround or backreferences. Watch Turkish `ı`/`İ`, Kelvin `K`, joiners, and non-ASCII `\w`/`\d`/`\b` (set `.unicode(false)` or spell `[A-Za-z0-9_]` where the original engine is ASCII-only, such as JS without `/u` or Java). `record` automatically injects regex trap cases.

## 3. Multi-Core Rayon Batching (`--batch` & `FnIndex`)

For functions called in tight loops over collections (search, scoring, formatting, deduplication):

```sh
fnr bridge <repo> <unit> --batch fn:param
# or promote an existing hotspot:
fnr escalate <id> --batch fn:param
# or add to an existing crate:
fnr refresh <crate> --batch fn:param
```

This generates a stateful batch wrapper (`new FnBatch(items)` in TS/Python/Java, `Mod::FnBatch.new(items)` in Ruby, `NewFnBatch(items)` in Go) that stores `param` once in Rust and runs across all CPU cores via Rayon:
- `run(<other args>)` returns every item's result in order.
- `rank(<other args>, min)` returns item indexes sorted high-to-low (ties broken in original item order).
- **Build the batch once** outside query loops, call `close()` in Go/Java when finished, and map indexes in JS with a plain `for` loop (`Array.from(order, fn)` is ~20× slower).
- **Pre-indexed acceleration (`FnIndex`)**: to precompute lowercase forms, bitmasks, or lookup tables once, define `pub struct <Fn>Index` in `src/port.rs` with:
  - `pub fn new(items: Vec<...>) -> Self`
  - `pub fn run(&self, ...) -> Result<Vec<R>, String>` (must match the scalar function item-for-item)
  - optional `pub fn rank(&self, ..., min: ...) -> Result<Vec<u32>, String>` (must match ranking the scalar results).

## 4. Prove, Fuzz, Shadow & Gate

```sh
fnr build <crate>
fnr diff <crate> [--float-tolerance 1e-9]
fnr fuzz <crate> --cases 100
fnr bench <crate>
fnr shadow <crate>
fnr gate <crate>
```

- **Stall rule (`trend.stalled`)**: if `diff` reports three consecutive edits to `src/port.rs` with zero additional passing cases, **stop editing**. Show the user the remaining mismatches and ask whether a hidden invariant applies or the function should stay in the original language.
- **`known-diffs.jsonl`**: only if the user explicitly approves fixing an original bug, add one JSON line copied from `fnr diff --json`:
  `{"fn": "...", "args": [...], "actual": ..., "reason": "..."}`. Remove it once the difference no longer exists.
- **`--float-tolerance`**: use only when the user accepts IEEE-754 FMA/operation-order deltas (e.g. `1e-9`). Never widen tolerance to hide a logic bug.
- **Shadow rollout**: load the generated shadow wrapper in place of the original. Control via `FNR_MODE` (or `RUST_ANYTHING_MODE`): `shadow` (default: runs both, returns original, logs mismatches), `new` (Rust only), `old` (original only), or per-unit `RUST_ANYTHING_UNIT_<NAME>=original` / `FNR_DISABLE=1`.
- **Gate requirements**: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, `unsafe` only with `// SAFETY:` comments, zero `unimplemented!`/`todo!`, every original throw/error path covered by a failing recorded case, zero `unwrap`/`expect`/`panic!` in fallible functions (mark provably unreachable cases with `// INFALLIBLE: <why>`), port LOC `<= 3×` original (extract helpers / use `crates.toml` recommendations if larger), and `100%` recorded `diff` pass.
