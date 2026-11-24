//! Unified CLI entrypoint for both `fast-n-rust` and `fnr`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use serde_json::json;

use fnr_migrate::{
    analyze, app, bridge, card, effects, enable as menable, front_door, fuzz, gate, harness,
    lang::Lang, lift, package, port_map, profile, report, shadow, tolerance, whole_app,
};

use crate::{clean, doctor, mcp, setup, synergy};

#[derive(Clone, Copy, ValueEnum)]
pub enum KindArg {
    Napi,
    Wasm,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum GoMode {
    Fast,
    Unit,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ProfileArg {
    Host,
    Android,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum KeepArg {
    Mismatches,
    All,
}

#[derive(Parser)]
#[command(
    name = "fast-n-rust",
    version,
    about = "Rewrite the 5% that's slow — or lift the whole app to pure Rust. Proven against real behavior at every step.",
    arg_required_else_help = true,
    after_help = "\
Quick Start (IDE chat — Cursor, Claude Code, Antigravity, Windsurf, VS Code):
  1. fnr setup                  # register MCP + skills once
  2. fnr doctor                 # check Rust + your project runtime
  3. Ask your AI agent: \"Profile and speed up this project with fast-n-rust\"

Or from the terminal:
  fnr scan -- python3 app.py    # or: fnr scan -- npm run start
  fnr go --agent claude -- python3 app.py

Run `fnr commands` to see all 30 commands (batching, fuzzing, CI, packaging, whole-app lift)."
)]
pub struct Cli {
    /// Project folder (default: current directory).
    #[arg(short = 'C', long = "dir", global = true, default_value = ".")]
    pub dir: PathBuf,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Everything in one go: scan -> port -> agent -> verify -> (optional --strict gate) -> enable -> bench.
    Go {
        /// Coding agent: claude, codex, gemini, opencode, aider, or a custom command reading a prompt on stdin.
        #[arg(long)]
        agent: Option<String>,
        /// Mode: fast (5% hotspot loop, default) or unit (deep rust-anything unit go).
        #[arg(long, value_enum, default_value = "fast")]
        mode: GoMode,
        /// How many top portable hotspots to port (fast mode).
        #[arg(long, default_value_t = 1)]
        top: usize,
        /// Agent attempts per function (fast) or rounds (unit).
        #[arg(long, default_value_t = 3)]
        attempts: usize,
        /// Benchmark runs.
        #[arg(long, default_value_t = 5)]
        runs: usize,
        /// Real calls (fast) or generated cases (unit) to record.
        #[arg(long, default_value_t = 500)]
        samples: usize,
        /// Require strict `gate` (`fmt`, `clippy -D warnings`, `cargo test`, `unsafe` SAFETY) before enabling.
        #[arg(long, alias = "gate")]
        strict: bool,
        /// Relative/absolute tolerance when comparing floats in hotspot verify.
        #[arg(long)]
        float_tolerance: Option<f64>,
        /// Require at least this per-call speedup before enabling a port.
        #[arg(long)]
        min_speedup: Option<f64>,
        /// C/C++ compile_commands.json path (replaces compiler + sources in `-- <app args>`).
        #[arg(long)]
        cdb: Option<PathBuf>,
        /// Unit-mode only: port this function instead of profile's top pick.
        #[arg(long)]
        function: Option<String>,
        /// Unit-mode only: source language override.
        #[arg(long, value_enum)]
        lang: Option<Lang>,
        /// How you run your app (or app args when `--cdb` is set), after `--`.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Profile an app, rank portable hotspots, and run the Mode Advisor.
    Scan {
        /// C/C++ compile_commands.json path (for CMake/Meson/Ninja/Make projects).
        #[arg(long)]
        cdb: Option<PathBuf>,
        /// Stop profiling after N seconds (for servers, bots, and long-running apps on Python, Node, or Bun).
        #[arg(long)]
        duration: Option<u64>,
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// How you run your app (or app args when `--cdb` is set), after `--`.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Record real calls for a hotspot and scaffold `.fast/ports/<id>` with risk-enriched TASK.md.
    Port {
        /// Hotspot id (defaults to top candidate from last scan).
        id: Option<String>,
        /// Have this coding agent write the Rust port right away.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 3)]
        attempts: usize,
        #[arg(long, default_value_t = 500)]
        samples: usize,
    },
    /// Release-build and replay recorded + edge cases against a hotspot port.
    Verify {
        /// Hotspot id (defaults to the only port if there is one).
        id: Option<String>,
        /// Verify every hotspot port in `.fast`.
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// Also run strict `gate` (`fmt`, `clippy -D warnings`, `cargo test`, `unsafe` SAFETY).
        #[arg(long)]
        gate: bool,
        /// Explicit relative/absolute float comparison tolerance.
        #[arg(long)]
        float_tolerance: Option<f64>,
        /// Path to `known-diffs.jsonl` waiving expected differences.
        #[arg(long)]
        known_diffs: Option<PathBuf>,
        /// Fail if per-call Rust speedup is below this threshold.
        #[arg(long)]
        min_speedup: Option<f64>,
    },
    /// Switch on a verified hotspot port OR a unit crate (auto-detected).
    Enable {
        /// Hotspot id or path to `rust/<crate>`.
        target: Option<String>,
        /// Enable even if verification / diff has not passed.
        #[arg(long)]
        force: bool,
        /// Build hotspot port with Profile-Guided Optimization (`llvm-profdata`).
        #[arg(long)]
        pgo: bool,
        /// Compile with `-C target-cpu=native` (host-specific AVX2/FMA/NEON optimization).
        #[arg(long)]
        native_cpu: bool,
        #[arg(long)]
        json: bool,
    },
    /// Switch back to the original code for a hotspot port OR unit crate (auto-detected).
    Disable {
        /// Hotspot id or path to `rust/<crate>`.
        target: Option<String>,
        /// Disable every enabled hotspot port.
        #[arg(long, conflicts_with = "target")]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Benchmark the whole app (hotspot mode) or a unit crate (when passed a crate path).
    Bench {
        /// Optional path to a `rust/<crate>` directory.
        target: Option<PathBuf>,
        /// App benchmark runs (hotspot mode).
        #[arg(long, default_value_t = 5)]
        runs: usize,
        /// Rebuild enabled hotspot ports with Profile-Guided Optimization before benchmarking.
        #[arg(long)]
        pgo: bool,
        /// Rebuild enabled hotspot ports with `-C target-cpu=native` before benchmarking.
        #[arg(long)]
        native_cpu: bool,
        /// Minimum measured ms per function (unit crate mode).
        #[arg(long, default_value_t = 200)]
        min_ms: u64,
        #[arg(long)]
        json: bool,
    },
    /// Generate Markdown + SVG report card for the app or a unit crate.
    #[command(hide = true)]
    Report {
        /// Optional path to a `rust/<crate>` directory.
        target: Option<PathBuf>,
        #[arg(long, short)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 200)]
        min_ms: u64,
        #[arg(long)]
        json: bool,
    },
    /// Show scan hotspots, `.fast` ports, and `rust/` unit crate map.
    Status {
        #[arg(long)]
        json: bool,
    },

    // --- Deep Unit Migration & Synergies ---
    /// Map a repository with AST analysis and produce a ranked migration plan.
    #[command(hide = true)]
    Analyze {
        #[arg(default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long, short)]
        out: Option<PathBuf>,
        #[arg(long, value_enum)]
        lang: Option<Lang>,
        #[arg(long)]
        whole_app: bool,
    },
    /// Profile a workload using runtime profilers and list callers + `--batch` opportunities.
    #[command(hide = true)]
    Profile {
        #[arg(default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum)]
        lang: Option<Lang>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Promote a `.fast/ports/<id>` hotspot port into a full `rust/<name>` unit crate (supports `--batch`, `fuzz`, `shadow`, `gate`).
    #[command(hide = true)]
    Escalate {
        /// Hotspot id (defaults to the only port if there is one).
        id: Option<String>,
        /// Optional Rayon batch spec (`function:param`).
        #[arg(long)]
        batch: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Scaffold a `rust/<name>` unit crate with multi-language bindings and optional Rayon `--batch`.
    #[command(hide = true)]
    Bridge {
        root: PathBuf,
        unit: String,
        #[arg(long, value_enum)]
        kind: Option<KindArg>,
        #[arg(long, short)]
        out: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        keep: Vec<String>,
        #[arg(long)]
        batch: Vec<String>,
        #[arg(long = "as-json", value_name = "TYPE_PARAM")]
        as_json: Vec<String>,
        #[arg(long, value_enum, default_value = "mimalloc")]
        allocator: bridge::Allocator,
    },
    /// Regenerate bindings from signatures in `src/port.rs`.
    #[command(hide = true)]
    Refresh {
        #[arg(default_value = ".")]
        crate_dir: PathBuf,
        #[arg(long)]
        batch: Vec<String>,
    },
    /// Record edge cases, semantic traps, and optional test suite calls into `cases.jsonl`.
    #[command(hide = true)]
    Record {
        #[arg(default_value = ".")]
        crate_dir: PathBuf,
        #[arg(long, default_value_t = 200)]
        count: usize,
        #[arg(long)]
        inputs: Option<PathBuf>,
        #[arg(long)]
        module: Option<PathBuf>,
        #[arg(long)]
        resolve_from: Option<PathBuf>,
        #[arg(long = "node-arg", allow_hyphen_values = true)]
        node_args: Vec<String>,
        #[arg(long)]
        float_tolerance: Option<f64>,
        #[arg(long, num_args = 1..)]
        from_tests: Vec<String>,
    },
    /// Build a unit crate (or all hotspot ports when omitted).
    #[command(hide = true)]
    Build {
        crate_dir: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "host")]
        profile: ProfileArg,
        /// Build all `.fast/ports/*` crates with Ship LTO into `.fast/lib/` without needing state/recordings (for CI/Docker).
        #[arg(long)]
        deploy: bool,
        /// Enable host-specific `-C target-cpu=native` when using `--deploy`.
        #[arg(long)]
        native_cpu: bool,
        #[arg(long)]
        abi: Vec<String>,
        #[arg(long)]
        api: Option<u32>,
        #[arg(long)]
        ndk: Option<PathBuf>,
        #[arg(long)]
        jni_libs: Option<PathBuf>,
    },
    /// Replay recorded cases against a unit crate's Rust module.
    #[command(hide = true)]
    Diff {
        #[arg(default_value = ".")]
        crate_dir: PathBuf,
        #[arg(long)]
        module: Option<PathBuf>,
        #[arg(long)]
        ignore_key_order: bool,
        #[arg(long = "node-arg", allow_hyphen_values = true)]
        node_args: Vec<String>,
        #[arg(long)]
        float_tolerance: Option<f64>,
        #[arg(long)]
        json: bool,
    },
    /// Property-fuzz a hotspot port (`.fast/ports/<id>`) or unit crate (`rust/<crate>`).
    #[command(hide = true)]
    Fuzz {
        /// Hotspot id or path to `rust/<crate>` (defaults to `.` / single hotspot).
        target: Option<String>,
        #[arg(long, default_value_t = 200)]
        cases: usize,
        #[arg(long)]
        seed: Option<u64>,
        #[arg(long)]
        time: Option<f64>,
        #[arg(long, value_enum)]
        keep: Option<KeepArg>,
        #[arg(long)]
        resolve_from: Option<PathBuf>,
        #[arg(long = "node-arg", allow_hyphen_values = true)]
        node_args: Vec<String>,
        #[arg(long)]
        float_tolerance: Option<f64>,
        /// Save shrunk differential mismatches into `recordings.jsonl` as permanent regression tests.
        #[arg(long)]
        save_regressions: bool,
        #[arg(long)]
        json: bool,
    },
    /// Generate a drop-in shadow module running original and Rust side by side.
    #[command(hide = true)]
    Shadow {
        #[arg(default_value = ".")]
        crate_dir: PathBuf,
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Strict Definition-of-Done check (`fmt`, `clippy -D warnings`, `cargo test`, `unsafe` SAFETY, parity) on a hotspot port OR unit crate.
    #[command(hide = true)]
    Gate {
        /// Hotspot id or path to `rust/<crate>` (defaults to `.` / single hotspot).
        target: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long = "node-arg", allow_hyphen_values = true)]
        node_args: Vec<String>,
    },
    /// Run CI quality & speedup gate (`verify`, strict `gate`, optional `--fuzz`, `--bench`, `--min-speedup`).
    #[command(hide = true)]
    Ci {
        /// Optional hotspot port id (default: all hotspot ports).
        target: Option<String>,
        #[arg(long)]
        float_tolerance: Option<f64>,
        #[arg(long, default_value_t = 0)]
        fuzz: usize,
        #[arg(long)]
        bench: bool,
        #[arg(long, default_value_t = 5)]
        runs: usize,
        #[arg(long)]
        min_speedup: Option<f64>,
        #[arg(long)]
        native_cpu: bool,
        #[arg(long)]
        json: bool,
    },
    /// Generate a port map across all units and `rust/` crates.
    #[command(hide = true)]
    Map {
        #[arg(default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        crates: Vec<PathBuf>,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        check: bool,
        #[arg(long)]
        json: bool,
    },
    /// Package a verified hotspot port or unit crate for distribution (`dist/<ext>` / npm / wheel).
    #[command(hide = true)]
    Package {
        /// Hotspot id or path to `rust/<crate>`.
        target: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, short)]
        out: Option<PathBuf>,
        #[arg(long)]
        fallback: Option<String>,
        #[arg(long)]
        bin: Vec<String>,
        #[arg(long)]
        original: Option<String>,
        /// Compile with `-C target-cpu=native` (non-portable across different CPU generations).
        #[arg(long)]
        native_cpu: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },

    // --- Whole-App Pure Rust Migration ---
    /// Whole-app recording, replay, front-door scaffolding, wiring, and lift to pure Rust.
    #[command(hide = true)]
    App {
        #[command(subcommand)]
        command: AppCmd,
    },

    // --- Ops & Agent Integration ---
    /// Run as an MCP server over stdio (JSON-RPC 2.0; compact 7-tool mode by default, or `--all-tools` for 30 tools).
    #[command(hide = true)]
    Mcp {
        /// Expose all 30 fine-grained legacy tools instead of the default 7 compact orchestrator tools.
        #[arg(long)]
        all_tools: bool,
    },
    /// Register `fast-n-rust` MCP server and skills with AI coding agents (Claude, Codex, Cursor, Windsurf, VS Code, Gemini, Antigravity, OpenCode).
    Setup {
        #[arg(long = "agent", value_enum)]
        agents: Vec<setup::Agent>,
        #[arg(long)]
        remove: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        no_backup: bool,
        #[arg(long)]
        json: bool,
    },
    /// Check Rust toolchain, project runtimes (pass `--all` for all 14 languages), AI agents, and build cache sizes.
    Doctor {
        /// Check all 14 source languages, WASM, Android NDK, and all CLI agents instead of scoping to the current project.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Reclaim disk space from shared Cargo cache, `.fast/target`, `rust/*/target`, or `.fast/`.
    Clean {
        #[arg(long)]
        cache: bool,
        #[arg(long)]
        ports: bool,
        #[arg(long)]
        rust: bool,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// List all commands including deep unit-migration, fuzzing, CI, packaging, and whole-app lift.
    Commands,
    /// Generate shell completion scripts for `fast-n-rust` and `fnr`.
    #[command(hide = true)]
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Internal child-process replay runner for C-ABI hotspot verification.
    #[command(name = "__replay", hide = true)]
    Replay { job: PathBuf },
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum AppCmd {
    /// Record whole-app CLI or HTTP behavior into `<repo>/rust/app-cases/`.
    Record {
        #[arg(default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        inputs: Option<PathBuf>,
        #[arg(long)]
        normalize: Vec<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        no_traps: bool,
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        http: bool,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
        server_args: Vec<String>,
        #[arg(long)]
        fresh: Vec<String>,
        #[arg(long)]
        pace: Option<u64>,
        #[arg(long)]
        float_tolerance: Option<f64>,
        #[arg(long)]
        float_file: Vec<String>,
        #[arg(long)]
        effects: Vec<String>,
        #[arg(long)]
        effect_env: Vec<String>,
        #[arg(long)]
        db_reset: Option<String>,
        #[arg(long)]
        db_dump: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(last = true, required = true)]
        run: Vec<String>,
    },
    /// Replay recorded whole-app cases against the original or a Rust build.
    Replay {
        #[arg(default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        build: Option<String>,
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        case: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Scaffold `rust/app`, the Rust entry point falling back to the original.
    FrontDoor {
        #[arg(default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        no_check: bool,
        #[arg(long)]
        library: bool,
        #[arg(long)]
        json: bool,
    },
    /// Wire a gated unit crate into `rust/app`.
    Wire {
        repo: PathBuf,
        unit: String,
        #[arg(long = "command")]
        commands: Vec<String>,
        #[arg(long = "route")]
        routes: Vec<String>,
        #[arg(long)]
        skip_gate: bool,
        #[arg(long)]
        no_check: bool,
        #[arg(long)]
        json: bool,
    },
    /// Lift `rust/app` into a standalone pure Rust app with zero fallback.
    Lift {
        #[arg(default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

fn abs(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

fn resolve_in(root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        abs(&root.join(p))
    }
}

fn print_banner() {
    eprintln!("⚡ fast-n-rust v{} (fnr)", env!("CARGO_PKG_VERSION"));
}

fn is_silent_cmd(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::Mcp { .. }
            | Cmd::Replay { .. }
            | Cmd::Completions { .. }
            | Cmd::Scan { json: true, .. }
            | Cmd::Enable { json: true, .. }
            | Cmd::Disable { json: true, .. }
            | Cmd::Bench { json: true, .. }
            | Cmd::Report { json: true, .. }
            | Cmd::Status { json: true }
            | Cmd::Analyze { json: true, .. }
            | Cmd::Profile { json: true, .. }
            | Cmd::Escalate { json: true, .. }
            | Cmd::Diff { json: true, .. }
            | Cmd::Fuzz { json: true, .. }
            | Cmd::Gate { json: true, .. }
            | Cmd::Ci { json: true, .. }
            | Cmd::Map { json: true, .. }
            | Cmd::Package { json: true, .. }
            | Cmd::Setup { json: true, .. }
            | Cmd::Doctor { json: true, .. }
            | Cmd::Clean { json: true, .. }
            | Cmd::App {
                command: AppCmd::Record { json: true, .. }
                    | AppCmd::Replay { json: true, .. }
                    | AppCmd::FrontDoor { json: true, .. }
                    | AppCmd::Wire { json: true, .. }
                    | AppCmd::Lift { json: true, .. }
            }
    )
}

pub fn run_cli() -> ExitCode {
    let cli = Cli::parse();
    let root = abs(&cli.dir);
    let silent = is_silent_cmd(&cli.cmd);
    if silent {
        fnr_fast::ui::set_quiet(true);
    } else {
        print_banner();
    }
    match dispatch(cli.cmd, &root) {
        Ok(code) => code,
        Err(e) => {
            fnr_fast::ui::error(format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}

fn print_gate_checks(checks: &[gate::Check], passed: bool) {
    for c in checks {
        let mark = if c.passed { "pass" } else { "FAIL" };
        let first = c.detail.lines().next().unwrap_or_default();
        println!("{mark}  {:<10} {first}", c.name);
        if !c.passed {
            for line in c.detail.lines().skip(1) {
                println!("            {line}");
            }
            if let Some(fix) = &c.fix {
                println!("            fix: {fix}");
            }
        }
    }
    println!(
        "{}",
        if passed {
            "Gate passed."
        } else {
            "Gate failed."
        }
    );
}

fn dispatch(cmd: Cmd, root: &Path) -> Result<ExitCode> {
    match cmd {
        Cmd::Mcp { all_tools } => {
            mcp::serve_with_mode(if all_tools {
                mcp::ToolMode::Full
            } else {
                mcp::ToolMode::Compact
            })?;
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Replay { job } => {
            fnr_fast::native::replay(&job)?;
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Scan {
            cdb,
            duration,
            json,
            command,
        } => {
            let command = if let Some(cdb_path) = cdb {
                fnr_fast::cfamily::command_from_cdb(&resolve_in(root, &cdb_path), &command)?
            } else {
                anyhow::ensure!(
                    !command.is_empty(),
                    "put the command that runs your app after `--`, or pass `--cdb build/compile_commands.json`"
                );
                command
            };
            let out = fnr_fast::pipeline::scan_with_options(
                root,
                command,
                fnr_fast::pipeline::ScanOptions {
                    duration_secs: duration,
                },
            )?;
            let advice = synergy::advise_mode(&out);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "scan": out, "advice": advice }))?
                );
            } else {
                fnr_fast::show::scan(&out);
                println!("\nMode Advisor: {}", advice.headline);
                for hint in &advice.diagnosis.hints {
                    println!(
                        "  [{}] {}:{} ({:.1}%) — {}",
                        hint.category,
                        hint.file,
                        hint.line,
                        hint.share * 100.0,
                        hint.suggestion
                    );
                }
                if !advice.next_command.is_empty() {
                    println!("  → {}", advice.next_command);
                }
            }
        }
        Cmd::Port {
            id,
            agent,
            attempts,
            samples,
        } => {
            let out = synergy::port_and_enrich(root, id.as_deref(), samples)?;
            fnr_fast::show::port(&out, agent.is_none());
            if let Some(agent) = agent {
                let v = fnr_fast::pipeline::agent_port(root, &out.id, &agent, attempts)?;
                if !v.ok() {
                    bail!(
                        "{agent} couldn't pass verify for {} in {attempts} attempts",
                        out.id
                    );
                }
            }
        }
        Cmd::Verify {
            id,
            all,
            gate: run_gate,
            float_tolerance,
            known_diffs,
            min_speedup,
        } => {
            let vopts = fnr_fast::pipeline::VerifyOptions {
                float_tolerance,
                known_diffs: known_diffs.map(|p| resolve_in(root, &p)),
                min_speedup,
            };
            if all {
                fnr_fast::pipeline::verify_all_with(root, &vopts)?;
            } else {
                let (vid, v) = fnr_fast::pipeline::verify_with(root, id.as_deref(), &vopts)?;
                fnr_fast::show::verify(&vid, &v);
                if !v.ok() || v.below_min_speedup(min_speedup) {
                    if v.below_min_speedup(min_speedup) {
                        fnr_fast::ui::error(format!(
                            "{vid} speedup {:.2}x is below --min-speedup {:.2}x",
                            v.micro.map_or(0.0, |m| m.speedup),
                            min_speedup.unwrap_or(0.0)
                        ));
                    }
                    return Ok(ExitCode::FAILURE);
                }
                if run_gate {
                    let g = synergy::gate_hotspot_port(root, Some(&vid))?;
                    print_gate_checks(&g.checks, g.passed);
                    if !g.passed {
                        return Ok(ExitCode::FAILURE);
                    }
                }
            }
        }
        Cmd::Go {
            agent,
            mode,
            top,
            attempts,
            runs,
            samples,
            strict,
            float_tolerance,
            min_speedup,
            cdb,
            function,
            lang,
            command,
        } => {
            let command = if let Some(cdb_path) = cdb {
                fnr_fast::cfamily::command_from_cdb(&resolve_in(root, &cdb_path), &command)?
            } else {
                anyhow::ensure!(
                    !command.is_empty(),
                    "put the command that runs your app after `--`, or pass `--cdb build/compile_commands.json`"
                );
                command
            };
            match mode {
                GoMode::Fast => {
                    let outcome = synergy::go_hotspot(
                        root,
                        synergy::GoHotspotOptions {
                            command,
                            agent: agent.as_deref(),
                            top,
                            attempts,
                            runs,
                            samples,
                            strict,
                            float_tolerance,
                            min_speedup,
                            print_progress: true,
                        },
                    )?;
                    if agent.is_none() {
                        println!("\nPrepared `.fast/ports/` with recorded inputs and TASK.md:");
                        for p in &outcome.ports {
                            println!("  • {} ({})", p.id, p.crate_dir.display());
                        }
                        println!(
                            "\nNext (in your IDE chat — Cursor, Claude Code, Antigravity, Windsurf, VS Code):"
                        );
                        println!(
                            "  Ask your agent: \"Implement the fast-n-rust port(s) in .fast/ports/ and verify them\""
                        );
                        println!("\nOr finish manually from the terminal:");
                        for p in &outcome.ports {
                            println!("  → fnr verify {} --gate && fnr enable {}", p.id, p.id);
                        }
                    }
                }
                GoMode::Unit => {
                    let outcome = fnr_migrate::go::go(&fnr_migrate::go::Options {
                        root,
                        command: &command,
                        lang,
                        agent: agent.as_deref(),
                        function: function.as_deref(),
                        count: samples,
                        rounds: attempts,
                    })?;
                    println!("{}", serde_json::to_string_pretty(&outcome)?);
                }
            }
        }
        Cmd::Enable {
            target,
            force,
            pgo,
            native_cpu,
            json,
        } => match synergy::detect_target(root, target.as_deref()) {
            synergy::TargetKind::MigrateCrate(crate_dir) => {
                anyhow::ensure!(
                    !pgo && !native_cpu,
                    "--pgo / --native-cpu are supported on `.fast` hotspot ports; for unit crates use `fnr build`"
                );
                if !force {
                    harness::build(&crate_dir)?;
                    let d = harness::diff(&harness::DiffOptions {
                        crate_dir: &crate_dir,
                        module: None,
                        ignore_key_order: false,
                        node_args: &[],
                        float_tolerance: None,
                    })?;
                    anyhow::ensure!(
                        d.failed == 0 && d.missing.is_empty(),
                        "{}/{} recorded cases match; pass --force to override",
                        d.passed,
                        d.total
                    );
                }
                let on = menable::enable(&crate_dir)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&on)?);
                } else {
                    println!("Enabled {} ({})", on.crate_name, on.functions.join(", "));
                }
            }
            synergy::TargetKind::FastPort(id) => {
                let eid = fnr_fast::pipeline::enable_with_options(
                    root,
                    id.as_deref(),
                    force,
                    pgo,
                    native_cpu,
                )?;
                let deploy = synergy::audit_deploy(root);
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &json!({ "enabled": eid, "pgo": pgo, "native_cpu": native_cpu, "deploy": deploy })
                        )?
                    );
                } else {
                    println!("Enabled hotspot `{eid}` (set FNR_DISABLE=1 to run original)");
                    for w in &deploy.warnings {
                        println!("  ! {w}");
                    }
                    for rec in deploy.recommendations.iter().take(2) {
                        println!("  deploy: {rec}");
                    }
                }
            }
        },
        Cmd::Disable { target, all, json } => {
            if all {
                let list = fnr_fast::pipeline::disable_all(root)?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "disabled": list }))?
                    );
                } else {
                    println!("Disabled {} hotspot port(s).", list.len());
                }
            } else {
                match synergy::detect_target(root, target.as_deref()) {
                    synergy::TargetKind::MigrateCrate(crate_dir) => {
                        let off = menable::disable(&crate_dir)?;
                        if json {
                            println!("{}", serde_json::to_string_pretty(&off)?);
                        } else {
                            println!("Disabled {}", off.crate_name);
                        }
                    }
                    synergy::TargetKind::FastPort(id) => {
                        let did = fnr_fast::pipeline::disable(root, id.as_deref())?;
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&json!({ "disabled": did }))?
                            );
                        } else {
                            println!("Disabled hotspot `{did}`.");
                        }
                    }
                }
            }
        }
        Cmd::Bench {
            target,
            runs,
            pgo,
            native_cpu,
            min_ms,
            json,
        } => {
            let t_str = target.as_ref().map(|p| p.to_string_lossy().into_owned());
            if let synergy::TargetKind::MigrateCrate(crate_dir) =
                synergy::detect_target(root, t_str.as_deref())
            {
                let rows = harness::bench(&harness::BenchOptions {
                    crate_dir: &crate_dir,
                    min_ms,
                    resolve_from: None,
                    node_args: &[],
                })?;
                let lang = bridge::Manifest::load(&crate_dir)
                    .map(|m| m.language)
                    .unwrap_or_default();
                let v = harness::verdict(&rows, lang);
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "rows": rows, "verdict": v }))?
                    );
                } else {
                    println!("Overall {:.2}x: {}", v.speedup, v.advice);
                }
            } else {
                if pgo || native_cpu {
                    let mut ctx = fnr_fast::state::Ctx::open(root)?;
                    let enabled_ids: Vec<String> = ctx
                        .state
                        .ports
                        .values()
                        .filter(|p| p.enabled)
                        .map(|p| p.id.clone())
                        .collect();
                    anyhow::ensure!(
                        !enabled_ids.is_empty(),
                        "no hotspot ports are enabled yet: run `fnr enable` first"
                    );
                    let prof_label = match (pgo, native_cpu) {
                        (true, true) => "pgo+native-cpu",
                        (true, false) => "pgo",
                        (false, true) => "ship+native-cpu",
                        (false, false) => "ship",
                    };
                    for pid in enabled_ids {
                        let port = ctx.state.ports[&pid].clone();
                        if pgo {
                            fnr_fast::backend::build_pgo_with_options(&ctx, &port, native_cpu)?;
                        } else {
                            match fnr_fast::backend::build_with_options(
                                &ctx,
                                &port,
                                fnr_fast::backend::BuildOptions {
                                    profile: fnr_fast::backend::BuildProfile::Ship,
                                    native_cpu,
                                },
                            )? {
                                Ok(_) => {}
                                Err(log) => anyhow::bail!("ship build failed for {pid}:\n{log}"),
                            }
                        }
                        if let Some(p) = ctx.state.ports.get_mut(&pid) {
                            p.build_profile = Some(prof_label.to_string());
                        }
                    }
                    ctx.save()?;
                }
                let b = fnr_fast::pipeline::bench(root, runs)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&b)?);
                } else {
                    fnr_fast::show::bench(&b);
                }
            }
        }
        Cmd::Report {
            target,
            out,
            min_ms,
            json,
        } => {
            let t_str = target.as_ref().map(|p| p.to_string_lossy().into_owned());
            if let synergy::TargetKind::MigrateCrate(crate_dir) =
                synergy::detect_target(root, t_str.as_deref())
            {
                let c = card::report(&card::Options {
                    crate_dir: &crate_dir,
                    out: out.as_deref(),
                    min_ms,
                    node_args: &[],
                })?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&c)?);
                } else {
                    print!("{}", card::markdown(&c));
                }
            } else {
                let r = fnr_fast::pipeline::report(root)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&r)?);
                } else {
                    fnr_fast::show::report(&r);
                }
            }
        }
        Cmd::Status { json } => {
            let st = synergy::unified_status(root)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                if st.has_fast {
                    let ctx = fnr_fast::state::Ctx::open(root)?;
                    fnr_fast::show::status(&ctx);
                    if !st.hotspots.is_empty() {
                        println!("\nHotspot Ports (Transport & Build Matrix):");
                        for row in &st.hotspots {
                            let sp = row
                                .per_call_speedup
                                .map_or("n/a".into(), |s| format!("{s:.1}x"));
                            println!(
                                "  {:<28} {:<7} {:<8} [{}]  profile={}  verify={}/{}  per-call={}",
                                row.id,
                                row.lang.label(),
                                if row.enabled { "ON" } else { "off" },
                                row.transport,
                                row.build_profile.as_deref().unwrap_or("release"),
                                row.passed,
                                row.total,
                                sp
                            );
                        }
                    }
                } else {
                    println!("No `.fast/state.json` hotspot scan yet.");
                }
                if let Some(m) = st.units
                    && (!m.ports.is_empty() || !m.not_ported.is_empty())
                {
                    println!("\n{}", port_map::markdown(&m));
                }
                if let Some(deploy) = &st.deploy {
                    println!("\nDeploy readiness ({}):", deploy.host_target);
                    for w in &deploy.warnings {
                        println!("  ! {w}");
                    }
                    for rec in &deploy.recommendations {
                        println!("  • {rec}");
                    }
                }
            }
        }
        Cmd::Analyze {
            root: r,
            json,
            out,
            lang,
            whole_app: wa,
        } => {
            let r = resolve_in(root, &r);
            let lang = lang.unwrap_or_else(|| Lang::detect(&r));
            let mut rep = analyze::analyze_lang(&r, lang)
                .with_context(|| format!("analyzing {}", r.display()))?;
            if wa {
                rep.whole_app = Some(whole_app::whole_app(&r, &rep));
            }
            let text = if json {
                serde_json::to_string_pretty(&rep)? + "\n"
            } else {
                let title = r
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| r.display().to_string());
                report::markdown(&rep, &title)
            };
            match out {
                Some(path) => std::fs::write(&path, text)?,
                None => print!("{text}"),
            }
        }
        Cmd::Profile {
            root: r,
            json,
            lang,
            command,
        } => {
            let rep = profile::profile(&resolve_in(root, &r), &command, lang)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                println!("{}", profile::markdown(&rep));
            }
        }
        Cmd::Escalate { id, batch, json } => {
            let out = synergy::escalate_hotspot(root, id.as_deref(), batch.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!(
                    "Escalated `{}` -> {}",
                    out.port_id,
                    out.migrate_crate.display()
                );
                if out.disabled_active_patch {
                    println!("  disabled active inline hotspot patch before AST analysis");
                }
                if out.auto_ported {
                    println!("  auto-spliced hotspot Rust implementation into src/port.rs");
                }
                if out.imported_cases > 0 {
                    println!("  imported {} recorded calls", out.imported_cases);
                }
                for step in &out.next_steps {
                    println!("  → {step}");
                }
            }
        }
        Cmd::Bridge {
            root: r,
            unit,
            kind,
            out,
            name,
            force,
            keep,
            batch,
            as_json,
            allocator,
        } => {
            let outcome = bridge::bridge(&bridge::Options {
                root: &resolve_in(root, &r),
                unit: &unit,
                kind: kind.map(|k| match k {
                    KindArg::Napi => bridge::Kind::Napi,
                    KindArg::Wasm => bridge::Kind::Wasm,
                }),
                out,
                name,
                force,
                keep,
                lang: None,
                batch,
                as_json,
                allocator,
            })?;
            println!(
                "Created {} ({:?}) for {}",
                outcome.crate_dir.display(),
                outcome.kind,
                outcome.exports.join(", ")
            );
        }
        Cmd::Refresh { crate_dir, batch } => {
            let c = resolve_in(root, &crate_dir);
            let r = if batch.is_empty() {
                bridge::refresh(&c)?
            } else {
                bridge::add_batches(&c, &batch)?
            };
            println!(
                "Regenerated {} for {}",
                r.files.join(", "),
                r.functions.join(", ")
            );
        }
        Cmd::Record {
            crate_dir,
            count,
            inputs,
            module,
            resolve_from,
            node_args,
            float_tolerance,
            from_tests,
        } => {
            let c = resolve_in(root, &crate_dir);
            let mut inputs = inputs;
            if !from_tests.is_empty() {
                let cap = fnr_migrate::from_tests::capture(&c, &from_tests)?;
                inputs = Some(fnr_migrate::from_tests::with_own_inputs(
                    &c,
                    inputs.as_deref(),
                    &cap.inputs,
                )?);
            }
            if let Some(t) = float_tolerance {
                let mut m = bridge::Manifest::load(&c)?;
                m.float_tolerance = (tolerance::check(t)? > 0.0).then_some(t);
                m.save(&c)?;
            }
            let out = harness::record(&harness::RecordOptions {
                crate_dir: &c,
                count,
                inputs: inputs.as_deref(),
                module: module.as_deref(),
                resolve_from: resolve_from.as_deref(),
                node_args: &node_args,
            })?;
            println!("Recorded {} cases in {}", out.cases, c.display());
        }
        Cmd::Build {
            crate_dir,
            profile,
            deploy,
            native_cpu,
            abi,
            api,
            ndk,
            jni_libs,
        } => match crate_dir {
            None if deploy => {
                let built = fnr_fast::pipeline::build_deploy(root, native_cpu)?;
                for p in built {
                    println!("Built {}", p.display());
                }
            }
            None => fnr_fast::pipeline::build_all(root)?,
            Some(c) => {
                let c = resolve_in(root, &c);
                match profile {
                    ProfileArg::Host => println!("Built {}", harness::build(&c)?.display()),
                    ProfileArg::Android => {
                        let rep = harness::build_android(
                            &c,
                            &fnr_migrate::android::Options {
                                abis: abi,
                                api: api.unwrap_or(fnr_migrate::android::DEFAULT_API),
                                ndk,
                                out: jni_libs,
                            },
                        )?;
                        for l in &rep.libraries {
                            println!("Built {}", l.path.display());
                        }
                    }
                }
            }
        },
        Cmd::Diff {
            crate_dir,
            module,
            ignore_key_order,
            node_args,
            float_tolerance,
            json,
        } => {
            let c = resolve_in(root, &crate_dir);
            let out = harness::diff(&harness::DiffOptions {
                crate_dir: &c,
                module: module.as_deref(),
                ignore_key_order,
                node_args: &node_args,
                float_tolerance,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!(
                    "diff: {}/{} passed ({} failed)",
                    out.passed, out.total, out.failed
                );
            }
            if out.failed > 0 || !out.missing.is_empty() {
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Fuzz {
            target,
            cases,
            seed,
            time,
            keep,
            resolve_from,
            node_args,
            float_tolerance,
            save_regressions,
            json,
        } => match synergy::detect_target(root, target.as_deref()) {
            synergy::TargetKind::MigrateCrate(c) => {
                let time = time.and_then(fnr_migrate::process::seconds);
                let out = fuzz::fuzz(&fuzz::FuzzOptions {
                    crate_dir: &c,
                    cases,
                    seed,
                    time,
                    keep: keep.map(|k| match k {
                        KeepArg::Mismatches => fuzz::Keep::Mismatches,
                        KeepArg::All => fuzz::Keep::All,
                    }),
                    resolve_from: resolve_from.as_deref(),
                    node_args: &node_args,
                })?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&out)?);
                } else {
                    println!(
                        "fuzz: {}/{} matched across {} rounds",
                        out.passed, out.cases, out.rounds
                    );
                }
                if out.failed > 0 {
                    return Ok(ExitCode::FAILURE);
                }
            }
            synergy::TargetKind::FastPort(id) => {
                let out = synergy::fuzz_hotspot_port(
                    root,
                    id.as_deref(),
                    cases,
                    seed,
                    float_tolerance,
                    save_regressions,
                )?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&out)?);
                } else {
                    println!(
                        "fuzz {} [{}]: {}/{} passed (mismatches: {}, panics: {}, nondeterministic: {}, handled errors: {})",
                        out.id,
                        out.mode,
                        out.passed,
                        out.cases,
                        out.mismatches,
                        out.panics,
                        out.nondeterministic,
                        out.errors
                    );
                    if let Some(note) = &out.oracle_note {
                        println!("  note: {note}");
                    }
                    for sh in &out.report.shrunk {
                        println!(
                            "  shrunk case #{}: {} -> {} ({} probes) | expected={} got={}",
                            sh.case,
                            sh.original_args,
                            sh.shrunk_args,
                            sh.probes,
                            sh.expected,
                            sh.got
                        );
                    }
                    if out.saved_regressions > 0 {
                        println!(
                            "  saved {} regression case(s) to recordings.jsonl",
                            out.saved_regressions
                        );
                    }
                    if let Some(p) = &out.failures_file {
                        println!("  failures saved: {}", p.display());
                    }
                }
                if out.mismatches > 0 || out.panics > 0 || out.nondeterministic > 0 {
                    return Ok(ExitCode::FAILURE);
                }
            }
        },
        Cmd::Shadow { crate_dir, out } => {
            let s = shadow::shadow(&resolve_in(root, &crate_dir), out.as_deref())?;
            println!("Wrote {} ({})", s.path.display(), s.exports.join(", "));
        }
        Cmd::Gate {
            target,
            json,
            node_args,
        } => match synergy::detect_target(root, target.as_deref()) {
            synergy::TargetKind::MigrateCrate(crate_dir) => {
                let out = gate::gate(&crate_dir, &node_args)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&out)?);
                } else {
                    print_gate_checks(&out.checks, out.passed);
                }
                if !out.passed {
                    return Ok(ExitCode::FAILURE);
                }
            }
            synergy::TargetKind::FastPort(id) => {
                let out = synergy::gate_hotspot_port(root, id.as_deref())?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&out)?);
                } else {
                    print_gate_checks(&out.checks, out.passed);
                }
                if !out.passed {
                    return Ok(ExitCode::FAILURE);
                }
            }
        },
        Cmd::Ci {
            target,
            float_tolerance,
            fuzz,
            bench,
            runs,
            min_speedup,
            native_cpu,
            json,
        } => {
            let out = synergy::run_ci(
                root,
                synergy::CiOptions {
                    target,
                    float_tolerance,
                    fuzz,
                    bench,
                    runs,
                    min_speedup,
                    native_cpu,
                },
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for p in &out.ports {
                    println!(
                        "CI {}: verify={}, gate={}, fuzz={}, speedup={}",
                        p.id,
                        p.verified,
                        p.gate_passed,
                        p.fuzz_passed.map_or("skipped".into(), |b| b.to_string()),
                        p.per_call_speedup
                            .map_or("n/a".into(), |s| format!("{s:.2}x"))
                    );
                }
                if let Some(b) = &out.bench {
                    fnr_fast::show::bench(b);
                }
                if let Some(reason) = &out.failure_reason {
                    fnr_fast::ui::error(reason);
                } else {
                    fnr_fast::ui::ok("CI gate passed");
                }
            }
            if !out.passed {
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Map {
            repo,
            crates,
            write,
            check,
            json,
        } => {
            let m = port_map::map(&resolve_in(root, &repo), &crates)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&m)?);
            } else if write {
                let path = m.root.join(port_map::MAP_FILE);
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&path, port_map::markdown(&m))?;
                println!("Wrote {}", path.display());
            } else {
                print!("{}", port_map::markdown(&m));
            }
            if check && m.changed() > 0 {
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Package {
            target,
            name,
            version,
            out,
            fallback,
            bin,
            original,
            native_cpu,
            dry_run,
            json,
        } => match synergy::detect_target(root, target.as_deref()) {
            synergy::TargetKind::MigrateCrate(crate_dir) => {
                let out = package::package(&package::Options {
                    crate_dir: &crate_dir,
                    name,
                    version,
                    out,
                    dry_run,
                    fallback,
                    bin,
                    original,
                })?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&out)?);
                } else {
                    println!(
                        "Packaged {}@{} into {}",
                        out.name,
                        out.version,
                        out.out.display()
                    );
                }
            }
            synergy::TargetKind::FastPort(id) => {
                let res = synergy::package_hotspot_port_with(
                    root,
                    id.as_deref(),
                    out.as_deref(),
                    dry_run,
                    native_cpu,
                )?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&res)?);
                } else {
                    println!(
                        "Packaged hotspot `{}` into {} ({})",
                        res.id,
                        res.out_dir.display(),
                        res.files.join(", ")
                    );
                }
            }
        },
        Cmd::App { command } => return run_app(command, root),
        Cmd::Setup {
            agents,
            remove,
            dry_run,
            no_backup,
            json,
        } => {
            let home = setup::home()?;
            let command = std::env::current_exe()
                .and_then(|p| p.canonicalize())
                .context("locating fast-n-rust binary")?;
            let rep = setup::setup(&setup::Options {
                home: &home,
                agents,
                command,
                remove,
                dry_run,
                backup: !no_backup,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                print!("{}", setup::text(&rep));
            }
        }
        Cmd::Doctor { all, json } => {
            let rep = doctor::run_with_options(root, all);
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                print!("{}", doctor::text(&rep));
            }
            if !rep.ok {
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Clean {
            cache,
            ports,
            rust,
            all,
            force,
            dry_run,
            json,
        } => {
            let out = clean::clean(
                root,
                &clean::CleanOptions {
                    cache,
                    ports,
                    rust,
                    all,
                    force,
                    dry_run,
                },
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                print!("{}", clean::text(&out));
            }
        }
        Cmd::Commands => {
            println!("{}", all_commands_text());
        }
        Cmd::Completions { shell } => {
            let mut cmd = Cli::command();
            for sub in cmd.get_subcommands_mut() {
                if sub.get_name() != "__replay" {
                    *sub = std::mem::take(sub).hide(false);
                }
            }
            let bin = std::env::args()
                .next()
                .and_then(|a| {
                    std::path::Path::new(&a)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                })
                .filter(|n| n == "fnr" || n == "fast-n-rust")
                .unwrap_or_else(|| "fnr".to_string());
            clap_complete::generate(shell, &mut cmd, bin, &mut std::io::stdout());
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_app(cmd: AppCmd, root: &Path) -> Result<ExitCode> {
    match cmd {
        AppCmd::Record {
            repo,
            inputs,
            normalize,
            name,
            no_traps,
            timeout,
            http,
            port,
            server_args,
            fresh,
            pace,
            float_tolerance,
            float_file,
            effects,
            effect_env,
            db_reset,
            db_dump,
            json,
            run,
        } => {
            let eff = effects::EffectsConfig::from_args(
                &effects,
                &effect_env,
                db_reset.as_deref(),
                db_dump.as_deref(),
            )?;
            let out = app::record(&app::RecordOptions {
                repo: &resolve_in(root, &repo),
                run: &run,
                inputs: inputs.as_deref(),
                normalize: &normalize,
                name: name.as_deref(),
                traps: !no_traps,
                timeout_secs: timeout,
                http,
                port,
                server_args: &server_args,
                fresh: &fresh,
                pace_ms: pace,
                float_tolerance,
                float_files: &float_file,
                effects: eff,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("Recorded {} cases into {}", out.cases, out.dir.display());
            }
        }
        AppCmd::Replay {
            repo,
            build,
            timeout,
            port,
            case,
            json,
        } => {
            let out = app::replay(&app::ReplayOptions {
                repo: &resolve_in(root, &repo),
                build: build.as_deref(),
                timeout_secs: timeout,
                port,
                case: case.as_deref(),
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!(
                    "app replay: {}/{} passed ({} failed)",
                    out.passed,
                    out.total,
                    out.failures.len()
                );
            }
            if !out.failures.is_empty() {
                return Ok(ExitCode::FAILURE);
            }
        }
        AppCmd::FrontDoor {
            repo,
            no_check,
            library,
            json,
        } => {
            let out = front_door::front_door(&front_door::FrontDoorOptions {
                repo: &resolve_in(root, &repo),
                library,
                check: !no_check,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!(
                    "Created front-door `{}` in {}",
                    out.binary,
                    out.dir.display()
                );
            }
        }
        AppCmd::Wire {
            repo,
            unit,
            commands,
            routes,
            skip_gate,
            no_check,
            json,
        } => {
            let out = front_door::wire(&front_door::WireOptions {
                repo: &resolve_in(root, &repo),
                unit: &unit,
                commands: &commands,
                routes: &routes,
                skip_gate,
                check: !no_check,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("Wired `{}` (glue: {})", out.unit, out.glue.display());
                if let Some(r) = &out.replay {
                    println!("  replay: {}/{} passed", r.passed, r.total);
                }
            }
        }
        AppCmd::Lift { repo, json } => {
            let out = lift::lift(&lift::LiftOptions {
                repo: &resolve_in(root, &repo),
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if out.lifted {
                println!(
                    "Lifted pure Rust app ({} units){}",
                    out.units.len(),
                    out.binary
                        .as_ref()
                        .map_or_else(String::new, |b| format!(": {}", b.display()))
                );
            } else {
                println!("Lift blocked:");
                for b in &out.blockers {
                    println!("  - {b}");
                }
            }
            if !out.lifted {
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn all_commands_text() -> &'static str {
    "\
Everyday Workflow (IDE & Fast 5%):
  fnr setup [--agent ...]       Register MCP + skills for Cursor, Claude Code, Antigravity, Windsurf, VS Code, Codex, Gemini, OpenCode
  fnr doctor [--all]            Check Rust + project runtimes (or all 14 languages with --all)
  fnr scan -- <cmd>             Profile workload, rank portable hotspots, and diagnose bottlenecks
  fnr go [--agent ...] -- <cmd> Scan, port, verify, enable, and benchmark
  fnr port [id]                 Record calls + scaffold `.fast/ports/<id>` with TASK.md
  fnr verify [id] [--gate]      Replay recorded + edge cases (plus strict fmt/clippy/test/unsafe gate)
  fnr enable [id] | disable     Switch on/off a verified port (reversible via FNR_DISABLE=1)
  fnr bench                     Interleaved 95% CI + peak RSS benchmark
  fnr status                    Show hotspot ports, `rust/` crates, and deploy readiness
  fnr clean --cache|--all       Reclaim Cargo / `.fast/` build caches

Deep Unit Migration, Batching & Fuzzing:
  fnr analyze . [--whole-app]   AST migration map and ranked plan
  fnr profile . -- <cmd>        List callers and Rayon `--batch` opportunities
  fnr escalate <id> [--batch]   Promote a `.fast/` port into a `rust/` unit crate
  fnr bridge . <unit> [--batch] Scaffold a `rust/` unit crate with bindings
  fnr refresh <crate>           Regenerate bindings from `src/port.rs`
  fnr record <crate>            Record edge cases and tests into `cases.jsonl`
  fnr diff <crate>              Replay recorded cases against a unit crate
  fnr fuzz [target]             Property / differential fuzzing with auto-shrinking
  fnr shadow <crate>            Generate side-by-side shadow wrapper

Whole-App Pure Rust Rewrite (`fnr app ...`):
  fnr app record . -- <cmd>     Capture CLI or `--http` server behavior into `rust/app-cases/`
  fnr app replay .              Replay recorded app cases against original or build
  fnr app front-door .          Scaffold `rust/app` (`clap` / `axum`) with original fallback
  fnr app wire . <unit>         Route commands/routes from front-door to ported Rust units
  fnr app lift .                Strip fallback and emit standalone pure Rust binary

Verification, CI & Packaging:
  fnr build [crate | --deploy]  Build unit crate or deploy-build `.fast/ports/*` in CI/Docker
  fnr gate [target]             Strict Definition-of-Done check (`fmt`, `clippy`, `test`, `unsafe`)
  fnr ci [target]               One-shot CI parity/gate/fuzz/speedup pipeline
  fnr package [target]          Package verified port into `dist/` (wheel / npm / native)
  fnr report [target]           Generate Markdown report + SVG card
  fnr map . [--write]           Generate `rust/PORT_MAP.md`

Ops, MCP & Shell Completions:
  fnr mcp [--all-tools]         Run JSON-RPC 2.0 MCP server (7 compact tools by default)
  fnr completions <shell>       Generate bash/zsh/fish/powershell/elvish completions

Tip: run `fnr <command> --help` for full flags on any command."
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_key_unified_cli_invocations() {
        Cli::command().debug_assert();
        let go = Cli::try_parse_from([
            "fnr", "go", "--strict", "--agent", "claude", "--", "python", "app.py",
        ])
        .unwrap();
        assert!(matches!(go.cmd, Cmd::Go { strict: true, .. }));

        let scan = Cli::try_parse_from(["fnr", "scan", "--", "node", "app.mjs"]).unwrap();
        assert!(matches!(scan.cmd, Cmd::Scan { .. }));

        let esc = Cli::try_parse_from(["fnr", "escalate", "app.dedupe", "--batch", "dedupe:names"])
            .unwrap();
        assert!(matches!(esc.cmd, Cmd::Escalate { .. }));

        let clean = Cli::try_parse_from(["fnr", "clean", "--all", "--dry-run"]).unwrap();
        assert!(matches!(
            clean.cmd,
            Cmd::Clean {
                all: true,
                dry_run: true,
                ..
            }
        ));

        let replay = Cli::try_parse_from(["fnr", "__replay", "/tmp/job.json"]).unwrap();
        assert!(matches!(replay.cmd, Cmd::Replay { .. }));

        let lift = Cli::try_parse_from(["fnr", "app", "lift", "."]).unwrap();
        assert!(matches!(
            lift.cmd,
            Cmd::App {
                command: AppCmd::Lift { .. }
            }
        ));

        let comp = Cli::try_parse_from(["fnr", "completions", "bash"]).unwrap();
        assert!(matches!(comp.cmd, Cmd::Completions { .. }));

        let cdb =
            Cli::try_parse_from(["fnr", "scan", "--cdb", "build/compile_commands.json"]).unwrap();
        assert!(matches!(cdb.cmd, Cmd::Scan { cdb: Some(_), .. }));

        let ci =
            Cli::try_parse_from(["fnr", "ci", "--fuzz", "50", "--min-speedup", "2.0"]).unwrap();
        assert!(matches!(ci.cmd, Cmd::Ci { fuzz: 50, .. }));

        let mcp_full = Cli::try_parse_from(["fnr", "mcp", "--all-tools"]).unwrap();
        assert!(matches!(mcp_full.cmd, Cmd::Mcp { all_tools: true }));

        let cmds = Cli::try_parse_from(["fnr", "commands"]).unwrap();
        assert!(matches!(cmds.cmd, Cmd::Commands));

        let doc_all = Cli::try_parse_from(["fnr", "doctor", "--all"]).unwrap();
        assert!(matches!(doc_all.cmd, Cmd::Doctor { all: true, .. }));

        let scan_dur =
            Cli::try_parse_from(["fnr", "scan", "--duration", "5", "--", "python", "app.py"])
                .unwrap();
        assert!(matches!(
            scan_dur.cmd,
            Cmd::Scan {
                duration: Some(5),
                ..
            }
        ));

        for hidden in [
            vec!["fnr", "analyze", "."],
            vec!["fnr", "package"],
            vec!["fnr", "report"],
            vec!["fnr", "gate"],
            vec!["fnr", "fuzz"],
        ] {
            assert!(
                Cli::try_parse_from(&hidden).is_ok(),
                "hidden command should still parse: {hidden:?}"
            );
        }

        let cmd_tree = Cli::command();
        let visible: Vec<&str> = cmd_tree
            .get_subcommands()
            .filter(|s| !s.is_hide_set())
            .map(|s| s.get_name())
            .collect();
        assert!(visible.contains(&"commands"));
        assert!(visible.contains(&"setup"));
        assert!(!visible.contains(&"escalate"));
        assert!(!visible.contains(&"__replay"));
        assert!(all_commands_text().contains("fnr app lift"));
    }

    #[test]
    fn resolve_in_honors_dir_and_app_json_is_silent() {
        let base = abs(&std::env::temp_dir());
        let root = base.join("fnr-ws");
        let other = base.join("other");
        assert_eq!(resolve_in(&root, Path::new(".")), root);
        assert_eq!(
            resolve_in(&root, Path::new("rust/demo")),
            abs(&root.join("rust/demo"))
        );
        assert_eq!(resolve_in(&root, &other), other);

        let lift_json = Cli::try_parse_from(["fnr", "app", "lift", "--json"]).unwrap();
        assert!(is_silent_cmd(&lift_json.cmd));
        let lift_text = Cli::try_parse_from(["fnr", "app", "lift"]).unwrap();
        assert!(!is_silent_cmd(&lift_text.cmd));
    }
}
