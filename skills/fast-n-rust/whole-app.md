# Whole app to a pure Rust app (`fast-n-rust` / `fnr`)

The goal: a standalone Rust app that does what the original does, with the original kept only as reference. The app stays runnable at every step: a Rust **front door** owns the entry point early and hands whatever is not ported yet to the original, units move to Rust one at a time through `analyze -> bridge -> record -> diff -> gate` (following [unit-porting.md](unit-porting.md)), and `app lift` at the end removes the fallback.

Everything written for a whole-app port lives in `<repo>/rust/`: `MAP.md`, `DECISIONS.md`, `GLOSSARY.md`, `HANDOFF.md`, `research/`, `app-cases/`, the unit crates, and the `app/` crate.

## 1. Plan & Decide (`analyze`)

1. Run `fnr analyze <repo> --whole-app --json` (and `fnr profile <repo> -- <cmd>` if speed matters).
2. Settle architectural decisions with the user before writing code and record them as numbered ADRs in `rust/DECISIONS.md`:
   - **Target shape & scope**: CLI (`clap`), HTTP server (`axum`), or library crate; which plugins/UIs stay out of scope.
   - **Equivalence bar**: exact parity vs `--float-tolerance` for numeric modules, error-message compatibility, exit codes.
   - **Dependencies**: review `crates.toml` fit (`exact`/`good`/`partial`/`none`); research `partial`/`none` dependencies in `rust/research/` before choosing crates.
   - **Targets & cutover**: target OS/CPU, rollback plan, and what triggers final cutover.
3. Write `rust/MAP.md` (unit waves, dependencies, fallback plan) and `rust/GLOSSARY.md` (domain terms).

## 2. Record the Whole App (`app record` / `app replay`)

- **CLI**: `fnr app record <repo> -- <command>` captures arguments, stdin, env, stdout, stderr, exit codes, and side effects into `rust/app-cases/`.
- **HTTP server**: add `--http` (or `--port <n>`) and `--effects http,smtp` (plus `--db-reset` / `--db-dump` when stateful) as needed.
- **Library**: use `fnr record <crate>` on each unit.
- Confirm the original passes 100%: `fnr app replay <repo>`.

## 3. Build the Front Door (`app front-door`)

Run `fnr app front-door <repo>` to scaffold `rust/app` (`clap` CLI, `axum` proxy server, or library facade) falling back to the original for everything unported. Verify `fnr app replay <repo>` passes 100% before porting units.

## 4. Port Every Unit & Wire It (`bridge`, `escalate`, `gate`, `app wire`)

For each wave in `rust/MAP.md`:
- Either promote an existing hotspot (`fnr port` -> `fnr escalate <id>`) or scaffold directly (`fnr bridge <repo> <unit-id>`).
- Record edge cases + existing test suite (`fnr record <crate> --from-tests -- <test-cmd>`).
- Implement `src/port.rs` per [unit-porting.md](unit-porting.md), then verify with `fnr build <crate>`, `fnr diff <crate>`, `fnr fuzz <crate>`, `fnr bench <crate>`, `fnr gate <crate>`.
- Wire into the front door: `fnr app wire <repo> <unit> --command <name>` (or `--route ...` for HTTP).
- Run `fnr map <repo> --write` to update `rust/PORT_MAP.md` and drive fallback rate toward 0%. Keep `rust/HANDOFF.md` updated between waves.
- Keep per-unit kill switches (`RUST_ANYTHING_UNIT_<NAME>=original`, `FNR_MODE=old`, `FNR_DISABLE=1`) active until lift.

## 5. Lift to a Pure Rust App (`app lift`)

Once fallback rate is 0% and every unit passes `gate`:
1. Run `fnr app lift <repo>`. It verifies on a copy first, strips fallback and kill switches, converts unit crates into internal libraries of `rust/app`, checks assets/targets, generates `rust/app/tests/app_cases.rs`, runs `fmt` + `clippy -D warnings` + `cargo test`, and replays all `app-cases` against the pure binary.
2. Run `fnr app replay <repo> --build <binary>` after any final reshape pass.
3. Keep the original in place as reference until the user approves cutover.
