//! MCP prompts: ready-to-send workflows for AI agents.

use serde_json::{Value, json};

const SKILL: &str = include_str!("../../../skills/fast-n-rust/SKILL.md");
const WHOLE_APP: &str = include_str!("../../../skills/fast-n-rust/whole-app.md");

fn strip_frontmatter(text: &str) -> &str {
    text.strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map_or(text, |(_, body)| body.trim_start())
}

pub fn list() -> Value {
    json!({
        "prompts": [
            {
                "name": "fast_optimize",
                "title": "Speed up the slow 5% in Rust",
                "description": "Profile a representative workload with `scan`, port the top portable hotspot with `port`, prove parity with `verify` + `gate`, `enable`, and `bench`.",
                "arguments": [
                    { "name": "path", "description": "Project root.", "required": true },
                    { "name": "command", "description": "Workload command, e.g. \"python app.py\".", "required": false }
                ]
            },
            {
                "name": "port_unit",
                "title": "Deep unit migration to Rust",
                "description": "Port one unit with AST analysis, edge-case recording, diff, fuzz, shadow, and strict gate.",
                "arguments": [
                    { "name": "path", "description": "Repository root.", "required": true },
                    { "name": "unit", "description": "Unit id from analyze; best first port when omitted.", "required": false }
                ]
            },
            {
                "name": "escalate_port",
                "title": "Escalate hotspot port to Rayon batch / shadow / gate",
                "description": "Promote a `.fast/ports/<id>` port to a `rust/` unit with Rayon `--batch`, fuzzing, and shadow support.",
                "arguments": [
                    { "name": "path", "description": "Project root.", "required": true },
                    { "name": "id", "description": "Hotspot function id.", "required": true }
                ]
            },
            {
                "name": "whole_app_rewrite",
                "title": "Rewrite a whole app into pure Rust",
                "description": "Strangler-fig migration: analyze, app_record, app_front_door, port every unit, app_wire, and app_lift.",
                "arguments": [
                    { "name": "path", "description": "Repository root.", "required": true }
                ]
            }
        ]
    })
}

pub fn get(name: &str, arguments: &Value) -> Result<Value, String> {
    let arg = |k: &str| arguments[k].as_str().filter(|s| !s.trim().is_empty());
    let path = arg("path").ok_or_else(|| format!("prompt `{name}` requires `path`"))?;
    let body = strip_frontmatter(SKILL);

    let (desc, text) = match name {
        "fast_optimize" => {
            let cmd_hint = arg("command").map_or_else(
                || "Detect how the project runs its representative workload.".to_string(),
                |c| format!("Workload command: `{c}`."),
            );
            (
                "Speed up the slow 5% in Rust",
                format!(
                    "Optimize hot paths in `{path}` using the compact fast-n-rust MCP tools (`inspect(scan)` -> `scaffold(port)` -> edit Rust -> `check(verify, gate=true, fuzz_cases=50)` -> `switch(enable)` -> `measure(bench)`), or legacy tool names (`scan`, `port`, `verify`, `enable`, `bench`). Pass `duration` (seconds) to `scan` for HTTP servers/long-running apps. Check `advice.mode` and `advice.diagnosis` first: do not force Rust on `optimize_io_first`, extract pure helpers on `extract_pure_helper`, and relay any `deploy` readiness warnings after `enable`.\n\n\
                     {cmd_hint}\n\n{body}"
                ),
            )
        }
        "port_unit" => {
            let unit_line = arg("unit").map_or_else(
                || "Run `analyze` on the repository and propose the top `start_here` unit before bridging.".to_string(),
                |u| format!("Target unit: `{u}`."),
            );
            (
                "Deep unit migration to Rust",
                format!("Port a unit in `{path}` to Rust.\n\n{unit_line}\n\n{body}"),
            )
        }
        "escalate_port" => {
            let id = arg("id").ok_or_else(|| "prompt `escalate_port` requires `id`".to_string())?;
            (
                "Escalate hotspot port to Rayon batch / shadow / gate",
                format!(
                    "Escalate hotspot port `{id}` in `{path}` by calling `escalate` (optionally with `batch`), then run `record`, `build`, `diff`, `fuzz`, and `gate`.\n\n{body}"
                ),
            )
        }
        "whole_app_rewrite" => (
            "Rewrite a whole app into pure Rust",
            format!(
                "Rewrite the application in `{path}` into a standalone pure Rust app.\n\n{WHOLE_APP}\n\n{body}"
            ),
        ),
        other => return Err(format!("unknown prompt `{other}`")),
    };

    Ok(json!({
        "description": desc,
        "messages": [
            { "role": "user", "content": { "type": "text", "text": text } }
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_and_renders_all_prompts() {
        let l = list();
        let arr = l["prompts"].as_array().unwrap();
        assert_eq!(arr.len(), 4);
        let p = get(
            "fast_optimize",
            &json!({ "path": "/tmp/app", "command": "python app.py" }),
        )
        .unwrap();
        let txt = p["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(txt.contains("/tmp/app") && txt.contains("python app.py"));
        assert!(!txt.starts_with("---"));
        assert!(get("nope", &json!({ "path": "/tmp/app" })).is_err());
    }
}
