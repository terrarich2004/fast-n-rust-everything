//! Unified `doctor`: checks Rust toolchain, all 14 source language runtimes,
//! AI coding agents, MCP registration, and build cache usage.

use std::path::PathBuf;
use std::process::Command;

use fnr_migrate::doctor::{self as mdoc, Status, Version};
use serde::Serialize;

use crate::clean::{dir_size, human_bytes};
use crate::setup::{self, Agent};

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

pub fn run(project_root: &std::path::Path) -> UnifiedDoctorReport {
    let mut base = mdoc::run();
    // Keep only rust-anything agent-setup checks out if we replace them with fnr setup checks,
    // while retaining toolchain/runtime checks from migrate-engine's rich probe suite.
    base.checks.retain(|c| {
        !c.name.starts_with("agent ") && !c.name.ends_with(" skill") && !c.name.ends_with(" mcp")
    });
    check_fnr_setup(&mut base.checks);

    let extra_runtimes = vec![
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

    let agents = vec![
        agent_check("claude", "claude", "npm i -g @anthropic-ai/claude-code"),
        agent_check("codex", "codex", "npm i -g @openai/codex"),
        agent_check("gemini", "gemini", "npm i -g @google/gemini-cli"),
        agent_check("opencode", "opencode", "https://opencode.ai"),
        agent_check("aider", "aider", "pip install aider-chat"),
    ];

    let shared_dir = fnr_migrate::cache::shared_dir();
    let shared_bytes = dir_size(&shared_dir);
    let local_fast_target = project_root.join(".fast").join("target");
    let local_fast_bytes = dir_size(&local_fast_target);

    let ok = base.ok();
    UnifiedDoctorReport {
        ok,
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
    let mut out = String::from("Toolchains & Core (14 languages):\n");
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
    out.push_str("\nAdditional Runtimes (C++, R, Bun, Deno, Env Runners):\n");
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
        let dir = std::env::temp_dir().join(format!("fnr-doc-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let rep = run(&dir);
        assert!(!rep.checks.is_empty());
        assert_eq!(rep.agents.len(), 5);
        assert_eq!(rep.extra_runtimes.len(), 6);
        let rendered = text(&rep);
        assert!(rendered.contains("Build Caches:"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
