//! Cross-engine synergies ("1 + 1 = 3") connecting `fnr-fast` and `fnr-migrate`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};

use fnr_fast::backend::Lang as FastLang;
use fnr_fast::bench::BenchResult;
use fnr_fast::hotspot::Hotspot;
use fnr_fast::pipeline::{self, ScanOut};
use fnr_fast::report::ReportOut;
use fnr_fast::state::{Ctx, Port};
use fnr_migrate::bridge::{self, Allocator, MANIFEST};
use fnr_migrate::gate::{self, Check};
use fnr_migrate::lang::Lang as MigrateLang;
use std::process::Command;

const ENRICH_MARKER: &str = "<!-- fnr:enriched -->";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    MigrateCrate(PathBuf),
    FastPort(Option<String>),
}

/// Auto-detect whether the user passed a `rust/<crate>` directory (with `rust-anything.json`)
/// or a `.fast` hotspot id (or nothing).
pub fn detect_target(root: &Path, target: Option<&str>) -> TargetKind {
    if let Some(raw) = target {
        let direct = PathBuf::from(raw);
        if direct.join(MANIFEST).is_file() {
            return TargetKind::MigrateCrate(direct);
        }
        let under_root = root.join(raw);
        if under_root.join(MANIFEST).is_file() {
            return TargetKind::MigrateCrate(under_root);
        }
        let under_rust = root.join("rust").join(raw);
        if under_rust.join(MANIFEST).is_file() {
            return TargetKind::MigrateCrate(under_rust);
        }
        if raw == "." && root.join(MANIFEST).is_file() {
            return TargetKind::MigrateCrate(root.to_path_buf());
        }
    } else if root.join(MANIFEST).is_file() {
        return TargetKind::MigrateCrate(root.to_path_buf());
    }
    TargetKind::FastPort(target.map(str::to_string))
}

fn to_migrate_lang(lang: FastLang) -> Option<MigrateLang> {
    Some(match lang {
        FastLang::Python => MigrateLang::Python,
        FastLang::Node => MigrateLang::Ts,
        FastLang::Ruby => MigrateLang::Ruby,
        FastLang::Java => MigrateLang::Java,
        FastLang::Go => MigrateLang::Go,
        FastLang::Php => MigrateLang::Php,
        FastLang::C => MigrateLang::C,
        FastLang::CSharp => MigrateLang::Csharp,
        FastLang::Kotlin => MigrateLang::Kotlin,
        FastLang::Cpp | FastLang::R => return None,
    })
}

fn crate_table_key(lang: FastLang) -> &'static str {
    match lang {
        FastLang::Python => "python",
        FastLang::Node => "ts",
        FastLang::Ruby => "ruby",
        FastLang::Java => "java",
        FastLang::Go => "go",
        FastLang::Php => "php",
        FastLang::C | FastLang::Cpp => "c",
        FastLang::CSharp => "csharp",
        FastLang::Kotlin => "kotlin",
        FastLang::R => "r",
    }
}

/// Appends static semantic risks, crate recommendations from `fnr_migrate::crates`,
/// and the strict `gate` checklist to a hotspot port's `TASK.md`.
pub fn enrich_task_md(root: &Path, crate_dir: &Path, hs: &Hotspot, lang: FastLang) -> Result<bool> {
    let task_path = crate_dir.join("TASK.md");
    let Ok(existing) = fs::read_to_string(&task_path) else {
        return Ok(false);
    };
    if existing.contains(ENRICH_MARKER) {
        return Ok(false);
    }

    let source_path = root.join(&hs.file);
    let file_text = fs::read_to_string(&source_path).unwrap_or_else(|_| hs.source.clone());
    let combined_text = format!("{file_text}\n{}", hs.source);

    let mut risks = Vec::new();
    if let Some(mlang) = to_migrate_lang(lang)
        && (fnr_migrate::risks::uses_regex(mlang, &combined_text)
            || (lang == FastLang::Python
                && (combined_text.contains("import re,")
                    || combined_text.contains("re.findall(")
                    || combined_text.contains("re.match(")
                    || combined_text.contains("re.search(")
                    || combined_text.contains("re.sub("))))
    {
        risks.push(
            "Regex semantics: `\\w`, `\\b`, `\\d`, and case-folding differ between Rust `regex` (Unicode-aware) and ASCII-only engines; use `fancy-regex` for lookaround/backreferences.",
        );
    }
    if matches!(
        lang,
        FastLang::Node | FastLang::Java | FastLang::Kotlin | FastLang::CSharp
    ) {
        risks.push("String indexing: JS/Java/Kotlin/C# index UTF-16 code units, whereas Rust `&str` indexes UTF-8 bytes.");
    }
    if matches!(lang, FastLang::Python | FastLang::Ruby) {
        risks.push("Numeric parity: preserve exact integer vs float types and negative floor-division / modulo (`div_euclid` / `rem_euclid`) semantics.");
    }
    if file_text.contains("float")
        || file_text.contains("NaN")
        || file_text.contains("math.")
        || file_text.contains("Math.")
    {
        risks.push("Floating point: handle `NaN`, `±inf`, and `-0.0` explicitly; preserve operation order where possible.");
    }

    // Suggest Rust crates for detected imports / packages from `fnr_migrate::crates`
    let mut crate_hints = BTreeSet::new();
    let table_lang = crate_table_key(lang);
    for line in combined_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("import ")
            || trimmed.starts_with("from ")
            || trimmed.contains("require(")
            || trimmed.starts_with("library(")
            || trimmed.starts_with("requireNamespace(")
            || trimmed.starts_with("use ")
            || trimmed.starts_with("#include")
        {
            for tok in trimmed
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '/')
                .filter(|t| t.len() >= 2)
            {
                if let Some(m) = fnr_migrate::crates::find(table_lang, tok)
                    && !m.crates.is_empty()
                    && m.crates != "-"
                {
                    crate_hints.insert(format!("- `{tok}` → `{}` ({})", m.crates, m.note));
                }
            }
        }
    }

    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut perf_tips = Vec::new();
    let lower_src = hs.source.to_ascii_lowercase();
    if (lower_src.contains("for ") || lower_src.contains("while "))
        && (lower_src.contains("double")
            || lower_src.contains("float")
            || lower_src.contains("f64"))
    {
        perf_tips.push(format!(
            "Numeric loop detected ({cores} logical cores available): process 4 independent items per inner iteration for SIMD/ILP without FMA (preserving exact IEEE-754 rounding), and use dynamic work-stealing (`AtomicI32` row counter or `rayon`) instead of static `.step_by(threads)` or repeated `thread::scope` spawns."
        ));
    }
    if hs.ncalls >= 500 {
        perf_tips.push(format!(
            "High call frequency ({} calls): eliminate per-call heap allocations (`Vec::new()` inside loops, `format!`), reuse `thread_local!` scratch buffers, or escalate to `--batch` if FFI dominates.",
            hs.ncalls
        ));
    }
    if lower_src.contains("len(")
        || lower_src.contains(".length")
        || lower_src.contains("levenshtein")
        || lower_src.contains("split")
        || lower_src.contains("lower")
    {
        perf_tips.push(
            "String/sequence workload: add a `&[u8]` fast path when `s.is_ascii()` before falling back to `Vec<char>`/`encode_utf16()`, and prune early on length difference.".to_string(),
        );
    }

    let mut section = format!(
        "\n\n{ENRICH_MARKER}\n## Static Risks, Performance Blueprint & Gate Checklist (`fast-n-rust`)\n"
    );
    if !perf_tips.is_empty() {
        section.push_str("\n### Performance Blueprint\n");
        for tip in perf_tips {
            section.push_str(&format!("- {tip}\n"));
        }
    }
    if !risks.is_empty() {
        section.push_str("\n### Semantic Traps to Watch\n");
        for r in risks {
            section.push_str(&format!("- {r}\n"));
        }
    }
    if !crate_hints.is_empty() {
        section.push_str("\n### Suggested Crates (from static dependency table)\n");
        for c in crate_hints {
            section.push_str(&format!("{c}\n"));
        }
    }
    section.push_str(
        "\n### Definition of Done (`fnr verify --gate`)\n\
         - `cargo fmt --check` passes cleanly\n\
         - `cargo clippy --all-targets -- -D warnings` passes with zero warnings\n\
         - No unexplained `unsafe` (every `unsafe` needs a `// SAFETY:` comment)\n\
         - No `todo!()` or `unimplemented!()`, and return `Err(...)` instead of panicking on bad inputs\n",
    );

    fs::write(&task_path, format!("{existing}{section}"))?;
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedMode {
    FastHotspot,
    BatchHotLoop,
    WholeApp,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModeAdvice {
    pub mode: RecommendedMode,
    pub headline: String,
    pub next_command: String,
    pub top_portable_share: f64,
}

/// Recommends the best strategy (`fast` 5% hotspot, `--batch` Rayon escalation, or `whole-app` lift)
/// from a `scan` profile.
pub fn advise_mode(scan: &ScanOut) -> ModeAdvice {
    let portable: Vec<&Hotspot> = scan.hotspots.iter().filter(|h| h.worth_it()).collect();
    let top_share: f64 = portable
        .iter()
        .map(|h| h.share)
        .fold(0.0_f64, f64::max)
        .min(1.0);

    if let Some(loop_hs) = portable.iter().find(|h| {
        h.ffi_budget_ratio() > 0.25
            || (h.ncalls >= 500 && (h.tottime / (h.ncalls as f64).max(1.0)) < 5e-5)
    }) {
        let ratio = loop_hs.ffi_budget_ratio();
        let abi_hint = if loop_hs.supports_direct_abi() {
            " (direct C ABI applies, or batch outer caller)"
        } else {
            ""
        };
        return ModeAdvice {
            mode: RecommendedMode::BatchHotLoop,
            headline: format!(
                "`{}` was called {} times (estimated FFI budget {:.0}% of time){abi_hint}; crossing FFI per call can add overhead.",
                loop_hs.id,
                loop_hs.ncalls,
                (ratio * 100.0).min(100.0)
            ),
            next_command: format!(
                "Port its outer caller with `fnr port`, or escalate to a multi-core Rayon batch: `fnr escalate {} --batch {}:items`",
                loop_hs.id, loop_hs.name
            ),
            top_portable_share: top_share,
        };
    }

    if !portable.is_empty() && top_share >= 0.35 {
        let direct_note = if portable[0].supports_direct_abi() {
            " (direct zero-JSON C ABI eligible)"
        } else {
            ""
        };
        return ModeAdvice {
            mode: RecommendedMode::FastHotspot,
            headline: format!(
                "Top portable hotspot(s) cover up to {:.1}% of runtime{direct_note} — ideal 5% Rust speedup target.",
                top_share * 100.0
            ),
            next_command: format!(
                "fnr port {}  (or `fnr go --agent claude -- ...`)",
                portable[0].id
            ),
            top_portable_share: top_share,
        };
    }

    ModeAdvice {
        mode: RecommendedMode::WholeApp,
        headline: if portable.is_empty() {
            "No isolated pure hotspot >=2% stood out (or helpers do I/O). Consider incremental whole-app migration.".to_string()
        } else {
            format!(
                "Runtime is spread across many functions (top portable = {:.1}%). Consider incremental whole-app migration.",
                top_share * 100.0
            )
        },
        next_command: "fnr analyze . --whole-app  &&  fnr app record . -- <cmd>".to_string(),
        top_portable_share: top_share,
    }
}

#[derive(Debug, Serialize)]
pub struct HotspotGateOutcome {
    pub id: String,
    pub crate_dir: PathBuf,
    pub passed: bool,
    pub checks: Vec<Check>,
}

fn run_cargo_step(
    crate_dir: &Path,
    target_dir: &Path,
    python_exe: Option<&str>,
    args: &[&str],
) -> (bool, String) {
    let mut cmd = Command::new("cargo");
    cmd.args(args)
        .env("CARGO_TARGET_DIR", target_dir)
        .current_dir(crate_dir);
    if let Some(py) = python_exe {
        cmd.env("PYO3_PYTHON", py);
    }
    match cmd.output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = text.lines().rev().take(15).collect();
            let summary = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
            (out.status.success(), summary)
        }
        Err(e) => (false, e.to_string()),
    }
}

fn user_port_file(crate_dir: &Path) -> PathBuf {
    let port_rs = crate_dir.join("src/port.rs");
    if port_rs.is_file() {
        port_rs
    } else {
        crate_dir.join("src/lib.rs")
    }
}

/// Runs hotspot-aware `gate` checks (`fmt`, `clippy --lib -D warnings`, `cargo test` / `check`,
/// `ported` stub check, `unsafe` SAFETY audit on the user port file, and `verify` parity).
pub fn gate_hotspot_port(root: &Path, id: Option<&str>) -> Result<HotspotGateOutcome> {
    let ctx = Ctx::open(root)?;
    let resolved = ctx.resolve_port(id)?;
    let port = ctx.state.ports.get(&resolved).context("missing port")?;
    let crate_dir = ctx.crate_dir(port);
    let target_dir = fnr_fast::backend::target_dir(&ctx);
    let manifest_str = crate_dir.join("Cargo.toml").to_string_lossy().into_owned();
    let py_exe = (port.lang == FastLang::Python)
        .then(|| ctx.runtime().ok().map(str::to_string))
        .flatten();
    let py_ref = py_exe.as_deref();

    let mut checks = Vec::new();

    // 1. fmt
    let (fmt_ok, fmt_out) = run_cargo_step(
        &crate_dir,
        &target_dir,
        None,
        &["fmt", "--check", "--manifest-path", &manifest_str],
    );
    checks.push(Check {
        name: "fmt",
        passed: fmt_ok,
        detail: if fmt_ok {
            "formatted".into()
        } else {
            format!("run `cargo fmt --manifest-path {manifest_str}`\n{fmt_out}")
        },
        fix: (!fmt_ok).then(|| format!("cargo fmt --manifest-path {manifest_str}")),
    });

    // 2. clippy --lib -D warnings
    let (clip_ok, clip_out) = run_cargo_step(
        &crate_dir,
        &target_dir,
        py_ref,
        &[
            "clippy",
            "--lib",
            "--manifest-path",
            &manifest_str,
            "--",
            "-D",
            "warnings",
        ],
    );
    checks.push(Check {
        name: "clippy",
        passed: clip_ok,
        detail: if clip_ok {
            "no warnings".into()
        } else {
            clip_out
        },
        fix: (!clip_ok)
            .then(|| format!("cargo clippy --lib --manifest-path {manifest_str} -- -D warnings")),
    });

    // 3. cargo test (or check when cdylib test linking isn't available without host runtime)
    let cargo_toml_txt = fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap_or_default();
    let (test_ok, test_out, test_name) = match port.lang {
        FastLang::Python => {
            if cargo_toml_txt.contains("extension-module = [") {
                let (ok, out) = run_cargo_step(
                    &crate_dir,
                    &target_dir,
                    py_ref,
                    &[
                        "test",
                        "--no-default-features",
                        "--quiet",
                        "--manifest-path",
                        &manifest_str,
                    ],
                );
                (ok, out, "cargo test")
            } else {
                let (ok, out) = run_cargo_step(
                    &crate_dir,
                    &target_dir,
                    py_ref,
                    &["check", "--lib", "--manifest-path", &manifest_str],
                );
                (ok, out, "cargo check")
            }
        }
        FastLang::Node => {
            let (ok, out) = run_cargo_step(
                &crate_dir,
                &target_dir,
                None,
                &["check", "--lib", "--manifest-path", &manifest_str],
            );
            (ok, out, "cargo check")
        }
        _ => {
            let (ok, out) = run_cargo_step(
                &crate_dir,
                &target_dir,
                None,
                &["test", "--quiet", "--manifest-path", &manifest_str],
            );
            (ok, out, "cargo test")
        }
    };
    checks.push(Check {
        name: test_name,
        passed: test_ok,
        detail: if test_ok { "passed".into() } else { test_out },
        fix: (!test_ok).then(|| format!("cargo test --manifest-path {manifest_str}")),
    });

    // 4. ported stubs check on user file
    let user_file = user_port_file(&crate_dir);
    let user_text = fs::read_to_string(&user_file).unwrap_or_default();
    let todo_count = gate::stubs(&user_text);
    checks.push(Check {
        name: "ported",
        passed: todo_count == 0,
        detail: if todo_count == 0 {
            format!(
                "nothing in {} is `unimplemented!`, `todo!` or only a `panic!`",
                user_file
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("port")
            )
        } else {
            format!(
                "{todo_count} place(s) in {} still `unimplemented!`, `todo!` or panic-only",
                user_file.display()
            )
        },
        fix: (todo_count > 0).then(|| format!("finish porting {}", user_file.display())),
    });

    // 5. unsafe SAFETY-comment audit ONLY on user file (generated C-ABI glue is excluded)
    let (unexplained, explained) = gate::unsafe_blocks(&user_file);
    checks.push(Check {
        name: "unsafe",
        passed: unexplained.is_empty(),
        detail: if unexplained.is_empty() {
            format!("{explained} unsafe uses in port source, each with a `// SAFETY:` comment")
        } else {
            format!(
                "unsafe without a `// SAFETY:` comment: {}",
                unexplained.join(", ")
            )
        },
        fix: (!unexplained.is_empty())
            .then(|| "add a `// SAFETY:` comment above each `unsafe`, or remove `unsafe`".into()),
    });

    // 6. verify parity + source hash freshness
    let cur_hash = fnr_fast::backend::source_hash(&crate_dir).ok();
    let verified = port
        .verify
        .as_ref()
        .is_some_and(|v| v.ok() && (v.source_hash.is_none() || v.source_hash == cur_hash));
    let verify_detail = match &port.verify {
        Some(v) if verified => format!(
            "{}/{} recorded cases match current sources",
            v.passed, v.total
        ),
        Some(v) if v.failed > 0 => format!("{}/{} failed verification", v.failed, v.total),
        Some(_) => "sources changed since last verify".to_string(),
        None => "not verified yet".to_string(),
    };
    checks.push(Check {
        name: "verify",
        passed: verified,
        detail: verify_detail,
        fix: (!verified).then(|| format!("fnr verify {resolved}")),
    });

    let passed = checks.iter().all(|c| c.passed);
    Ok(HotspotGateOutcome {
        id: resolved,
        crate_dir,
        passed,
        checks,
    })
}

pub fn format_gate_failures(checks: &[Check]) -> String {
    let mut lines = Vec::new();
    for c in checks.iter().filter(|c| !c.passed) {
        lines.push(format!("[FAIL {}] {}", c.name, c.detail));
        if let Some(fix) = &c.fix {
            lines.push(format!("  fix: {fix}"));
        }
    }
    lines.join("\n")
}

#[derive(Debug, Serialize)]
pub struct EscalateOut {
    pub port_id: String,
    pub unit_id: String,
    pub migrate_crate: PathBuf,
    pub recorded_inputs: Option<PathBuf>,
    pub imported_cases: usize,
    pub reference_rs: Option<PathBuf>,
    pub disabled_active_patch: bool,
    pub auto_ported: bool,
    pub next_steps: Vec<String>,
}

fn convert_calls_jsonl(calls_path: &Path, fn_name: &str, out_path: &Path) -> Result<usize> {
    let text = fs::read_to_string(calls_path)?;
    let mut lines = Vec::new();
    for raw in text.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(v) = serde_json::from_str::<Value>(raw)
            && let Some(args) = v.get("args")
        {
            let has_kwargs = v
                .get("kwargs")
                .and_then(Value::as_object)
                .is_some_and(|m| !m.is_empty());
            let row = if has_kwargs {
                json!({ "fn": fn_name, "args": args, "kwargs": v["kwargs"] })
            } else {
                json!({ "fn": fn_name, "args": args })
            };
            lines.push(serde_json::to_string(&row)?);
        }
    }
    if lines.is_empty() {
        return Ok(0);
    }
    fs::write(out_path, lines.join("\n") + "\n")?;
    Ok(lines.len())
}

fn find_calls_file(crate_dir: &Path, port: &Port) -> Option<PathBuf> {
    for candidate in [
        crate_dir.join("recordings.jsonl"),
        crate_dir.join("calls.jsonl"),
        crate_dir.join(format!("{}.jsonl", port.func)),
    ] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    fs::read_dir(crate_dir).ok()?.flatten().find_map(|e| {
        let p = e.path();
        (p.extension().and_then(|s| s.to_str()) == Some("jsonl")).then_some(p)
    })
}

fn has_unsafe_token(src: &str) -> bool {
    src.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|tok| tok == "unsafe")
}

fn find_matching_brace(text: &str, open_idx: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open_idx;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_top_level_params(params_str: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut angle = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    for ch in params_str.chars() {
        match ch {
            '<' => angle += 1,
            '>' => angle = angle.saturating_sub(1),
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            ',' if angle == 0 && paren == 0 && bracket == 0 => {
                if let Some((n, t)) = cur.split_once(':') {
                    out.push((n.trim().to_string(), t.trim().to_string()));
                }
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    if let Some((n, t)) = cur.split_once(':') {
        out.push((n.trim().to_string(), t.trim().to_string()));
    }
    out
}

fn normalize_c_scalar_type(ty: &str) -> &str {
    match ty.trim() {
        "std::ffi::c_int" | "c_int" | "i32" => "i32",
        "std::ffi::c_uint" | "c_uint" | "u32" => "u32",
        "std::ffi::c_longlong" | "c_longlong" | "i64" => "i64",
        "std::ffi::c_ulonglong" | "c_ulonglong" | "u64" => "u64",
        "std::ffi::c_short" | "c_short" | "i16" => "i16",
        "std::ffi::c_ushort" | "c_ushort" | "u16" => "u16",
        "std::ffi::c_schar" | "c_schar" | "i8" => "i8",
        "std::ffi::c_uchar" | "c_uchar" | "u8" => "u8",
        "std::ffi::c_float" | "c_float" | "f32" => "f32",
        "std::ffi::c_double" | "c_double" | "f64" => "f64",
        other => other,
    }
}

fn splice_c_abi_stub(dst_port: &str, fast_port: &str, func_name: &str) -> Option<String> {
    let sig_needle = format!("pub fn {func_name}(");
    let fn_start = dst_port.find(&sig_needle)?;
    let paren_open = fn_start + sig_needle.len() - 1;
    let mut depth = 0usize;
    let mut paren_close = None;
    for (off, ch) in dst_port[paren_open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    paren_close = Some(paren_open + off);
                    break;
                }
            }
            _ => {}
        }
    }
    let paren_close = paren_close?;
    let brace_rel = dst_port[paren_close..].find('{')?;
    let brace_open = paren_close + brace_rel;
    let brace_close = find_matching_brace(dst_port, brace_open)?;
    let body = &dst_port[brace_open + 1..brace_close];
    if !body.contains("unimplemented!") && !body.contains("todo!") {
        return None;
    }

    let params_slice = &dst_port[paren_open + 1..paren_close];
    let params = split_top_level_params(params_slice);
    // Only auto-bridge when every parameter is a primitive scalar (no slice/OutPtr marshalling mismatch).
    if params
        .iter()
        .any(|(_, ty)| ty.contains('&') || ty.contains("Option<") || ty.contains("crate::"))
    {
        return None;
    }
    let sig_ret = dst_port[paren_close + 1..brace_open].trim();
    let has_ret = sig_ret.starts_with("->");

    let fast_sig_pos = fast_port.find(&sig_needle)?;
    let fast_paren_open = fast_sig_pos + sig_needle.len() - 1;
    let fast_paren_close = fast_port[fast_paren_open..].find(')')? + fast_paren_open;
    let fast_params = split_top_level_params(&fast_port[fast_paren_open + 1..fast_paren_close]);
    let fast_brace_open = fast_port[fast_paren_close..].find('{')? + fast_paren_close;
    let fast_returns_result = fast_port[fast_paren_close + 1..fast_brace_open]
        .trim()
        .contains("Result<");

    let mut used_cast = false;
    let arg_exprs: Vec<String> = params
        .iter()
        .enumerate()
        .map(|(idx, (name, dst_ty))| {
            let same_ty = fast_params.get(idx).is_some_and(|(_, fty)| {
                normalize_c_scalar_type(dst_ty) == normalize_c_scalar_type(fty)
            });
            if same_ty {
                name.clone()
            } else {
                used_cast = true;
                format!("{name} as _")
            }
        })
        .collect();

    let call = format!("__fnr_hotspot_impl::{func_name}({})", arg_exprs.join(", "));
    let replacement_body = match (has_ret, fast_returns_result) {
        (true, true) => format!("\n    {call}.unwrap_or_default()\n"),
        (true, false) => format!("\n    {call}\n"),
        (false, true) => format!("\n    let _ = {call};\n"),
        (false, false) => format!("\n    {call};\n"),
    };

    let mut out = String::with_capacity(dst_port.len() + fast_port.len() + 128);
    if used_cast {
        out.push_str(&dst_port[..fn_start]);
        out.push_str("#[allow(trivial_numeric_casts)]\n");
        out.push_str(&dst_port[fn_start..brace_open + 1]);
    } else {
        out.push_str(&dst_port[..brace_open + 1]);
    }
    out.push_str(&replacement_body);
    out.push_str(&dst_port[brace_close..]);
    out.push_str("\n#[allow(dead_code, clippy::all)]\nmod __fnr_hotspot_impl {\n");
    for line in fast_port.lines() {
        // Strip outer crate-level attributes inside the nested module.
        if line.trim_start().starts_with("#![") {
            continue;
        }
        out.push_str("    ");
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("}\n");
    Some(out)
}

/// Attempts to splice an already-implemented C-bridge hotspot `src/port.rs` into a freshly
/// bridged `rust/<unit>` crate, validating with `bridge::refresh` and rolling back on mismatch.
pub(crate) fn try_splice_hotspot_port(
    fast_crate: &Path,
    bridged_crate: &Path,
    func_name: &str,
    lang: FastLang,
) -> Result<bool> {
    if !lang.c_bridge() {
        return Ok(false);
    }
    let fast_port_path = fast_crate.join("src/port.rs");
    let dst_port_path = bridged_crate.join("src/port.rs");
    if !fast_port_path.is_file() || !dst_port_path.is_file() {
        return Ok(false);
    }

    let fast_port = fs::read_to_string(&fast_port_path)?;
    if fast_port.contains("todo!(")
        || fast_port.contains("unimplemented!(")
        || !fast_port.contains(&format!("pub fn {func_name}("))
    {
        return Ok(false);
    }

    let dst_port = fs::read_to_string(&dst_port_path)?;
    if dst_port.contains("#![forbid(unsafe_code)]") && has_unsafe_token(&fast_port) {
        return Ok(false);
    }

    let manifest = bridge::Manifest::load(bridged_crate)?;
    let candidate = match manifest.kind {
        bridge::Kind::CAbi => splice_c_abi_stub(&dst_port, &fast_port, func_name),
        bridge::Kind::Ffi
            if manifest.exports.len() == 1 && manifest.exports[0].name == func_name =>
        {
            if dst_port.contains("#![forbid(unsafe_code)]")
                && !fast_port.contains("#![forbid(unsafe_code)]")
            {
                Some(format!("#![forbid(unsafe_code)]\n\n{fast_port}"))
            } else {
                Some(fast_port)
            }
        }
        _ => None,
    };

    let Some(new_port) = candidate else {
        return Ok(false);
    };
    fs::write(&dst_port_path, &new_port)?;
    match bridge::refresh(bridged_crate) {
        Ok(_) => Ok(true),
        Err(_) => {
            fs::write(&dst_port_path, &dst_port)?;
            let _ = bridge::refresh(bridged_crate);
            Ok(false)
        }
    }
}

/// Promotes a `.fast/ports/<id>` hotspot port into a full `rust/<crate>` unit crate
/// supporting `--batch` (Rayon), `fuzz`, `shadow`, `gate`, and `app wire`.
pub fn escalate_hotspot(root: &Path, id: Option<&str>, batch: Option<&str>) -> Result<EscalateOut> {
    let ctx = Ctx::open(root)?;
    let resolved = ctx.resolve_port(id)?;
    let port = ctx
        .state
        .ports
        .get(&resolved)
        .context("missing port")?
        .clone();
    let fast_crate = ctx.crate_dir(&port);

    let mlang = to_migrate_lang(port.lang).with_context(|| {
        format!(
            "{} hotspot ports cannot yet escalate into `rust/` units (C++ and R are supported via the fast hotspot loop)",
            port.lang.label()
        )
    })?;

    // If the inline hotspot patch is currently enabled in source files, disable it first so
    // AST analysis and subsequent `record` observe clean, unpatched original code.
    let disabled_active_patch = if port.enabled {
        pipeline::disable(&ctx.root, Some(&resolved))?;
        true
    } else {
        false
    };

    let report = fnr_migrate::analyze::analyze_lang(&ctx.root, mlang)?;
    let norm_file = port.file.replace('\\', "/");
    let unit = report
        .units
        .iter()
        .find(|u| {
            u.id == norm_file
                || u.files.iter().any(|f| f == &norm_file)
                || norm_file.starts_with(u.id.trim_end_matches('/'))
        })
        .with_context(|| {
            format!(
                "no `analyze` unit covers `{}` (run `fnr analyze .` to inspect units)",
                port.file
            )
        })?;

    let batch_vec: Vec<String> = batch.map(|b| vec![b.to_string()]).unwrap_or_default();
    let bridged = bridge::bridge(&bridge::Options {
        root: &ctx.root,
        unit: &unit.id,
        kind: None,
        out: None,
        name: None,
        force: false,
        keep: Vec::new(),
        lang: Some(mlang),
        batch: batch_vec,
        as_json: Vec::new(),
        allocator: Allocator::default(),
    })?;

    let mut imported_cases = 0usize;
    let mut recorded_inputs = None;
    if let Some(calls) = find_calls_file(&fast_crate, &port) {
        let dest = bridged.crate_dir.join("recorded-inputs.jsonl");
        let n = convert_calls_jsonl(&calls, &port.func, &dest)?;
        if n > 0 {
            imported_cases = n;
            recorded_inputs = Some(dest);
        }
    }

    let mut reference_rs = None;
    for candidate in ["src/port.rs", "src/lib.rs"] {
        let p = fast_crate.join(candidate);
        if p.is_file() {
            let dst = bridged.crate_dir.join("hotspot_reference.rs");
            fs::copy(&p, &dst)?;
            reference_rs = Some(dst);
            break;
        }
    }

    merge_extra_dependencies(
        &fast_crate.join("Cargo.toml"),
        &bridged.crate_dir.join("Cargo.toml"),
    )?;
    let fast_kd = fast_crate.join("known-diffs.jsonl");
    if fast_kd.is_file() {
        let _ = fs::copy(&fast_kd, bridged.crate_dir.join("known-diffs.jsonl"));
    }

    let auto_ported =
        try_splice_hotspot_port(&fast_crate, &bridged.crate_dir, &port.func, port.lang)?;

    let cdisp = bridged.crate_dir.display().to_string();
    let mut next_steps = Vec::new();
    if let Some(ref inp) = recorded_inputs {
        next_steps.push(format!("fnr record {cdisp} --inputs {}", inp.display()));
    } else {
        next_steps.push(format!("fnr record {cdisp}"));
    }
    if auto_ported {
        next_steps.push(format!(
            "Auto-spliced `{}` into {cdisp}/src/port.rs (backup saved at hotspot_reference.rs)",
            port.func
        ));
    } else {
        next_steps.push(format!(
            "Port `{}` into {cdisp}/src/port.rs (reference saved at hotspot_reference.rs)",
            port.func
        ));
    }
    next_steps.push(format!(
        "fnr build {cdisp} && fnr diff {cdisp} && fnr fuzz {cdisp} && fnr gate {cdisp}"
    ));

    Ok(EscalateOut {
        port_id: resolved,
        unit_id: unit.id.clone(),
        migrate_crate: bridged.crate_dir,
        recorded_inputs,
        imported_cases,
        reference_rs,
        disabled_active_patch,
        auto_ported,
        next_steps,
    })
}

/// Runs the hotspot `port` command plus `enrich_task_md`.
pub fn port_and_enrich(root: &Path, id: Option<&str>, samples: usize) -> Result<pipeline::PortOut> {
    let out = pipeline::port(root, id, samples)?;
    if let Ok(ctx) = Ctx::open(root)
        && let Some(hs) = ctx.state.hotspots.iter().find(|h| h.id == out.id)
    {
        let _ = enrich_task_md(root, &out.crate_dir, hs, ctx.state.lang);
    }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct GoHotspotOptions<'a> {
    pub command: Vec<String>,
    pub agent: Option<&'a str>,
    pub top: usize,
    pub attempts: usize,
    pub runs: usize,
    pub samples: usize,
    pub strict: bool,
    pub float_tolerance: Option<f64>,
    pub min_speedup: Option<f64>,
    pub print_progress: bool,
}

#[derive(Debug, Serialize)]
pub struct GoHotspotPortSummary {
    pub id: String,
    pub crate_dir: PathBuf,
    pub verified: bool,
    pub gate_passed: Option<bool>,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct GoHotspotOutcome {
    pub advice: ModeAdvice,
    pub ports: Vec<GoHotspotPortSummary>,
    pub bench: Option<BenchResult>,
    pub report: Option<ReportOut>,
}

/// Unified end-to-end hotspot `go` pipeline shared by CLI and MCP.
pub fn go_hotspot(root: &Path, opts: GoHotspotOptions<'_>) -> Result<GoHotspotOutcome> {
    let scanned = pipeline::scan(root, opts.command)?;
    let advice = advise_mode(&scanned);
    if opts.print_progress {
        fnr_fast::show::scan(&scanned);
        println!("\nMode Advisor: {}", advice.headline);
        println!("  → {}", advice.next_command);
    }

    let picks: Vec<String> = scanned
        .hotspots
        .iter()
        .filter(|h| h.worth_it())
        .take(opts.top.max(1))
        .map(|h| h.id.clone())
        .collect();
    anyhow::ensure!(
        !picks.is_empty(),
        "nothing worth porting: no portable function takes {}% or more of the run",
        fnr_fast::hotspot::WORTH_IT * 100.0
    );

    let mut summaries = Vec::new();
    let mut enabled_count = 0usize;

    for id in &picks {
        let pout = port_and_enrich(root, Some(id), opts.samples)?;
        if opts.print_progress {
            fnr_fast::show::port(&pout, opts.agent.is_none());
        }

        let Some(agent_spec) = opts.agent else {
            summaries.push(GoHotspotPortSummary {
                id: id.clone(),
                crate_dir: pout.crate_dir,
                verified: false,
                gate_passed: None,
                enabled: false,
            });
            continue;
        };

        let mut v = pipeline::agent_port(root, id, agent_spec, opts.attempts)?;
        if opts.float_tolerance.is_some() && !v.ok() {
            let vopts = pipeline::VerifyOptions {
                float_tolerance: opts.float_tolerance,
                known_diffs: None,
                min_speedup: opts.min_speedup,
            };
            let (_, re_v) = pipeline::verify_with(root, Some(id), &vopts)?;
            v = re_v;
        }
        let mut gate_passed = None;

        if v.ok() && opts.strict {
            let mut g = gate_hotspot_port(root, Some(id))?;
            if !g.passed {
                let prompt = format!(
                    "Fix these `gate` failures in {}:\n{}\n",
                    pout.crate_dir.display(),
                    format_gate_failures(&g.checks)
                );
                let log = pout.crate_dir.join("gate-fix.log");
                let _ = fnr_fast::agent::run(agent_spec, &pout.crate_dir, &prompt, &log);
                let vopts = pipeline::VerifyOptions {
                    float_tolerance: opts.float_tolerance,
                    known_diffs: None,
                    min_speedup: opts.min_speedup,
                };
                let (_, re_v) = pipeline::verify_with(root, Some(id), &vopts)?;
                v = re_v;
                g = gate_hotspot_port(root, Some(id))?;
            }
            gate_passed = Some(g.passed);
        }

        let speed_ok = !v.slower() && !v.below_min_speedup(opts.min_speedup);
        let can_enable = v.ok() && speed_ok && (!opts.strict || gate_passed == Some(true));
        if can_enable {
            pipeline::enable(root, Some(id), false)?;
            enabled_count += 1;
        }
        summaries.push(GoHotspotPortSummary {
            id: id.clone(),
            crate_dir: pout.crate_dir,
            verified: v.ok(),
            gate_passed,
            enabled: can_enable,
        });
    }

    let (bench, report) = if opts.agent.is_some() {
        anyhow::ensure!(
            enabled_count > 0,
            if opts.strict {
                "no port passed verification, strict gate, and per-call speed check"
            } else {
                "no port both passed verification and ran faster"
            }
        );
        let b = pipeline::bench(root, opts.runs)?;
        if opts.print_progress {
            fnr_fast::show::bench(&b);
        }
        let r = pipeline::report(root)?;
        if opts.print_progress {
            fnr_fast::show::report(&r);
        }
        (Some(b), Some(r))
    } else {
        (None, None)
    };

    Ok(GoHotspotOutcome {
        advice,
        ports: summaries,
        bench,
        report,
    })
}

fn merge_extra_dependencies(src_cargo: &Path, dst_cargo: &Path) -> Result<()> {
    let Ok(src_txt) = fs::read_to_string(src_cargo) else {
        return Ok(());
    };
    let Ok(dst_txt) = fs::read_to_string(dst_cargo) else {
        return Ok(());
    };
    let Ok(src_doc) = src_txt.parse::<toml_edit::DocumentMut>() else {
        return Ok(());
    };
    let Ok(mut dst_doc) = dst_txt.parse::<toml_edit::DocumentMut>() else {
        return Ok(());
    };
    let Some(src_deps) = src_doc.get("dependencies").and_then(|v| v.as_table()) else {
        return Ok(());
    };
    let skip = [
        "serde",
        "serde_json",
        "pyo3",
        "napi",
        "napi-derive",
        "numpy",
    ];
    let mut changed = false;
    if !dst_doc.contains_key("dependencies") {
        dst_doc["dependencies"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    if let Some(dst_deps) = dst_doc
        .get_mut("dependencies")
        .and_then(|v| v.as_table_mut())
    {
        for (k, v) in src_deps {
            if !skip.contains(&k) && !dst_deps.contains_key(k) {
                dst_deps.insert(k, v.clone());
                changed = true;
            }
        }
    }
    if changed {
        fs::write(dst_cargo, dst_doc.to_string())?;
    }
    Ok(())
}

struct Rng64(u64);

impl Rng64 {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x.max(1);
        self.0
    }

    fn usize_below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() as usize) % bound
        }
    }
}

fn mutate_value(sample: &Value, rng: &mut Rng64, step: usize) -> Value {
    match sample {
        Value::Null => Value::Null,
        Value::Bool(_) => Value::Bool(rng.next_u64().is_multiple_of(2)),
        Value::Number(n) => {
            if n.is_f64() {
                let base = n.as_f64().unwrap_or(0.0);
                let edge_pool = [0.0, -0.0, 1.0, -1.0, 1e-9, -1e-9, -base, base * 0.5];
                if step.is_multiple_of(7) {
                    json!(edge_pool[rng.usize_below(edge_pool.len())])
                } else {
                    let scale = ((rng.next_u64() % 401) as f64 - 200.0) / 100.0;
                    let jitter = ((rng.next_u64() % 401) as f64 - 200.0) / 100.0;
                    json!(base * scale + jitter)
                }
            } else {
                let base = n.as_i64().unwrap_or(0);
                let mag = base.unsigned_abs();
                if step.is_multiple_of(5) {
                    let edge_pool = [
                        0i64,
                        1,
                        -1,
                        2,
                        -2,
                        base.saturating_sub(1),
                        -base.saturating_abs(),
                    ];
                    json!(edge_pool[rng.usize_below(edge_pool.len())])
                } else if mag <= 32 {
                    json!((rng.next_u64() % 65) as i64 - 32)
                } else if !step.is_multiple_of(4) {
                    let lo = -(mag.min(64) as i64);
                    let hi = mag.min(128) as i64;
                    let span = (hi - lo) as u64 + 1;
                    json!(lo + (rng.next_u64() % span) as i64)
                } else {
                    let bound = mag.min(i64::MAX as u64);
                    json!((rng.next_u64() % (bound + 1)) as i64)
                }
            }
        }
        Value::String(s) => {
            let edges = [
                "",
                " ",
                "a",
                "Łukasz 🚀",
                "café naïve",
                "\n\t\r",
                &"x".repeat(256),
            ];
            if step.is_multiple_of(5) {
                Value::String(edges[rng.usize_below(edges.len())].to_string())
            } else {
                let mut chars: Vec<char> = s.chars().collect();
                if !chars.is_empty() {
                    let idx = rng.usize_below(chars.len());
                    let pool = ['a', 'Z', '0', ' ', 'é', 'Ł', '_'];
                    chars[idx] = pool[rng.usize_below(pool.len())];
                    if rng.next_u64().is_multiple_of(3) && chars.len() > 1 {
                        chars.pop();
                    }
                }
                Value::String(chars.into_iter().collect())
            }
        }
        Value::Array(items) => {
            if step.is_multiple_of(8) {
                return Value::Array(Vec::new());
            }
            if items.is_empty() {
                return Value::Array(Vec::new());
            }
            let target_len = if step % 9 == 1 {
                1
            } else {
                (rng.usize_below(items.len().saturating_mul(2).max(2))).min(64)
            };
            let mut out = Vec::with_capacity(target_len);
            for k in 0..target_len {
                let proto = &items[k % items.len()];
                out.push(mutate_value(proto, rng, step + k));
            }
            Value::Array(out)
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), mutate_value(v, rng, step + 1));
            }
            Value::Object(out)
        }
    }
}

pub fn synthesize_fuzz_cases(
    recordings_jsonl: &Path,
    count: usize,
    seed: u64,
) -> Result<Vec<Value>> {
    let text = fs::read_to_string(recordings_jsonl)
        .with_context(|| format!("reading {}", recordings_jsonl.display()))?;
    let seeds: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    anyhow::ensure!(
        !seeds.is_empty(),
        "no recorded cases in {} to guide fuzzing",
        recordings_jsonl.display()
    );
    let mut rng = Rng64(seed.max(0x9e37_79b9_7f4a_7c15));
    let mut out = Vec::with_capacity(count);
    for step in 0..count {
        let base = &seeds[step % seeds.len()];
        let args = base["args"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .enumerate()
                    .map(|(j, v)| mutate_value(v, &mut rng, step + j))
                    .collect()
            })
            .unwrap_or_default();
        let mut obj = json!({ "args": Value::Array(args) });
        if let Some(kw) = base.get("kwargs").and_then(Value::as_object) {
            let mut m = serde_json::Map::new();
            for (k, v) in kw {
                m.insert(k.clone(), mutate_value(v, &mut rng, step + 3));
            }
            obj["kwargs"] = Value::Object(m);
        }
        out.push(obj);
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct HotspotFuzzOutcome {
    pub id: String,
    pub mode: String,
    pub seed: u64,
    pub cases: usize,
    pub passed: usize,
    pub mismatches: usize,
    pub panics: usize,
    pub nondeterministic: usize,
    pub errors: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oracle_note: Option<String>,
    pub saved_regressions: usize,
    pub failures_file: Option<PathBuf>,
    pub report: fnr_fast::native::NativeFuzzReport,
}

fn shrink_candidates(args: &Value) -> Vec<Value> {
    let Some(arr) = args.as_array() else {
        return Vec::new();
    };
    let mut raw = Vec::<Value>::new();
    let mut push_arg_edit = |idx: usize, val: Value| {
        let mut next = arr.clone();
        next[idx] = val;
        raw.push(Value::Array(next));
    };
    for (i, v) in arr.iter().enumerate().take(4) {
        match v {
            Value::Array(items) if !items.is_empty() => {
                push_arg_edit(i, Value::Array(Vec::new()));
                if items.len() > 1 {
                    let mid = items.len() / 2;
                    push_arg_edit(i, Value::Array(items[..mid].to_vec()));
                    push_arg_edit(i, Value::Array(items[mid..].to_vec()));
                    let mut drop_mid = items.clone();
                    drop_mid.remove(mid);
                    push_arg_edit(i, Value::Array(drop_mid));
                } else if let Value::Number(n) = &items[0] {
                    if let Some(iv) = n.as_i64()
                        && iv != 0
                    {
                        push_arg_edit(i, json!([iv / 2]));
                        push_arg_edit(i, json!([0]));
                    }
                } else if let Value::String(s) = &items[0]
                    && s.len() > 1
                {
                    let half: String = s.chars().take(s.chars().count() / 2).collect();
                    push_arg_edit(i, Value::Array(vec![Value::String(half)]));
                }
            }
            Value::String(s) if !s.is_empty() => {
                push_arg_edit(i, Value::String(String::new()));
                let cc = s.chars().count();
                if cc > 1 {
                    let h1: String = s.chars().take(cc / 2).collect();
                    let h2: String = s.chars().skip(cc / 2).collect();
                    push_arg_edit(i, Value::String(h1));
                    push_arg_edit(i, Value::String(h2));
                }
                if !s.is_ascii() {
                    push_arg_edit(i, Value::String("a".into()));
                }
            }
            Value::Number(n) => {
                if let Some(iv) = n.as_i64()
                    && iv != 0
                {
                    push_arg_edit(i, json!(0));
                    if iv.abs() > 1 {
                        push_arg_edit(i, json!(iv / 2));
                        push_arg_edit(i, json!(1));
                    }
                    if iv < 0 {
                        push_arg_edit(i, json!(-1));
                    }
                } else if let Some(fv) = n.as_f64()
                    && fv != 0.0
                {
                    push_arg_edit(i, json!(0.0));
                    if fv.fract() != 0.0 {
                        push_arg_edit(i, json!(fv.trunc()));
                    }
                    push_arg_edit(i, json!(1.0));
                }
            }
            _ => {}
        }
    }
    let orig_len = serde_json::to_string(args).map(|s| s.len()).unwrap_or(0);
    let mut seen = std::collections::HashSet::new();
    let mut uniq: Vec<(usize, Value)> = raw
        .into_iter()
        .filter_map(|v| {
            let s = serde_json::to_string(&v).ok()?;
            if s.len() < orig_len && seen.insert(s.clone()) {
                Some((s.len(), v))
            } else {
                None
            }
        })
        .collect();
    uniq.sort_by_key(|(len, _)| *len);
    uniq.into_iter().take(12).map(|(_, v)| v).collect()
}

fn shrink_counterexamples(
    ctx: &Ctx,
    port: &Port,
    crate_dir: &Path,
    generated: &[Value],
    rep: &fnr_fast::native::NativeFuzzReport,
    float_tolerance: Option<f64>,
) -> Vec<fnr_fast::native::ShrunkFailure> {
    let mut out = Vec::new();
    let shrink_cases = crate_dir.join("shrink-cases.jsonl");
    let shrink_out = crate_dir.join("shrink.json");
    for fail in rep.failures.iter().take(3) {
        let Some(orig_case) = generated.get(fail.case) else {
            continue;
        };
        let orig_args = orig_case["args"].clone();
        let mut cur_args = orig_args.clone();
        let mut cur_expected = fail.expected.clone();
        let mut cur_got = fail.got.clone();
        let mut probes = 0usize;
        for _ in 0..4 {
            let cands = shrink_candidates(&cur_args);
            if cands.is_empty() {
                break;
            }
            let mut batch_vals = Vec::with_capacity(cands.len());
            let mut jsonl = String::new();
            for a in cands {
                let mut obj = json!({ "args": a });
                if let Some(kw) = orig_case.get("kwargs") {
                    obj["kwargs"] = kw.clone();
                }
                jsonl.push_str(&serde_json::to_string(&obj).unwrap_or_default());
                jsonl.push('\n');
                batch_vals.push(obj);
            }
            if fs::write(&shrink_cases, &jsonl).is_err() {
                break;
            }
            probes += batch_vals.len();
            let Ok(sub) =
                port.lang
                    .fuzz_cases(ctx, port, &shrink_cases, &shrink_out, float_tolerance)
            else {
                break;
            };
            let best = sub
                .failures
                .iter()
                .filter_map(|sf| {
                    let cand = batch_vals.get(sf.case)?;
                    let len = serde_json::to_string(&cand["args"]).ok()?.len();
                    Some((len, &cand["args"], &sf.expected, &sf.got))
                })
                .min_by_key(|(len, _, _, _)| *len);
            if let Some((_, smaller_args, exp, got)) = best {
                cur_args = smaller_args.clone();
                cur_expected = exp.clone();
                cur_got = got.clone();
            } else {
                break;
            }
        }
        out.push(fnr_fast::native::ShrunkFailure {
            case: fail.case,
            original_args: orig_args,
            shrunk_args: cur_args,
            expected: cur_expected,
            got: cur_got,
            probes,
        });
    }
    let _ = fs::remove_file(shrink_cases);
    let _ = fs::remove_file(shrink_out);
    out
}

fn append_fuzz_regressions(
    recordings_path: &Path,
    shrunk: &[fnr_fast::native::ShrunkFailure],
    failures: &[fnr_fast::state::Failure],
    generated: &[Value],
) -> Result<usize> {
    let existing = fs::read_to_string(recordings_path).unwrap_or_default();
    let mut seen: std::collections::HashSet<String> = existing
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| serde_json::to_string(&v["args"]).ok())
        .collect();
    let mut added = String::new();
    let mut count = 0usize;
    let items: Vec<(Value, String)> = if !shrunk.is_empty() {
        shrunk
            .iter()
            .map(|s| (s.shrunk_args.clone(), s.expected.clone()))
            .collect()
    } else {
        failures
            .iter()
            .filter_map(|f| {
                generated
                    .get(f.case)
                    .map(|c| (c["args"].clone(), f.expected.clone()))
            })
            .collect()
    };
    for (args, expected) in items {
        let Ok(res_val) = serde_json::from_str::<Value>(&expected) else {
            continue;
        };
        let key = serde_json::to_string(&args)?;
        if seen.insert(key) {
            added.push_str(&serde_json::to_string(&json!({
                "args": args,
                "result": res_val,
                "derived": "fuzz_regression",
            }))?);
            added.push('\n');
            count += 1;
        }
    }
    if count > 0 {
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&added);
        fs::write(recordings_path, content)?;
    }
    Ok(count)
}

/// Property/differential fuzzes a `.fast/ports/<id>` port directly, with counterexample shrinking.
pub fn fuzz_hotspot_port(
    root: &Path,
    id: Option<&str>,
    cases: usize,
    seed: Option<u64>,
    float_tolerance: Option<f64>,
    save_regressions: bool,
) -> Result<HotspotFuzzOutcome> {
    let ctx = Ctx::open(root)?;
    let resolved = ctx.resolve_port(id)?;
    let port = ctx.state.ports[&resolved].clone();
    let crate_dir = ctx.crate_dir(&port);
    let seed = seed.unwrap_or(0x517c_c1b7_2722_0a95);
    let generated = synthesize_fuzz_cases(&crate_dir.join("recordings.jsonl"), cases.max(1), seed)?;
    let cases_path = crate_dir.join("fuzz-cases.jsonl");
    let mut jsonl = String::new();
    for c in &generated {
        jsonl.push_str(&serde_json::to_string(c)?);
        jsonl.push('\n');
    }
    fs::write(&cases_path, jsonl)?;

    match fnr_fast::backend::build(&ctx, &port)? {
        Ok(_) => {}
        Err(log) => anyhow::bail!("building {} for fuzz failed:\n{log}", port.id),
    }

    let out_path = crate_dir.join("fuzz.json");
    let mut rep = port
        .lang
        .fuzz_cases(&ctx, &port, &cases_path, &out_path, float_tolerance)?;
    if rep.mode.is_empty() {
        rep.mode = "crash+determinism".into();
    }
    let shrunk = if !rep.failures.is_empty() {
        shrink_counterexamples(&ctx, &port, &crate_dir, &generated, &rep, float_tolerance)
    } else {
        Vec::new()
    };
    rep.shrunk = shrunk.clone();

    let failures_file = if !rep.failures.is_empty() {
        let p = crate_dir.join("fuzz-failures.jsonl");
        let mut ftxt = String::new();
        for fail in &rep.failures {
            if let Some(sh) = shrunk.iter().find(|s| s.case == fail.case) {
                ftxt.push_str(&serde_json::to_string(&json!({
                    "case": sh.case,
                    "original_args": sh.original_args,
                    "shrunk_args": sh.shrunk_args,
                    "expected": sh.expected,
                    "got": sh.got,
                    "probes": sh.probes,
                }))?);
                ftxt.push('\n');
            } else if let Some(case) = generated.get(fail.case) {
                ftxt.push_str(&serde_json::to_string(&json!({
                    "case": fail.case,
                    "args": case["args"],
                    "expected": fail.expected,
                    "got": fail.got,
                }))?);
                ftxt.push('\n');
            }
        }
        fs::write(&p, ftxt)?;
        Some(p)
    } else {
        None
    };

    let saved_regressions = if save_regressions && !rep.failures.is_empty() {
        append_fuzz_regressions(
            &crate_dir.join("recordings.jsonl"),
            &shrunk,
            &rep.failures,
            &generated,
        )?
    } else {
        0
    };

    Ok(HotspotFuzzOutcome {
        id: resolved,
        mode: rep.mode.clone(),
        seed,
        cases: rep.total,
        passed: rep.passed,
        mismatches: rep.mismatches,
        panics: rep.panics,
        nondeterministic: rep.nondeterministic,
        errors: rep.errors,
        oracle_note: rep.oracle_note.clone(),
        saved_regressions,
        failures_file,
        report: rep,
    })
}

#[derive(Debug, Serialize)]
pub struct HotspotPackageOut {
    pub id: String,
    pub lang: FastLang,
    pub out_dir: PathBuf,
    pub artifact: PathBuf,
    pub files: Vec<String>,
    pub dry_run: bool,
}

fn c_header_for_port(port: &Port, hs: Option<&Hotspot>) -> String {
    let ext = &port.ext;
    let mut direct_decl = String::new();
    if let Some(hs) = hs {
        let flavor = fnr_fast::native::Flavor {
            lang: port.lang,
            fence: "c",
            rust_type: if port.lang == FastLang::Cpp {
                fnr_fast::cfamily::rust_type_cpp
            } else {
                fnr_fast::cfamily::rust_type_c
            },
            default_json: |_| None,
            rules: &[],
        };
        if let Some(sig) = fnr_fast::native::DirectSig::from_hotspot(hs, &flavor) {
            let mut params = Vec::new();
            for dp in &sig.params {
                match dp {
                    fnr_fast::native::DirectParam::Scalar { name, ty } => {
                        params.push(format!("{} {name}", ty.c_ty()));
                    }
                    fnr_fast::native::DirectParam::Str { name } => {
                        params.push(format!("const uint8_t *{name}_ptr"));
                        params.push(format!("size_t {name}_len"));
                    }
                    fnr_fast::native::DirectParam::Slice { name, elem } => {
                        params.push(format!("const {} *{name}_ptr", elem.c_ty()));
                        params.push(format!("size_t {name}_len"));
                    }
                }
            }
            params.push("int32_t *fa_ok".to_string());
            direct_decl = format!("{} fa_direct({});\n", sig.ret.c_ty(), params.join(", "));
        }
    }
    format!(
        "/* Generated by fast-n-rust (`fnr package`) for `{}` */\n\
         #ifndef FNR_{guard}_H\n#define FNR_{guard}_H\n\n\
         #include <stddef.h>\n#include <stdint.h>\n\n\
         #ifdef __cplusplus\nextern \"C\" {{\n#endif\n\n\
         {direct_decl}\
         uint8_t *fa_call(const uint8_t *input, size_t len, size_t *out_len);\n\
         uint8_t *fa_bin(const uint8_t *input, size_t len, size_t *out_len);\n\
         void fa_free(uint8_t *ptr, size_t len);\n\n\
         #ifdef __cplusplus\n}}\n#endif\n\n#endif /* FNR_{guard}_H */\n",
        port.id,
        guard = ext.to_ascii_uppercase(),
    )
}

/// Packages a verified `.fast/ports/<id>` port using the `Ship` LTO profile into `dist/<ext>/`.
pub fn package_hotspot_port(
    root: &Path,
    id: Option<&str>,
    out_dir: Option<&Path>,
    dry_run: bool,
) -> Result<HotspotPackageOut> {
    package_hotspot_port_with(root, id, out_dir, dry_run, false)
}

pub fn package_hotspot_port_with(
    root: &Path,
    id: Option<&str>,
    out_dir: Option<&Path>,
    dry_run: bool,
    native_cpu: bool,
) -> Result<HotspotPackageOut> {
    let ctx = Ctx::open(root)?;
    let resolved = ctx.resolve_port(id)?;
    let port = ctx.state.ports[&resolved].clone();
    anyhow::ensure!(
        port.verify.as_ref().is_some_and(|v| v.ok()),
        "{resolved} must pass `fnr verify` before packaging"
    );

    let (_, lib_name) = port.lang.artifact(&port.ext);
    let dest_dir = out_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ctx.root.join("dist").join(&port.ext));
    let mut files = vec![lib_name.clone(), "manifest.json".to_string()];
    if matches!(port.lang, FastLang::C | FastLang::Cpp) {
        files.push(format!("{}.h", port.ext));
    } else if port.lang == FastLang::Python {
        files.push("__init__.py".to_string());
        files.push("pyproject.toml".to_string());
    }

    if dry_run {
        return Ok(HotspotPackageOut {
            id: resolved,
            lang: port.lang,
            out_dir: dest_dir.clone(),
            artifact: dest_dir.join(&lib_name),
            files,
            dry_run: true,
        });
    }

    let built = match fnr_fast::backend::build_with_options(
        &ctx,
        &port,
        fnr_fast::backend::BuildOptions {
            profile: fnr_fast::backend::BuildProfile::Ship,
            native_cpu,
        },
    )? {
        Ok(p) => p,
        Err(log) => anyhow::bail!("ship build failed for {resolved}:\n{log}"),
    };

    fs::create_dir_all(&dest_dir)?;
    let artifact_dst = dest_dir.join(&lib_name);
    fs::copy(&built, &artifact_dst)?;

    let hs = ctx.state.hotspots.iter().find(|h| h.id == resolved);
    if matches!(port.lang, FastLang::C | FastLang::Cpp) {
        fs::write(
            dest_dir.join(format!("{}.h", port.ext)),
            c_header_for_port(&port, hs),
        )?;
    } else if port.lang == FastLang::Python {
        fs::write(
            dest_dir.join("__init__.py"),
            format!(
                "from .{ext} import {func}\n__all__ = [{func:?}]\n",
                ext = port.ext,
                func = port.func
            ),
        )?;
        fs::write(
            dest_dir.join("pyproject.toml"),
            format!(
                "[project]\nname = \"{ext}\"\nversion = \"0.1.0\"\ndescription = \"Rust acceleration for {id} (fast-n-rust)\"\nrequires-python = \">=3.10\"\n",
                ext = port.ext,
                id = port.id
            ),
        )?;
    }

    let v = port.verify.clone().unwrap_or_default();
    let manifest = json!({
        "tool": "fast-n-rust",
        "port_id": resolved,
        "lang": port.lang,
        "func": port.func,
        "artifact": lib_name,
        "source_hash": v.source_hash,
        "verified_cases": v.passed,
        "direct_abi": v.micro.and_then(|m| m.direct_abi),
        "binary_wire": v.micro.and_then(|m| m.binary_wire),
        "native_cpu": native_cpu,
        "per_call_speedup": v.micro.map(|m| m.speedup),
    });
    fs::write(
        dest_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    Ok(HotspotPackageOut {
        id: resolved,
        lang: port.lang,
        out_dir: dest_dir,
        artifact: artifact_dst,
        files,
        dry_run: false,
    })
}

#[derive(Debug, Clone, Default)]
pub struct CiOptions {
    pub target: Option<String>,
    pub float_tolerance: Option<f64>,
    pub fuzz: usize,
    pub bench: bool,
    pub runs: usize,
    pub min_speedup: Option<f64>,
    pub native_cpu: bool,
}

#[derive(Debug, Serialize)]
pub struct CiPortReport {
    pub id: String,
    pub verified: bool,
    pub gate_passed: bool,
    pub fuzz_passed: Option<bool>,
    pub per_call_speedup: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct CiOutcome {
    pub passed: bool,
    pub ports: Vec<CiPortReport>,
    pub bench: Option<BenchResult>,
    pub failure_reason: Option<String>,
}

/// Runs parity verification, strict gate, optional fuzzing, and optional benchmark + speedup guard.
pub fn run_ci(root: &Path, opts: CiOptions) -> Result<CiOutcome> {
    let ctx = Ctx::open(root)?;
    let ids: Vec<String> = match opts.target.as_deref() {
        Some(t) => vec![ctx.resolve_port(Some(t))?],
        None => ctx.state.ports.keys().cloned().collect(),
    };
    anyhow::ensure!(!ids.is_empty(), "nothing ported yet: run  fnr port");

    let vopts = pipeline::VerifyOptions {
        float_tolerance: opts.float_tolerance,
        known_diffs: None,
        min_speedup: opts.min_speedup,
    };
    let mut reports = Vec::new();
    let mut all_ok = true;
    let mut reason: Option<String> = None;

    for id in &ids {
        let (_, v) = pipeline::verify_with(root, Some(id), &vopts)?;
        let g = gate_hotspot_port(root, Some(id))?;
        let mut fuzz_ok = None;
        if opts.fuzz > 0 {
            let f =
                fuzz_hotspot_port(root, Some(id), opts.fuzz, None, opts.float_tolerance, false)?;
            let ok = f.mismatches == 0 && f.panics == 0 && f.nondeterministic == 0;
            fuzz_ok = Some(ok);
            if !ok && reason.is_none() {
                reason = Some(format!(
                    "{id}: fuzz found {} mismatch(es), {} panic(s), {} nondeterministic case(s)",
                    f.mismatches, f.panics, f.nondeterministic
                ));
            }
        }
        let speed_ok = !v.below_min_speedup(opts.min_speedup);
        let ok = v.ok() && g.passed && fuzz_ok.unwrap_or(true) && speed_ok;
        if !ok {
            all_ok = false;
            if reason.is_none() {
                if !v.ok() {
                    reason = Some(format!(
                        "{id}: verify failed ({}/{} passed)",
                        v.passed, v.total
                    ));
                } else if !g.passed {
                    reason = Some(format!("{id}: strict gate failed"));
                } else if !speed_ok {
                    reason = Some(format!(
                        "{id}: per-call speedup {:.2}x below --min-speedup {:.2}x",
                        v.micro.map_or(0.0, |m| m.speedup),
                        opts.min_speedup.unwrap_or(0.0)
                    ));
                }
            }
        }
        reports.push(CiPortReport {
            id: id.clone(),
            verified: v.ok(),
            gate_passed: g.passed,
            fuzz_passed: fuzz_ok,
            per_call_speedup: v.micro.map(|m| m.speedup),
        });
    }

    let bench_res = if opts.bench && all_ok {
        if opts.native_cpu {
            let ctx = Ctx::open(root)?;
            for id in &ids {
                if let Some(p) = ctx.state.ports.get(id)
                    && p.enabled
                {
                    let _ = fnr_fast::backend::build_with_options(
                        &ctx,
                        p,
                        fnr_fast::backend::BuildOptions {
                            profile: fnr_fast::backend::BuildProfile::Ship,
                            native_cpu: true,
                        },
                    )?;
                }
            }
        }
        let b = pipeline::bench(root, opts.runs.max(1))?;
        if let Some(min) = opts.min_speedup
            && b.speedup < min
        {
            all_ok = false;
            reason = Some(format!(
                "end-to-end speedup {:.2}x is below --min-speedup {:.2}x",
                b.speedup, min
            ));
        }
        if b.output_diff.is_some() {
            all_ok = false;
            reason = Some(format!(
                "bench output diff: {}",
                b.output_diff.as_deref().unwrap_or("")
            ));
        }
        Some(b)
    } else {
        None
    };

    Ok(CiOutcome {
        passed: all_ok,
        ports: reports,
        bench: bench_res,
        failure_reason: reason,
    })
}

#[derive(Debug, Serialize)]
pub struct PortStatusRow {
    pub id: String,
    pub lang: FastLang,
    pub enabled: bool,
    pub transport: String,
    pub build_profile: Option<String>,
    pub verified: bool,
    pub passed: usize,
    pub total: usize,
    pub per_call_speedup: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct UnifiedStatus {
    pub has_fast: bool,
    pub app_lang: FastLang,
    pub wall_secs: f64,
    pub hotspots: Vec<PortStatusRow>,
    pub units: Option<fnr_migrate::port_map::PortMap>,
    pub bench: Option<BenchResult>,
}

/// Collects both `.fast/state.json` hotspot ports (transport tier, build profile, verify) and `rust/` unit crates.
pub fn unified_status(root: &Path) -> Result<UnifiedStatus> {
    let has_fast = root.join(".fast/state.json").is_file();
    let (app_lang, wall_secs, bench, hotspots) = if has_fast {
        let ctx = Ctx::open(root)?;
        let rows = ctx
            .state
            .ports
            .values()
            .map(|p| {
                let v = p.verify.as_ref();
                let transport = match p.lang {
                    FastLang::Python => "PyO3 native".to_string(),
                    FastLang::Node => "napi-rs native".to_string(),
                    _ => match v.and_then(|vr| vr.micro) {
                        Some(m) if m.direct_abi == Some(true) => "direct C ABI".to_string(),
                        Some(m) if m.binary_wire == Some(true) => "binary wire".to_string(),
                        Some(_) => "JSON FFI".to_string(),
                        None => {
                            let hs = ctx.state.hotspots.iter().find(|h| h.id == p.id);
                            if hs.is_some_and(Hotspot::supports_direct_abi) {
                                "direct C ABI".to_string()
                            } else if hs.is_some_and(Hotspot::supports_binary_wire) {
                                "binary wire".to_string()
                            } else {
                                "JSON FFI".to_string()
                            }
                        }
                    },
                };
                PortStatusRow {
                    id: p.id.clone(),
                    lang: p.lang,
                    enabled: p.enabled,
                    transport,
                    build_profile: p.build_profile.clone(),
                    verified: v.is_some_and(fnr_fast::state::Verify::ok),
                    passed: v.map_or(0, |vr| vr.passed),
                    total: v.map_or(0, |vr| vr.total),
                    per_call_speedup: v.and_then(|vr| vr.micro).map(|m| m.speedup),
                }
            })
            .collect();
        (
            ctx.state.lang,
            ctx.state.wall,
            ctx.state.bench.clone(),
            rows,
        )
    } else {
        (FastLang::default(), 0.0, None, Vec::new())
    };
    let units = fnr_migrate::port_map::map(root, &[]).ok();
    Ok(UnifiedStatus {
        has_fast,
        app_lang,
        wall_secs,
        hotspots,
        units,
        bench,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fnr_fast::hotspot::Hotspot;
    use fnr_fast::pipeline::ScanOut;

    fn sample_hs(id: &str, share: f64, ncalls: u64, tottime: f64) -> Hotspot {
        Hotspot {
            id: id.into(),
            file: "app.py".into(),
            name: id.rsplit('.').next().unwrap_or(id).into(),
            share,
            portability: 1.0,
            ncalls,
            tottime,
            cumtime: tottime,
            ..Hotspot::default()
        }
    }

    #[test]
    fn mode_advisor_classifies_fast_batch_and_whole_app() {
        let fast_scan = ScanOut {
            command: vec!["python3".into(), "app.py".into()],
            wall: 2.0,
            hotspots: vec![
                sample_hs("app.heavy", 0.89, 10, 1.44),
                sample_hs("app.main_loop", 0.92, 1, 1.84),
            ],
        };
        let adv = advise_mode(&fast_scan);
        assert_eq!(adv.mode, RecommendedMode::FastHotspot);
        assert!(adv.top_portable_share <= 1.0);
        assert!(adv.headline.contains("92.0%"), "{}", adv.headline);

        let batch_scan = ScanOut {
            command: vec!["python3".into(), "app.py".into()],
            wall: 2.0,
            hotspots: vec![sample_hs("app.tiny", 0.60, 50_000, 0.5)],
        };
        assert_eq!(advise_mode(&batch_scan).mode, RecommendedMode::BatchHotLoop);

        let flat_scan = ScanOut {
            command: vec!["python3".into(), "app.py".into()],
            wall: 2.0,
            hotspots: vec![sample_hs("app.small", 0.10, 5, 0.2)],
        };
        assert_eq!(advise_mode(&flat_scan).mode, RecommendedMode::WholeApp);
    }

    #[test]
    fn enrich_task_md_appends_risks_and_crates_idempotently() {
        let root = std::env::temp_dir().join(format!("fnr-enrich-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let crate_dir = root.join(".fast/ports/app.parse");
        fs::create_dir_all(&crate_dir).unwrap();
        fs::write(
            root.join("app.py"),
            "import re, yaml\ndef parse(s):\n    return yaml.safe_load(s)\n",
        )
        .unwrap();
        fs::write(crate_dir.join("TASK.md"), "# Task\nPort parse\n").unwrap();

        let hs = Hotspot {
            id: "app.parse".into(),
            file: "app.py".into(),
            name: "parse".into(),
            source: "def parse(s):\n    return re.findall(r'\\w+', s)".into(),
            ..Hotspot::default()
        };

        assert!(enrich_task_md(&root, &crate_dir, &hs, FastLang::Python).unwrap());
        assert!(!enrich_task_md(&root, &crate_dir, &hs, FastLang::Python).unwrap());

        let md = fs::read_to_string(crate_dir.join("TASK.md")).unwrap();
        assert!(md.contains("Static Risks, Performance Blueprint & Gate Checklist"));
        assert!(md.contains("Regex semantics"));
        assert!(md.contains("serde_yaml_ng"), "{md}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_target_distinguishes_migrate_crate_and_fast_port() {
        let root = std::env::temp_dir().join(format!("fnr-detect-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let crate_dir = root.join("rust/slug-rs");
        fs::create_dir_all(&crate_dir).unwrap();
        fs::write(crate_dir.join(MANIFEST), "{}").unwrap();

        assert_eq!(
            detect_target(&root, Some("rust/slug-rs")),
            TargetKind::MigrateCrate(root.join("rust/slug-rs"))
        );
        assert_eq!(
            detect_target(&root, Some("slug-rs")),
            TargetKind::MigrateCrate(root.join("rust/slug-rs"))
        );
        assert_eq!(
            detect_target(&root, Some("app.dedupe")),
            TargetKind::FastPort(Some("app.dedupe".into()))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn convert_calls_jsonl_preserves_args_and_kwargs() {
        let root = std::env::temp_dir().join(format!("fnr-conv-calls-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let rec = root.join("recordings.jsonl");
        let out = root.join("recorded-inputs.jsonl");
        fs::write(
            &rec,
            "{\"args\":[1,2],\"kwargs\":{\"step\":3},\"ok\":4}\n{\"args\":[\"hi\"],\"kwargs\":{},\"ok\":\"HI\"}\n",
        )
        .unwrap();

        let n = convert_calls_jsonl(&rec, "calc", &out).unwrap();
        assert_eq!(n, 2);
        let txt = fs::read_to_string(&out).unwrap();
        let rows: Vec<Value> = txt
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            rows[0],
            json!({ "fn": "calc", "args": [1, 2], "kwargs": { "step": 3 } })
        );
        assert_eq!(rows[1], json!({ "fn": "calc", "args": ["hi"] }));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gate_hotspot_port_catches_stubs_and_unexplained_unsafe_and_passes_clean() {
        let root = std::env::temp_dir().join(format!("fnr-gate-hs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let crate_dir = root.join(".fast/ports/app.f");
        fs::create_dir_all(crate_dir.join("src")).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"fa_app_f\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[lib]\ncrate-type = [\"cdylib\", \"rlib\"]\n",
        )
        .unwrap();
        fs::write(
            crate_dir.join("src/lib.rs"),
            "pub fn f(x: i64) -> i64 {\n    unsafe { x + 1 }\n}\npub fn stub() {\n    todo!()\n}\n",
        )
        .unwrap();
        let hash = fnr_fast::backend::source_hash(&crate_dir).unwrap();
        let mut ctx = Ctx::open(&root).unwrap();
        ctx.state.ports.insert(
            "app.f".into(),
            Port {
                id: "app.f".into(),
                file: "app.c".into(),
                module: "app".into(),
                func: "f".into(),
                ext: "fa_app_f".into(),
                crate_dir: ".fast/ports/app.f".into(),
                recorded: 1,
                calls_seen: 1,
                verify: Some(fnr_fast::state::Verify {
                    built: true,
                    total: 1,
                    passed: 1,
                    source_hash: Some(hash),
                    ..Default::default()
                }),
                enabled: false,
                lang: FastLang::C,
                owner: None,
                line: 1,
                sources: vec![],
                build_profile: None,
            },
        );
        ctx.save().unwrap();

        let bad = gate_hotspot_port(&root, Some("app.f")).unwrap();
        assert!(!bad.passed);
        assert!(
            !bad.checks
                .iter()
                .find(|c| c.name == "ported")
                .unwrap()
                .passed
        );
        assert!(
            !bad.checks
                .iter()
                .find(|c| c.name == "unsafe")
                .unwrap()
                .passed
        );

        fs::write(
            crate_dir.join("src/lib.rs"),
            "pub fn f(x: i64) -> i64 {\n    x + 1\n}\n",
        )
        .unwrap();
        let new_hash = fnr_fast::backend::source_hash(&crate_dir).unwrap();
        let mut ctx2 = Ctx::open(&root).unwrap();
        ctx2.state
            .ports
            .get_mut("app.f")
            .unwrap()
            .verify
            .as_mut()
            .unwrap()
            .source_hash = Some(new_hash);
        ctx2.save().unwrap();

        let good = gate_hotspot_port(&root, Some("app.f")).unwrap();
        assert!(good.passed, "{:?}", good.checks);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn synthesize_fuzz_cases_and_merge_deps_and_package_dry_run() {
        let root = std::env::temp_dir().join(format!("fnr-synergy-extra-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let fast_crate = root.join(".fast/ports/app_f");
        fs::create_dir_all(fast_crate.join("src")).unwrap();
        fs::write(
            fast_crate.join("recordings.jsonl"),
            "{\"args\":[10, \"abc\"],\"result\":42}\n",
        )
        .unwrap();
        let cases = synthesize_fuzz_cases(&fast_crate.join("recordings.jsonl"), 12, 123).unwrap();
        assert_eq!(cases.len(), 12);
        assert!(cases[0]["args"].is_array());
        for c in &cases {
            let n = c["args"][0].as_i64().unwrap();
            assert!(n.abs() <= 32, "synthesized integer exploded: {n}");
        }

        let src_toml = fast_crate.join("Cargo.toml");
        let dst_toml = root.join("dst_Cargo.toml");
        fs::write(
            &src_toml,
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\nrayon = \"1.10\"\n",
        )
        .unwrap();
        fs::write(
            &dst_toml,
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
        )
        .unwrap();
        merge_extra_dependencies(&src_toml, &dst_toml).unwrap();
        let merged = fs::read_to_string(&dst_toml).unwrap();
        assert!(merged.contains("rayon = \"1.10\""), "{merged}");

        let mut ctx = Ctx::open(&root).unwrap();
        ctx.state.ports.insert(
            "app.f".into(),
            Port {
                id: "app.f".into(),
                file: "app.c".into(),
                module: "app".into(),
                func: "f".into(),
                ext: "fa_f".into(),
                crate_dir: ".fast/ports/app_f".into(),
                recorded: 1,
                calls_seen: 1,
                verify: Some(fnr_fast::state::Verify {
                    built: true,
                    total: 1,
                    passed: 1,
                    micro: Some(fnr_fast::state::Micro {
                        original_ns: 100.0,
                        rust_ns: 10.0,
                        ffi_ns: Some(1.0),
                        speedup: 10.0,
                        direct_abi: Some(true),
                        binary_wire: None,
                    }),
                    ..Default::default()
                }),
                enabled: false,
                lang: FastLang::C,
                owner: None,
                line: 1,
                sources: vec![],
                build_profile: None,
            },
        );
        ctx.save().unwrap();
        let pkg = package_hotspot_port(&root, Some("app.f"), None, true).unwrap();
        assert!(pkg.dry_run);
        assert!(pkg.files.contains(&"fa_f.h".to_string()));
        let status = unified_status(&root).unwrap();
        assert_eq!(status.hotspots.len(), 1);
        assert_eq!(status.hotspots[0].transport, "direct C ABI");
        let shrunk = shrink_candidates(&json!([[1, 2, 3, 4], "hello", 100]));
        assert!(!shrunk.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn enrich_task_md_for_r_recommends_r_crates() {
        let root = std::env::temp_dir().join(format!("fnr-r-crates-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let crate_dir = root.join(".fast/ports/app_calc");
        fs::create_dir_all(&crate_dir).unwrap();
        fs::write(
            root.join("app.R"),
            "library(dplyr)\nlibrary(stats)\ncalc <- function(x) sum(x)\n",
        )
        .unwrap();
        fs::write(crate_dir.join("TASK.md"), "# Task\n").unwrap();

        let hs = Hotspot {
            id: "app.calc".into(),
            module: Some("app".into()),
            name: "calc".into(),
            file: "app.R".into(),
            line: 3,
            ncalls: 10,
            tottime: 0.5,
            cumtime: 0.5,
            share: 0.9,
            ..Default::default()
        };

        assert!(enrich_task_md(&root, &crate_dir, &hs, FastLang::R).unwrap());
        let task = fs::read_to_string(crate_dir.join("TASK.md")).unwrap();
        assert!(task.contains("polars"), "expected R crate polars: {task}");
        assert!(
            task.contains("nalgebra"),
            "expected R crate nalgebra: {task}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn escalate_disables_active_patch_and_auto_splices_c_port() {
        let root = std::env::temp_dir().join(format!("fnr-esc-c-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".fast/ports/calc_sum_sq")).unwrap();
        let clean_c = "int sum_sq(int n) {\n    int s = 0;\n    for (int i = 0; i < n; i++) s += i * i;\n    return s;\n}\n";
        let patched_c = format!(
            "{clean_c}\n// >>> fnr: calc.sum_sq >>>\n// mocked inline patch\n// <<< fnr: calc.sum_sq <<<\n"
        );
        fs::write(root.join("calc.c"), &patched_c).unwrap();
        fs::write(
            root.join(".fast/ports/calc_sum_sq/Cargo.toml"),
            "[package]\nname = \"fa_sum_sq\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::create_dir_all(root.join(".fast/ports/calc_sum_sq/src")).unwrap();
        fs::write(
            root.join(".fast/ports/calc_sum_sq/src/port.rs"),
            "fn helper(v: i32) -> i32 { v * v }\npub fn sum_sq(n: i32) -> Result<i32, String> {\n    Ok((0..n).map(helper).sum())\n}\n",
        )
        .unwrap();

        let mut ctx = Ctx::open(&root).unwrap_or_else(|_| {
            let mut c = Ctx {
                root: root.clone(),
                fast: root.join(".fast"),
                state: fnr_fast::state::State::default(),
            };
            c.state.lang = FastLang::C;
            c
        });
        ctx.state.lang = FastLang::C;
        ctx.state.ports.insert(
            "calc.sum_sq".into(),
            Port {
                id: "calc.sum_sq".into(),
                file: "calc.c".into(),
                module: "calc".into(),
                func: "sum_sq".into(),
                ext: "fa_sum_sq".into(),
                crate_dir: ".fast/ports/calc_sum_sq".into(),
                recorded: 1,
                calls_seen: 1,
                verify: None,
                enabled: true,
                lang: FastLang::C,
                owner: None,
                line: 1,
                sources: vec![],
                build_profile: None,
            },
        );
        ctx.save().unwrap();

        let out = escalate_hotspot(&root, Some("calc.sum_sq"), None).unwrap();
        assert!(out.disabled_active_patch);
        assert!(out.auto_ported);
        let c_after = fs::read_to_string(root.join("calc.c")).unwrap();
        assert!(
            !c_after.contains(">>> fnr:"),
            "inline patch should be stripped: {c_after}"
        );
        let port_rs = fs::read_to_string(out.migrate_crate.join("src/port.rs")).unwrap();
        assert!(port_rs.contains("__fnr_hotspot_impl::sum_sq"), "{port_rs}");
        let code_body = port_rs.split("#![forbid(unsafe_code)]").nth(1).unwrap();
        assert!(!code_body.contains("unimplemented!"), "{port_rs}");

        let _ = fs::remove_dir_all(&root);
    }
}
