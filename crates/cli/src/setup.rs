//! `setup`: register `fast-n-rust` MCP server and workflow skills with AI coding agents.
//! Preserves all unrelated user settings, creates backups by default, and supports `--remove` and `--dry-run`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value, json};

pub const NAME: &str = "fast-n-rust";
pub const SKILL: &str = include_str!("../../../skills/fast-n-rust/SKILL.md");
pub const UNIT_PORTING_SKILL: &str = include_str!("../../../skills/fast-n-rust/unit-porting.md");
pub const WHOLE_APP_SKILL: &str = include_str!("../../../skills/fast-n-rust/whole-app.md");

const SKILL_FILES: &[(&str, &str)] = &[
    ("SKILL.md", SKILL),
    ("unit-porting.md", UNIT_PORTING_SKILL),
    ("whole-app.md", WHOLE_APP_SKILL),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Cursor,
    Gemini,
    Antigravity,
    #[value(name = "opencode")]
    OpenCode,
}

impl Agent {
    pub const ALL: [Agent; 6] = [
        Agent::Claude,
        Agent::Codex,
        Agent::Cursor,
        Agent::Gemini,
        Agent::Antigravity,
        Agent::OpenCode,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
            Self::Gemini => "Gemini CLI",
            Self::Antigravity => "Antigravity",
            Self::OpenCode => "OpenCode",
        }
    }

    fn markers(self) -> (&'static str, &'static str) {
        match self {
            Self::Claude => (".claude", "claude"),
            Self::Codex => (".codex", "codex"),
            Self::Cursor => (".cursor", "cursor"),
            Self::Gemini => (".gemini", "gemini"),
            Self::Antigravity => (".gemini/antigravity", "antigravity"),
            Self::OpenCode => (".config/opencode", "opencode"),
        }
    }

    pub fn config(self, home: &Path) -> PathBuf {
        home.join(match self {
            Self::Claude => ".claude.json",
            Self::Codex => ".codex/config.toml",
            Self::Cursor => ".cursor/mcp.json",
            Self::Gemini => ".gemini/settings.json",
            Self::Antigravity => ".gemini/antigravity/mcp_config.json",
            Self::OpenCode => ".config/opencode/opencode.json",
        })
    }

    pub fn skill_dir(self, home: &Path) -> Option<PathBuf> {
        match self {
            Self::Claude => Some(home.join(".claude/skills").join(NAME)),
            Self::Codex => Some(home.join(".codex/skills").join(NAME)),
            Self::Gemini | Self::Antigravity => Some(home.join(".gemini/config/skills").join(NAME)),
            Self::OpenCode => Some(home.join(".config/opencode/skills").join(NAME)),
            Self::Cursor => None,
        }
    }
}

pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("neither HOME nor USERPROFILE is set")
}

fn on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        dir.join(bin).is_file() || (cfg!(windows) && dir.join(format!("{bin}.exe")).is_file())
    })
}

fn detected(home: &Path, agent: Agent) -> bool {
    let (dir, bin) = agent.markers();
    home.join(dir).exists() || agent.config(home).exists() || on_path(bin)
}

pub fn agent_status(home: &Path, agent: Agent, _exe: &Path) -> (bool, bool) {
    let det = detected(home, agent);
    let cfg = agent.config(home);
    let configured = if cfg.is_file() {
        let text = std::fs::read_to_string(&cfg).unwrap_or_default();
        match agent {
            Agent::Codex => text
                .parse::<toml_edit::DocumentMut>()
                .ok()
                .and_then(|doc| {
                    doc.get("mcp_servers")?
                        .as_table_like()?
                        .get(NAME)
                        .map(|_| true)
                })
                .unwrap_or(false),
            Agent::OpenCode => serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("mcp")?.as_object()?.get(NAME).map(|_| true))
                .unwrap_or(false),
            _ => serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("mcpServers")?.as_object()?.get(NAME).map(|_| true))
                .unwrap_or(false),
        }
    } else {
        false
    };
    (det, configured)
}

pub struct Options<'a> {
    pub home: &'a Path,
    pub agents: Vec<Agent>,
    pub command: PathBuf,
    pub remove: bool,
    pub dry_run: bool,
    pub backup: bool,
}

#[derive(Debug, Serialize)]
pub struct AgentReport {
    pub agent: String,
    pub config: PathBuf,
    pub skill_dir: Option<PathBuf>,
    pub changed: bool,
    pub backup: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct SetupReport {
    pub dry_run: bool,
    pub removed: bool,
    pub agents: Vec<AgentReport>,
}

fn write_atomic(
    path: &Path,
    content: &str,
    backup: bool,
    dry_run: bool,
) -> Result<Option<PathBuf>> {
    if dry_run {
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bak_path = None;
    if backup && path.exists() {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".fast-n-rust.bak");
        let bak = path.with_file_name(name);
        std::fs::copy(path, &bak)?;
        bak_path = Some(bak);
    }
    let tmp = path.with_extension("fast-n-rust.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(bak_path)
}

fn update_json_mcp(
    path: &Path,
    command: &str,
    remove: bool,
    opencode: bool,
) -> Result<(bool, String)> {
    let existing = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else if remove {
        return Ok((false, String::new()));
    } else {
        "{}\n".to_string()
    };
    let mut root: Value = serde_json::from_str(&existing)
        .with_context(|| format!("invalid JSON in {} (refusing to overwrite)", path.display()))?;
    let obj = root
        .as_object_mut()
        .with_context(|| format!("{} must be a JSON object", path.display()))?;

    let changed = if opencode {
        let mcp = obj
            .entry("mcp")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .context("`mcp` must be an object")?;
        if remove {
            mcp.remove(NAME).is_some()
        } else {
            let desired = json!({
                "type": "local",
                "command": [command, "mcp"],
                "enabled": true
            });
            let same = mcp.get(NAME) == Some(&desired);
            mcp.insert(NAME.to_string(), desired);
            !same
        }
    } else {
        let servers = obj
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .context("`mcpServers` must be an object")?;
        if remove {
            servers.remove(NAME).is_some()
        } else {
            let mut entry = servers
                .get(NAME)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            entry.insert("command".into(), json!(command));
            entry.insert("args".into(), json!(["mcp"]));
            let new_val = Value::Object(entry);
            let same = servers.get(NAME) == Some(&new_val);
            servers.insert(NAME.to_string(), new_val);
            !same
        }
    };

    Ok((changed, serde_json::to_string_pretty(&root)? + "\n"))
}

fn update_codex_toml(path: &Path, command: &str, remove: bool) -> Result<(bool, String)> {
    let existing = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else if remove {
        return Ok((false, String::new()));
    } else {
        String::new()
    };
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("invalid TOML in {} (refusing to overwrite)", path.display()))?;

    let changed = if remove {
        doc.get_mut("mcp_servers")
            .and_then(|v| v.as_table_like_mut())
            .is_some_and(|t| t.remove(NAME).is_some())
    } else {
        if doc.get("mcp_servers").is_none() {
            doc["mcp_servers"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let servers = doc["mcp_servers"]
            .as_table_like_mut()
            .context("`mcp_servers` in config.toml must be a table")?;
        if servers.get(NAME).and_then(|v| v.as_table_like()).is_none() {
            servers.insert(NAME, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let entry = servers
            .get_mut(NAME)
            .and_then(|v| v.as_table_like_mut())
            .context("`mcp_servers.fast-n-rust` in config.toml must be a table")?;
        let mut arr = toml_edit::Array::new();
        arr.push("mcp");
        entry.insert("command", toml_edit::value(command));
        entry.insert("args", toml_edit::value(arr));
        let rendered = doc.to_string();
        return Ok((rendered != existing, rendered));
    };
    Ok((changed, doc.to_string()))
}

pub fn setup(opts: &Options<'_>) -> Result<SetupReport> {
    let chosen = if opts.agents.is_empty() {
        let found: Vec<Agent> = Agent::ALL
            .into_iter()
            .filter(|a| detected(opts.home, *a))
            .collect();
        if found.is_empty() {
            bail!(
                "no supported AI coding agents detected on this machine (pass --agent claude|codex|cursor|gemini|antigravity|opencode)"
            );
        }
        found
    } else {
        opts.agents.clone()
    };

    let cmd_str = opts.command.to_string_lossy().into_owned();
    let mut reports = Vec::new();

    for agent in chosen {
        let cfg = agent.config(opts.home);
        let (cfg_changed, next_text) = match agent {
            Agent::Codex => update_codex_toml(&cfg, &cmd_str, opts.remove)?,
            Agent::OpenCode => update_json_mcp(&cfg, &cmd_str, opts.remove, true)?,
            _ => update_json_mcp(&cfg, &cmd_str, opts.remove, false)?,
        };
        let bak = if cfg_changed {
            write_atomic(&cfg, &next_text, opts.backup, opts.dry_run)?
        } else {
            None
        };

        let mut skill_changed = false;
        let skill_dir = agent.skill_dir(opts.home);
        if let Some(ref sdir) = skill_dir {
            if opts.remove {
                if sdir.exists() {
                    skill_changed = true;
                    if !opts.dry_run {
                        std::fs::remove_dir_all(sdir)?;
                    }
                }
            } else {
                for (fname, content) in SKILL_FILES {
                    let target = sdir.join(fname);
                    let cur = std::fs::read_to_string(&target).ok();
                    if cur.as_deref() != Some(*content) {
                        skill_changed = true;
                        if !opts.dry_run {
                            std::fs::create_dir_all(sdir)?;
                            std::fs::write(&target, content)?;
                        }
                    }
                }
            }
        }

        reports.push(AgentReport {
            agent: agent.name().to_string(),
            config: cfg,
            skill_dir,
            changed: cfg_changed || skill_changed,
            backup: bak,
        });
    }

    Ok(SetupReport {
        dry_run: opts.dry_run,
        removed: opts.remove,
        agents: reports,
    })
}

pub fn text(r: &SetupReport) -> String {
    let verb = match (r.dry_run, r.removed) {
        (true, true) => "Would remove",
        (true, false) => "Would configure",
        (false, true) => "Removed",
        (false, false) => "Configured",
    };
    let mut out = String::new();
    for a in &r.agents {
        let state = if a.changed {
            verb
        } else {
            "Already up to date"
        };
        out.push_str(&format!(
            "  {state}: {} -> {}\n",
            a.agent,
            a.config.display()
        ));
        if let Some(ref s) = a.skill_dir {
            out.push_str(&format!("    skill: {}\n", s.display()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_installs_preserves_settings_and_removes() {
        let home = std::env::temp_dir().join(format!("fnr-setup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"other":{"command":"x"}}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.other]\ncommand = \"y\"\n",
        )
        .unwrap();

        let rep = setup(&Options {
            home: &home,
            agents: vec![Agent::Claude, Agent::Codex, Agent::OpenCode],
            command: PathBuf::from("/usr/local/bin/fast-n-rust"),
            remove: false,
            dry_run: false,
            backup: true,
        })
        .unwrap();
        assert_eq!(rep.agents.len(), 3);

        let claude: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                .unwrap();
        assert!(claude["mcpServers"]["other"].is_object());
        assert_eq!(claude["mcpServers"]["fast-n-rust"]["args"], json!(["mcp"]));
        assert!(home.join(".claude/skills/fast-n-rust/SKILL.md").is_file());
        assert!(
            home.join(".claude/skills/fast-n-rust/unit-porting.md")
                .is_file()
        );
        assert!(
            home.join(".claude/skills/fast-n-rust/whole-app.md")
                .is_file()
        );

        let codex = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(codex.contains("[mcp_servers.other]"));
        assert!(codex.contains("[mcp_servers.fast-n-rust]"));

        // Verify custom key preservation + idempotency in Codex TOML
        let with_custom = format!("{codex}tool_timeout_sec = 120\n");
        std::fs::write(home.join(".codex/config.toml"), &with_custom).unwrap();
        let rep2 = setup(&Options {
            home: &home,
            agents: vec![Agent::Codex],
            command: PathBuf::from("/usr/local/bin/fast-n-rust"),
            remove: false,
            dry_run: false,
            backup: false,
        })
        .unwrap();
        assert!(
            !rep2.agents[0].changed,
            "second setup run must be idempotent"
        );
        let codex_custom = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(codex_custom.contains("tool_timeout_sec = 120"));
        assert_eq!(
            agent_status(&home, Agent::Codex, Path::new("fnr")),
            (true, true)
        );

        setup(&Options {
            home: &home,
            agents: vec![Agent::Claude, Agent::Codex],
            command: PathBuf::from("/usr/local/bin/fast-n-rust"),
            remove: true,
            dry_run: false,
            backup: false,
        })
        .unwrap();
        let claude_after: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                .unwrap();
        assert!(claude_after["mcpServers"].get("fast-n-rust").is_none());
        assert!(claude_after["mcpServers"].get("other").is_some());
        assert!(!home.join(".claude/skills/fast-n-rust").exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
