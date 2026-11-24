//! Unified `doctor`: checks Rust toolchain, all 14 source language runtimes,
//! AI coding agents, MCP registration, and build cache usage.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use fnr_migrate::doctor::{self as mdoc, Status, Version};
use serde::Serialize;

use crate::clean::{dir_size, human_bytes};
use crate::setup::{self, Agent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectStack {
    Python,
    Node,
    Go,
    Java,
    Kotlin,
    Ruby,
    Php,
    C,
    Cpp,
    CSharp,
    R,
    PowerShell,
    Wasm,
    Android,
}

impl ProjectStack {
    pub fn label(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Node => "node",
            Self::Go => "go",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::C => "c",
            Self::Cpp => "c++",
            Self::CSharp => "c#",
            Self::R => "r",
            Self::PowerShell => "powershell",
            Self::Wasm => "wasm",
            Self::Android => "android",
        }
    }
}

fn classify_entry(path: &Path, stacks: &mut BTreeSet<ProjectStack>) {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    match name {
        "pyproject.toml" | "requirements.txt" | "setup.py" | "Pipfile" | "uv.lock"
        | "poetry.lock" => {
            stacks.insert(ProjectStack::Python);
        }
        "package.json" | "tsconfig.json" | "deno.json" | "deno.jsonc" | "bun.lock"
        | "bun.lockb" => {
            stacks.insert(ProjectStack::Node);
        }
        "go.mod" => {
            stacks.insert(ProjectStack::Go);
        }
        "pom.xml" | "build.gradle" => {
            stacks.insert(ProjectStack::Java);
        }
        "build.gradle.kts" => {
            stacks.insert(ProjectStack::Kotlin);
        }
        "Gemfile" => {
            stacks.insert(ProjectStack::Ruby);
        }
        "composer.json" => {
            stacks.insert(ProjectStack::Php);
        }
        "CMakeLists.txt" | "compile_commands.json" => {
            stacks.insert(ProjectStack::C);
        }
        "DESCRIPTION" => {
            stacks.insert(ProjectStack::R);
        }
        "AndroidManifest.xml" | "gradlew" => {
            stacks.insert(ProjectStack::Android);
        }
        "wasm-pack.toml" => {
            stacks.insert(ProjectStack::Wasm);
        }
        "Cargo.toml" => {
            if let Ok(txt) = std::fs::read_to_string(path)
                && txt.contains("wasm-bindgen")
            {
                stacks.insert(ProjectStack::Wasm);
            }
        }
        _ => {}
    }

    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => {
            stacks.insert(ProjectStack::Python);
        }
        Some("js" | "ts" | "mjs" | "cjs" | "tsx" | "jsx") => {
            stacks.insert(ProjectStack::Node);
        }
        Some("go") => {
            stacks.insert(ProjectStack::Go);
        }
        Some("java") => {
            stacks.insert(ProjectStack::Java);
        }
        Some("kt" | "kts") => {
            stacks.insert(ProjectStack::Kotlin);
        }
        Some("csproj" | "sln" | "cs") => {
            stacks.insert(ProjectStack::CSharp);
        }
        Some("rb") => {
            stacks.insert(ProjectStack::Ruby);
        }
        Some("php") => {
            stacks.insert(ProjectStack::Php);
        }
        Some("c") => {
            stacks.insert(ProjectStack::C);
        }
        Some("cpp" | "cc" | "cxx" | "hpp") => {
            stacks.insert(ProjectStack::Cpp);
        }
        Some("R") => {
            stacks.insert(ProjectStack::R);
        }
        Some("ps1") => {
            stacks.insert(ProjectStack::PowerShell);
        }
        _ => {}
    }
}

pub fn detect_project_stacks(root: &Path) -> Vec<ProjectStack> {
    const SKIP_DIRS: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        ".venv",
        "venv",
        ".fast",
        "dist",
        "build",
    ];
    let mut stacks = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            classify_entry(&path, &mut stacks);
        } else if path.is_dir() {
            let dir_name = entry.file_name();
            let dir_str = dir_name.to_string_lossy();
            if SKIP_DIRS.contains(&dir_str.as_ref()) || dir_str.starts_with('.') {
                continue;
            }
            if let Ok(sub_entries) = std::fs::read_dir(&path) {
                for sub in sub_entries.flatten() {
                    let sub_path = sub.path();
                    if sub_path.is_file() {
                        classify_entry(&sub_path, &mut stacks);
                    }
                }
            }
        }
    }
    stacks.into_iter().collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheInfo {
    pub shared_dir: PathBuf,
    pub shared_bytes: u64,
    pub shared_human: String,
    pub local_fast_target: PathBuf,
    pub local_fast_bytes: u64,
    pub local_fast_human: String,
}

#[derive(Debug, Serialize)]
pub struct UnifiedDoctorReport {
    pub ok: bool,
    pub show_all: bool,
    pub detected_stacks: Vec<ProjectStack>,
    pub checks: Vec<mdoc::Check>,
    pub extra_runtimes: Vec<mdoc::Check>,
    pub agents: Vec<mdoc::Check>,
    pub cache: CacheInfo,
}

fn probe(program: &str, args: &[&str]) -> Option<String> {
    let run = |p: &str| {
        Command::new(p)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    let out = run(program).or_else(|| {
        if cfg!(windows) {
            run(&format!("{program}.cmd"))
        } else {
            None
        }
    })?;
    let text = if out.stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    Some(text.lines().next().unwrap_or("").trim().to_string())
}

fn runtime_check(
    name: &str,
    program: &str,
    args: &[&str],
    min: Option<Version>,
    fix: &str,
) -> mdoc::Check {
    match probe(program, args) {
        None => mdoc::Check {
            name: name.to_string(),
            status: Status::Missing,
            required: false,
            detail: "not installed".to_string(),
            fix: Some(fix.to_string()),
        },
        Some(line) => {
            let ver = mdoc::parse_version(&line);
            let status = mdoc::grade(true, ver, min);
            let fix_msg = (status != Status::Ok).then(|| fix.to_string());
            mdoc::Check {
                name: name.to_string(),
                status,
                required: false,
                detail: line,
                fix: fix_msg,
            }
        }
    }
}

fn agent_check(name: &str, bin: &str, fix: &str) -> mdoc::Check {
    match probe(bin, &["--version"]) {
        Some(line) => mdoc::Check {
            name: name.to_string(),
            status: Status::Ok,
            required: false,
            detail: line,
            fix: None,
        },
        None => mdoc::Check {
            name: name.to_string(),
            status: Status::Missing,
            required: false,
            detail: "not on PATH".to_string(),
            fix: Some(fix.to_string()),
        },
    }
}

fn check_fnr_setup(base_checks: &mut Vec<mdoc::Check>) {
    let Ok(home) = setup::home() else { return };
    for agent in Agent::ALL {
        let st = setup::agent_detail(&home, agent);
        if !st.detected && !st.mcp_configured {
            continue;
        }
        let broken_cmd = st.mcp_command.as_deref().and_then(|cmd| {
            let p = std::path::Path::new(cmd);
            (p.is_absolute() && !p.is_file()).then_some(cmd)
        });
        let (status, detail, ok) = if let Some(missing_cmd) = broken_cmd {
            (
                Status::Missing,
                format!("MCP command not found: {missing_cmd}"),
                false,
            )
        } else if !st.mcp_configured {
            (
                Status::Missing,
                "agent detected, fast-n-rust not registered yet".to_string(),
                false,
            )
        } else if st.skill_configured == Some(false) {
            (
                Status::Missing,
                "fast-n-rust MCP configured, skills missing".to_string(),
                false,
            )
        } else if st.skill_configured.is_none() {
            (Status::Ok, "fast-n-rust MCP configured".to_string(), true)
        } else {
            (
                Status::Ok,
                "fast-n-rust MCP & skills configured".to_string(),
                true,
            )
        };
        base_checks.push(mdoc::Check {
            name: format!("fnr setup ({})", agent.name()),
            status,
            required: false,
            detail,
            fix: (!ok).then(|| "fast-n-rust setup".to_string()),
        });
    }
}

fn check_needed_in_scope(name: &str, stacks: &[ProjectStack]) -> bool {
    if name == "cargo" || name == "cc" || name == "mcp server" || name.starts_with("fnr setup (") {
        return true;
    }
    let has = |s: ProjectStack| stacks.contains(&s);
    match name {
        "python3" => has(ProjectStack::Python),
        "node" => has(ProjectStack::Node),
        "go" => has(ProjectStack::Go),
        "ruby" => has(ProjectStack::Ruby),
        "java" | "javac" => has(ProjectStack::Java) || has(ProjectStack::Kotlin),
        "kotlinc" => has(ProjectStack::Kotlin),
        "php" => has(ProjectStack::Php),
        "dotnet" => has(ProjectStack::CSharp),
        "pwsh" => has(ProjectStack::PowerShell),
        "wasm-bindgen" | "wasm32-unknown-unknown" => has(ProjectStack::Wasm),
        "Android NDK" | "Android Rust targets" => has(ProjectStack::Android),
        _ => false,
    }
}

pub fn run(project_root: &Path) -> UnifiedDoctorReport {
    run_with_options(project_root, false)
}

pub fn run_with_options(project_root: &Path, show_all: bool) -> UnifiedDoctorReport {
    let mut base = mdoc::run();
    // Keep only rust-anything agent-setup checks out if we replace them with fnr setup checks,
    // while retaining toolchain/runtime checks from migrate-engine's rich probe suite.
    base.checks.retain(|c| {
        !c.name.starts_with("agent ") && !c.name.ends_with(" skill") && !c.name.ends_with(" mcp")
    });
    for c in &mut base.checks {
        if c.name == "cargo" && c.status != Status::Ok {
            c.fix = Some(if cfg!(windows) {
                "winget install Rustlang.Rustup".to_string()
            } else {
                "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh".to_string()
            });
        }
    }
    check_fnr_setup(&mut base.checks);

    let detected_stacks = detect_project_stacks(project_root);
    let active_stacks: Vec<ProjectStack> = if detected_stacks.is_empty() {
        vec![ProjectStack::Python, ProjectStack::Node]
    } else {
        detected_stacks.clone()
    };

    if !show_all {
        base.checks
            .retain(|c| check_needed_in_scope(&c.name, &active_stacks));
    }

    let mut extra_runtimes = vec![
        runtime_check(
            "c++ (clang++/g++)",
            "clang++",
            &["--version"],
            None,
            "install clang++ or g++",
        )
        .into_fallback("g++", &["--version"]),
        runtime_check(
            "r (Rscript)",
            "Rscript",
            &["--version"],
            Some(Version(4, 0, 0)),
            "install R 4.0+",
        ),
        runtime_check(
            "bun",
            "bun",
            &["--version"],
            Some(Version(1, 0, 0)),
            "https://bun.sh",
        ),
        runtime_check(
            "deno",
            "deno",
            &["--version"],
            Some(Version(2, 0, 0)),
            "https://deno.com",
        ),
        runtime_check(
            "uv (python)",
            "uv",
            &["--version"],
            None,
            "https://docs.astral.sh/uv/",
        ),
        runtime_check(
            "poetry (python)",
            "poetry",
            &["--version"],
            None,
            "https://python-poetry.org",
        ),
    ];

    if !show_all {
        extra_runtimes.retain(|c| match c.name.as_str() {
            "uv (python)" | "poetry (python)" => active_stacks.contains(&ProjectStack::Python),
            "bun" | "deno" => active_stacks.contains(&ProjectStack::Node),
            "c++ (clang++/g++)" => {
                active_stacks.contains(&ProjectStack::C)
                    || active_stacks.contains(&ProjectStack::Cpp)
            }
            "r (Rscript)" => active_stacks.contains(&ProjectStack::R),
            _ => false,
        });
    }

    let mut agents = vec![
        agent_check("claude", "claude", "npm i -g @anthropic-ai/claude-code"),
        agent_check("codex", "codex", "npm i -g @openai/codex"),
        agent_check("gemini", "gemini", "npm i -g @google/gemini-cli"),
        agent_check("opencode", "opencode", "https://opencode.ai"),
        agent_check("aider", "aider", "pip install aider-chat"),
    ];

    if !show_all
        && base
            .checks
            .iter()
            .any(|c| c.name.starts_with("fnr setup ("))
    {
        agents.retain(|a| a.status == Status::Ok);
    }

    let shared_dir = fnr_migrate::cache::shared_dir();
    let shared_bytes = dir_size(&shared_dir);
    let local_fast_target = project_root.join(".fast").join("target");
    let local_fast_bytes = dir_size(&local_fast_target);

    let ok = base.ok();
    UnifiedDoctorReport {
        ok,
        show_all,
        detected_stacks,
        checks: base.checks,
        extra_runtimes,
        agents,
        cache: CacheInfo {
            shared_dir,
            shared_bytes,
            shared_human: human_bytes(shared_bytes),
            local_fast_target,
            local_fast_bytes,
            local_fast_human: human_bytes(local_fast_bytes),
        },
    }
}

trait FallbackCheck {
    fn into_fallback(self, alt: &str, args: &[&str]) -> Self;
}

impl FallbackCheck for mdoc::Check {
    fn into_fallback(self, alt: &str, args: &[&str]) -> Self {
        if self.status != Status::Missing {
            return self;
        }
        if let Some(line) = probe(alt, args) {
            Self {
                name: self.name,
                status: Status::Ok,
                required: self.required,
                detail: line,
                fix: None,
            }
        } else {
            self
        }
    }
}

fn fmt_status(s: Status) -> &'static str {
    match s {
        Status::Ok => "ok     ",
        Status::Missing => "missing",
        Status::Outdated => "old    ",
        Status::Foreign => "foreign",
    }
}

pub fn text(r: &UnifiedDoctorReport) -> String {
    let mut out = if r.show_all {
        String::from("Toolchains & Core (14 languages):\n")
    } else if r.detected_stacks.is_empty() {
        String::from(
            "Environment Check (default scope: python, node — pass `fnr doctor --all` for all 14 languages):\n",
        )
    } else {
        let scope = r
            .detected_stacks
            .iter()
            .map(|s| s.label())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Environment Check (project scope: {scope} — pass `fnr doctor --all` for all 14 languages):\n"
        )
    };
    for c in &r.checks {
        out.push_str(&format!(
            "  {}  {:<26} {}\n",
            fmt_status(c.status),
            c.name,
            c.detail
        ));
        if let Some(fix) = &c.fix {
            out.push_str(&format!("           fix: {fix}\n"));
        }
    }
    if !r.extra_runtimes.is_empty() {
        out.push_str(if r.show_all {
            "\nAdditional Runtimes (C++, R, Bun, Deno, Env Runners):\n"
        } else {
            "\nAdditional Runtimes:\n"
        });
        for c in &r.extra_runtimes {
            out.push_str(&format!(
                "  {}  {:<26} {}\n",
                fmt_status(c.status),
                c.name,
                c.detail
            ));
            if let Some(fix) = &c.fix {
                out.push_str(&format!("           fix: {fix}\n"));
            }
        }
    }
    if !r.agents.is_empty() {
        out.push_str("\nAI Coding Agents:\n");
        for c in &r.agents {
            out.push_str(&format!(
                "  {}  {:<26} {}\n",
                fmt_status(c.status),
                c.name,
                c.detail
            ));
            if let Some(fix) = &c.fix {
                out.push_str(&format!("           fix: {fix}\n"));
            }
        }
    }
    out.push_str(&format!(
        "\nBuild Caches:\n  shared cache : {} ({})\n  local .fast  : {} ({})\n",
        r.cache.shared_dir.display(),
        r.cache.shared_human,
        r.cache.local_fast_target.display(),
        r.cache.local_fast_human
    ));
    if r.cache.shared_bytes + r.cache.local_fast_bytes > 1_073_741_824 {
        out.push_str(
            "  tip: cache exceeds 1 GB — reclaim space anytime with `fnr clean --cache`\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_doctor_runs_and_populates_cache_info() {
        let dir = std::env::temp_dir().join(format!("fnr-doc-all-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rep = run_with_options(&dir, true);
        assert!(rep.show_all);
        assert!(!rep.checks.is_empty());
        assert_eq!(rep.agents.len(), 5);
        assert_eq!(rep.extra_runtimes.len(), 6);
        let rendered = text(&rep);
        assert!(rendered.contains("Toolchains & Core (14 languages):"));
        assert!(rendered.contains("Build Caches:"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scoped_doctor_filters_to_python_or_node_stack() {
        let base = std::env::temp_dir().join(format!("fnr-doc-scope-{}", std::process::id()));
        let py_dir = base.join("py");
        let node_dir = base.join("node");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&py_dir).unwrap();
        std::fs::create_dir_all(&node_dir).unwrap();
        std::fs::write(py_dir.join("pyproject.toml"), "[project]\nname = \"a\"\n").unwrap();
        std::fs::write(node_dir.join("package.json"), "{}\n").unwrap();

        let py_rep = run(&py_dir);
        assert!(!py_rep.show_all);
        assert_eq!(py_rep.detected_stacks, vec![ProjectStack::Python]);
        let py_names: Vec<&str> = py_rep.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(py_names.contains(&"cargo"));
        assert!(py_names.contains(&"cc"));
        assert!(py_names.contains(&"python3"));
        assert!(!py_names.contains(&"go"));
        assert!(!py_names.contains(&"ruby"));
        assert!(!py_names.contains(&"pwsh"));
        assert!(!py_names.contains(&"Android NDK"));
        let py_extra: Vec<&str> = py_rep
            .extra_runtimes
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(py_extra, vec!["uv (python)", "poetry (python)"]);
        assert!(text(&py_rep).contains("project scope: python"));

        let node_rep = run(&node_dir);
        assert_eq!(node_rep.detected_stacks, vec![ProjectStack::Node]);
        let node_names: Vec<&str> = node_rep.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(node_names.contains(&"cargo"));
        assert!(node_names.contains(&"cc"));
        assert!(node_names.contains(&"node"));
        assert!(!node_names.contains(&"python3"));
        assert!(!node_names.contains(&"go"));
        assert!(!node_names.contains(&"Android NDK"));
        let node_extra: Vec<&str> = node_rep
            .extra_runtimes
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(node_extra, vec!["bun", "deno"]);
        assert!(text(&node_rep).contains("project scope: node"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
