//! Unified threaded MCP server on stdio for `fast-n-rust` (`fnr mcp`).
//!
//! Combines `fast-anything`'s hotspot tools and `rust-anything`'s deep unit + whole-app tools
//! over concurrent JSON-RPC 2.0 workers so `ping` stays instant and `notifications/cancelled`,
//! `notifications/progress`, and `resources/*` work reliably across both engines.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use fnr_migrate::{
    analyze, app, bridge, effects, front_door, fuzz, gate, harness, lift, port_map, shadow,
};

use crate::{clean, doctor, mcp_prompts, setup, synergy};

const PROTOCOL: &str = "2025-11-25";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolMode {
    #[default]
    Compact,
    Full,
}

struct SharedWriter<W: Write> {
    out: Mutex<BufWriter<W>>,
}

impl<W: Write> SharedWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            out: Mutex::new(BufWriter::new(inner)),
        }
    }

    fn send(&self, msg: &Value) -> io::Result<()> {
        let mut guard = self.out.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::to_writer(&mut *guard, msg)?;
        guard.write_all(b"\n")?;
        guard.flush()
    }
}

const MAX_CONCURRENT_WORKERS: usize = 8;

struct WorkerLimiter {
    active: Mutex<usize>,
    cvar: Condvar,
    max: usize,
}

impl WorkerLimiter {
    fn new(max: usize) -> Self {
        Self {
            active: Mutex::new(0),
            cvar: Condvar::new(),
            max,
        }
    }

    fn acquire(self: &Arc<Self>) -> WorkerPermit {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        while *active >= self.max {
            active = self.cvar.wait(active).unwrap_or_else(|e| e.into_inner());
        }
        *active += 1;
        WorkerPermit {
            limiter: Arc::clone(self),
        }
    }
}

struct WorkerPermit {
    limiter: Arc<WorkerLimiter>,
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        let mut active = self
            .limiter
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *active = active.saturating_sub(1);
        self.limiter.cvar.notify_one();
    }
}

struct ProgressGuard;

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        fnr_fast::ui::set_progress(None);
    }
}

struct SessionContext {
    default_root: Mutex<PathBuf>,
}

impl Default for SessionContext {
    fn default() -> Self {
        Self {
            default_root: Mutex::new(PathBuf::from(".")),
        }
    }
}

impl SessionContext {
    fn root(&self) -> PathBuf {
        self.default_root
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_root(&self, path: PathBuf) {
        *self.default_root.lock().unwrap_or_else(|e| e.into_inner()) = path;
    }
}

fn workspace_lock(root: &Path) -> Arc<Mutex<()>> {
    static WORKSPACE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let map = WORKSPACE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(
        guard
            .entry(canonical)
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

fn is_read_only_call(name: &str, args: &Value) -> bool {
    match name {
        "doctor" | "status" | "map" | "analyze" => true,
        "inspect" => matches!(
            args["action"].as_str(),
            Some("status" | "doctor" | "map" | "analyze")
        ),
        #[cfg(test)]
        "test_sleep" => args["mutate"].as_bool() != Some(true),
        _ => false,
    }
}

fn resolve_call_workspace(args: &Value, default_root: &Path) -> PathBuf {
    if let Some(r) = args["root"].as_str().or_else(|| args["repo"].as_str()) {
        return PathBuf::from(r);
    }
    if let Some(c) = args["crate"].as_str() {
        let p = Path::new(c);
        if let Some(parent) = p.parent()
            && !parent.as_os_str().is_empty()
        {
            return parent.to_path_buf();
        }
    }
    default_root.to_path_buf()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            out.push(((hi << 4) | lo) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn strip_file_uri(raw: &str, windows: bool) -> String {
    if let Some(rest) = raw.strip_prefix("file://") {
        let decoded = percent_decode(rest);
        let decoded = decoded
            .strip_prefix("localhost/")
            .map(|stripped| format!("/{stripped}"))
            .unwrap_or(decoded);
        if windows && decoded.starts_with('/') && decoded.as_bytes().get(2) == Some(&b':') {
            decoded[1..].to_string()
        } else {
            decoded
        }
    } else {
        raw.to_string()
    }
}

fn parse_root_uri_or_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path_str = strip_file_uri(trimmed, cfg!(windows));
    let pb = PathBuf::from(path_str);
    pb.is_dir().then_some(pb)
}

fn extract_init_root(params: &Value) -> Option<PathBuf> {
    params["rootUri"]
        .as_str()
        .and_then(parse_root_uri_or_path)
        .or_else(|| {
            params["workspaceFolders"]
                .as_array()
                .and_then(|folders| folders.first())
                .and_then(|f| f["uri"].as_str())
                .and_then(parse_root_uri_or_path)
        })
        .or_else(|| params["rootPath"].as_str().and_then(parse_root_uri_or_path))
}

pub fn serve() -> Result<()> {
    serve_with_mode(ToolMode::Compact)
}

pub fn serve_with_mode(mode: ToolMode) -> Result<()> {
    serve_io_with_mode(BufReader::new(io::stdin()), io::stdout(), mode)
}

pub fn serve_io<R: BufRead, W: Write + Send + 'static>(input: R, output: W) -> Result<()> {
    serve_io_with_mode(input, output, ToolMode::Compact)
}

pub fn serve_io_with_mode<R: BufRead, W: Write + Send + 'static>(
    input: R,
    output: W,
    mode: ToolMode,
) -> Result<()> {
    // Silence `fnr_fast::ui` stdout so JSON-RPC remains 100% pure on stdout.
    fnr_fast::ui::set_quiet(true);
    let writer = Arc::new(SharedWriter::new(output));
    let cancelled = Arc::new(Mutex::new(HashSet::<String>::new()));
    let inflight = Arc::new(Mutex::new(HashSet::<String>::new()));
    let limiter = Arc::new(WorkerLimiter::new(MAX_CONCURRENT_WORKERS));
    let session = Arc::new(SessionContext::default());
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    for line in input.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let _ = writer.send(&error(Value::Null, -32700, &format!("parse error: {e}")));
                continue;
            }
        };

        let method = req["method"].as_str().unwrap_or("");
        if method == "notifications/cancelled" {
            let rid = &req["params"]["requestId"];
            if !rid.is_null() {
                let id_key = rid.to_string();
                let inflight_now = inflight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&id_key);
                if inflight_now {
                    cancelled
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(id_key);
                }
            }
            continue;
        }

        if method == "tools/call" && !req["id"].is_null() {
            // Reap finished worker handles so long-lived MCP sessions don't leak handles.
            let mut still_running = Vec::with_capacity(handles.len());
            for h in handles.drain(..) {
                if h.is_finished() {
                    let _ = h.join();
                } else {
                    still_running.push(h);
                }
            }
            handles = still_running;

            let id = req["id"].clone();
            let id_key = id.to_string();
            inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id_key.clone());

            let lim = Arc::clone(&limiter);
            let w = Arc::clone(&writer);
            let canc = Arc::clone(&cancelled);
            let inf = Arc::clone(&inflight);
            let sess = Arc::clone(&session);
            let req_owned = req.clone();
            handles.push(std::thread::spawn(move || {
                if canc
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&id_key)
                {
                    inf.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id_key);
                    canc.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id_key);
                    return;
                }
                let _permit = lim.acquire();
                if canc
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&id_key)
                {
                    inf.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id_key);
                    canc.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id_key);
                    return;
                }
                let tool_name = req_owned["params"]["name"].as_str().unwrap_or("");
                let tool_args = &req_owned["params"]["arguments"];
                let ws_lock = if is_read_only_call(tool_name, tool_args) {
                    None
                } else {
                    let ws_root = resolve_call_workspace(tool_args, &sess.root());
                    Some(workspace_lock(&ws_root))
                };
                let _ws_guard = ws_lock
                    .as_ref()
                    .map(|lock| lock.lock().unwrap_or_else(|e| e.into_inner()));

                // If cancelled while waiting on worker limit or workspace lock, bail before touching files.
                if canc
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&id_key)
                {
                    inf.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id_key);
                    canc.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id_key);
                    return;
                }

                let token = req_owned["params"]["_meta"]["progressToken"].clone();
                let _progress_guard = if !token.is_null() {
                    let w_prog = Arc::clone(&w);
                    let t_prog = token.clone();
                    let mut seq = 0u64;
                    fnr_fast::ui::set_progress(Some(Box::new(move |msg: &str| {
                        seq += 1;
                        let _ = w_prog.send(&json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": { "progressToken": t_prog, "progress": seq, "message": msg }
                        }));
                    })));
                    Some(ProgressGuard)
                } else {
                    None
                };
                let resp = handle_with_context(&req_owned, mode, &sess);
                drop(_progress_guard);
                inf.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id_key);
                let was_cancelled = canc
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id_key);
                if !was_cancelled && let Some(r) = resp {
                    let _ = w.send(&r);
                }
            }));
            continue;
        }

        if let Some(resp) = handle_with_context(&req, mode, &session) {
            writer.send(&resp)?;
        }
    }

    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

pub fn handle(request: &Value) -> Option<Value> {
    handle_with_mode(request, ToolMode::Compact)
}

pub fn handle_with_mode(request: &Value, mode: ToolMode) -> Option<Value> {
    let session = SessionContext::default();
    handle_with_context(request, mode, &session)
}

fn handle_with_context(request: &Value, mode: ToolMode, session: &SessionContext) -> Option<Value> {
    let id = request.get("id")?.clone();
    if id.is_null()
        && request["method"]
            .as_str()
            .unwrap_or("")
            .starts_with("notifications/")
    {
        return None;
    }
    let method = request["method"].as_str().unwrap_or("");
    let params = &request["params"];
    let result = match method {
        "initialize" => {
            if let Some(root) = extract_init_root(params) {
                session.set_root(root);
            }
            json!({
                "protocolVersion": PROTOCOL,
                "capabilities": {
                    "tools": { "listChanged": false },
                    "prompts": { "listChanged": false },
                    "resources": { "subscribe": false, "listChanged": false }
                },
                "serverInfo": {
                    "name": "fast-n-rust",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({
            "tools": match mode {
                ToolMode::Compact => compact_tools(),
                ToolMode::Full => tools(),
            }
        }),
        "prompts/list" => mcp_prompts::list(),
        "prompts/get" => {
            let name = params["name"].as_str().unwrap_or("");
            match mcp_prompts::get(name, &params["arguments"]) {
                Ok(v) => v,
                Err(msg) => return Some(error(id, -32602, &msg)),
            }
        }
        "resources/list" => resources_list(),
        "resources/read" => {
            let uri = params["uri"].as_str().unwrap_or("");
            let default_root = session.root();
            match read_resource(uri, &default_root) {
                Ok(v) => v,
                Err(msg) => return Some(error(id, -32602, &msg)),
            }
        }
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = &params["arguments"];
            let default_root = session.root();
            match call_with_default_root(name, args, &default_root) {
                Ok(v) => {
                    let pretty = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
                    json!({
                        "content": [{ "type": "text", "text": pretty }],
                        "structuredContent": v,
                        "isError": false
                    })
                }
                Err(e) => failure(&format!("{e:#}"), name),
            }
        }
        _ => return Some(error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn resources_list() -> Value {
    json!({
        "resources": [
            {
                "uri": "fnr://guide/skill",
                "name": "fast-n-rust Skill Guide",
                "description": "Decision tree and workflow rules for Fast 5%, Escalate, and Whole-App modes.",
                "mimeType": "text/markdown"
            },
            {
                "uri": "fnr://guide/unit-porting",
                "name": "Deep Unit & Batch Porting Guide",
                "description": "Language bridge rules (FnIndex, pynum, utf16, regex traps), stall protocol, and gate rules.",
                "mimeType": "text/markdown"
            },
            {
                "uri": "fnr://guide/whole-app",
                "name": "Whole-App Pure Rust Guide",
                "description": "Strangler-fig playbook: analyze, app record, front-door, wire, and lift.",
                "mimeType": "text/markdown"
            },
            {
                "uri": "fnr://project/status",
                "name": "Project Hotspot & Port Status",
                "description": "Current .fast/state.json hotspots and ports for the working directory.",
                "mimeType": "application/json"
            },
            {
                "uri": "fnr://project/port-map",
                "name": "Repository Port Map",
                "description": "Markdown summary of rust/ unit crates, cases, diff results, and unported units.",
                "mimeType": "text/markdown"
            }
        ]
    })
}

fn read_resource(uri: &str, default_root: &Path) -> Result<Value, String> {
    let (mime, text) = match uri {
        "fnr://guide/skill" => ("text/markdown", setup::SKILL.to_string()),
        "fnr://guide/unit-porting" => ("text/markdown", setup::UNIT_PORTING_SKILL.to_string()),
        "fnr://guide/whole-app" => ("text/markdown", setup::WHOLE_APP_SKILL.to_string()),
        "fnr://project/status" => {
            let fast = fnr_fast::state::Ctx::open(default_root)
                .map(|ctx| {
                    json!({
                        "lang": ctx.state.lang,
                        "wall": ctx.state.wall,
                        "hotspots": ctx.state.hotspots.len(),
                        "ports": ctx.state.ports,
                    })
                })
                .unwrap_or_else(|e| json!({ "error": e.to_string() }));
            let body = serde_json::to_string_pretty(&fast).unwrap_or_else(|_| "{}".to_string());
            ("application/json", body)
        }
        "fnr://project/port-map" => {
            let body = port_map::map(default_root, &[])
                .map(|m| port_map::markdown(&m))
                .unwrap_or_else(|e| format!("Error generating port map: {e}"));
            ("text/markdown", body)
        }
        _ => return Err(format!("unknown resource `{uri}`")),
    };
    Ok(json!({
        "contents": [{ "uri": uri, "mimeType": mime, "text": text }]
    }))
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn failure(text: &str, tool: &str) -> Value {
    let next = match tool {
        "scan" | "go" => "Pass `command` (e.g. [\"python3\", \"app.py\"]) and `root`.",
        "port" | "verify" | "enable" | "disable" => {
            "Run `scan` first, then `port`, edit Rust, then `verify`."
        }
        "bridge" | "analyze" => "Run `analyze` on the repo root to see valid unit ids.",
        "diff" | "gate" | "build" => "Run `record` and `build` first, or check `doctor`.",
        _ => "Call `tools/list` or `doctor` to inspect arguments and toolchain status.",
    };
    let payload = json!({ "error": text, "tool": tool, "next": next });
    json!({
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&payload).unwrap_or_else(|_| text.to_string()) }],
        "structuredContent": payload,
        "isError": true
    })
}

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: Value,
    read_only: bool,
    destructive: bool,
    idempotent: bool,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required
        },
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": false
        }
    })
}

pub fn compact_tools() -> Value {
    let root_p = json!({ "type": "string", "description": "Project root directory (default `.`)" });
    let str_arr = json!({ "type": "array", "items": { "type": "string" } });
    let req_action = json!(["action"]);
    json!([
        tool(
            "inspect",
            "Discover hotspots, analyze codebase units, check toolchain (`doctor`), or inspect port status/map.",
            json!({
                "action": { "type": "string", "enum": ["scan", "analyze", "status", "doctor", "map"] },
                "root": root_p,
                "command": str_arr,
                "cdb": { "type": "string", "description": "C/C++ compile_commands.json path for `scan`" },
                "duration": { "type": "integer", "description": "Stop `scan` profiling after N seconds (for servers/long-running apps on Python, Node, or Bun)." },
                "load": { "type": "string", "description": "Optional client load command executed during timed server `scan` / `port` / `bench`." },
                "whole_app": { "type": "boolean" },
                "lang": { "type": "string" },
                "all": { "type": "boolean", "description": "For `doctor`: check all 14 languages instead of scoping to the project." }
            }),
            req_action.clone(),
            false,
            false,
            false,
        ),
        tool(
            "scaffold",
            "Record real calls and scaffold a hotspot port (`.fast/ports/<id>`), deep `rust/` unit bridge, refresh bindings, or escalate hotspot -> Rayon unit.",
            json!({
                "action": { "type": "string", "enum": ["port", "bridge", "refresh", "escalate"] },
                "root": root_p,
                "id": { "type": "string" },
                "unit": { "type": "string" },
                "crate": { "type": "string" },
                "samples": { "type": "integer" },
                "batch": { "type": "string" },
                "batch_list": str_arr
            }),
            req_action.clone(),
            false,
            false,
            false,
        ),
        tool(
            "check",
            "Build, verify parity (`verify`/`diff`), property-fuzz (`fuzz`), run strict `gate`, or run full `ci` across `.fast/` ports or `rust/` crates.",
            json!({
                "action": { "type": "string", "enum": ["verify", "diff", "fuzz", "gate", "ci", "record", "build"] },
                "root": root_p,
                "target": { "type": "string" },
                "id": { "type": "string" },
                "crate": { "type": "string" },
                "all": { "type": "boolean" },
                "gate": { "type": "boolean" },
                "fuzz_cases": { "type": "integer" },
                "cases": { "type": "integer" },
                "float_tolerance": { "type": "number" },
                "save_regressions": { "type": "boolean" },
                "min_speedup": { "type": "number" },
                "bench": { "type": "boolean" },
                "runs": { "type": "integer" },
                "deploy": { "type": "boolean" },
                "native_cpu": { "type": "boolean" }
            }),
            req_action.clone(),
            false,
            false,
            false,
        ),
        tool(
            "switch",
            "Enable (with Ship LTO + optional `--pgo`) or disable a port/unit, generate `shadow` module, or `clean` artifacts.",
            json!({
                "action": { "type": "string", "enum": ["enable", "disable", "shadow", "clean"] },
                "root": root_p,
                "target": { "type": "string" },
                "crate": { "type": "string" },
                "force": { "type": "boolean" },
                "pgo": { "type": "boolean" },
                "native_cpu": { "type": "boolean" },
                "cache": { "type": "boolean" },
                "ports": { "type": "boolean" },
                "rust": { "type": "boolean" },
                "all": { "type": "boolean" },
                "dry_run": { "type": "boolean" }
            }),
            req_action.clone(),
            false,
            true,
            false,
        ),
        tool(
            "measure",
            "Run interleaved benchmark with 95% CI + peak RSS (`bench`), generate Markdown+SVG `report`, or `profile` a workload.",
            json!({
                "action": { "type": "string", "enum": ["bench", "report", "profile"] },
                "root": root_p,
                "crate": { "type": "string" },
                "runs": { "type": "integer" },
                "pgo": { "type": "boolean" },
                "native_cpu": { "type": "boolean" },
                "command": str_arr,
                "lang": { "type": "string" }
            }),
            req_action.clone(),
            false,
            false,
            false,
        ),
        tool(
            "ship",
            "Run end-to-end automated `go` loop, `package` a verified port/unit, or `deploy` (generate Dockerfile/CI workflow).",
            json!({
                "action": { "type": "string", "enum": ["go", "package", "deploy"] },
                "root": root_p,
                "command": str_arr,
                "cdb": { "type": "string" },
                "duration": { "type": "integer" },
                "load": { "type": "string" },
                "agent": { "type": "string" },
                "strict": { "type": "boolean" },
                "float_tolerance": { "type": "number" },
                "save_regressions": { "type": "boolean" },
                "min_speedup": { "type": "number" },
                "target": { "type": "string" },
                "crate": { "type": "string" },
                "out": { "type": "string" },
                "native_cpu": { "type": "boolean" },
                "dry_run": { "type": "boolean" },
                "write": { "type": "boolean" },
                "force": { "type": "boolean" }
            }),
            req_action.clone(),
            false,
            false,
            false,
        ),
        tool(
            "app",
            "Whole-app strangler-fig migration (`record`, `replay`, `front_door`, `wire`, `lift`).",
            json!({
                "action": { "type": "string", "enum": ["record", "replay", "front_door", "wire", "lift"] },
                "root": root_p,
                "run": str_arr,
                "unit": { "type": "string" },
                "commands": str_arr,
                "routes": str_arr,
                "http": { "type": "boolean" },
                "port": { "type": "integer" }
            }),
            req_action,
            false,
            false,
            false,
        ),
    ])
}

pub fn tools() -> Value {
    let root_p =
        json!({ "type": "string", "description": "Project / repository root (default '.')." });
    let crate_p = json!({ "type": "string", "description": "Path to generated unit crate (holds rust-anything.json)." });
    let id_p = json!({ "type": "string", "description": "Hotspot function id from `scan`." });
    let cmd_p = json!({ "type": "array", "items": { "type": "string" }, "description": "Command that runs the representative workload." });
    let node_args_p = json!({ "type": "array", "items": { "type": "string" }, "description": "Extra arguments for node (e.g. [\"--import=tsx\"])." });
    let resolve_from_p = json!({ "type": "string", "description": "Resolve npm packages from this directory's node_modules." });

    json!([
        // --- Fast 5% Hotspot Loop ---
        tool(
            "scan",
            "Profile an app and rank portable hotspots with Mode Advisor (fast vs batch vs whole-app).",
            json!({
                "root": root_p,
                "command": cmd_p,
                "cdb": { "type": "string", "description": "C/C++ compile_commands.json path." },
                "duration": { "type": "integer", "description": "Stop profiling after N seconds (for servers/long-running apps on Python, Node, or Bun)." },
                "load": { "type": "string", "description": "Client load command run while the server starts during timed profiling/recording/bench." }
            }),
            json!([]),
            false,
            false,
            true
        ),
        tool(
            "port",
            "Record up to N real calls + edge cases for a hotspot and scaffold `.fast/ports/<id>` with static-risk-enriched TASK.md.",
            json!({ "root": root_p, "id": id_p, "samples": { "type": "integer", "description": "Max real calls to record (default 500)." } }),
            json!([]),
            false,
            false,
            false
        ),
        tool(
            "verify",
            "Release-build a hotspot port, replay recorded + derived edge cases, and optionally run `gate` checks.",
            json!({
                "root": root_p, "id": id_p, "target": { "type": "string" },
                "all": { "type": "boolean" }, "gate": { "type": "boolean" },
                "float_tolerance": { "type": "number" }, "known_diffs": { "type": "string" },
                "min_speedup": { "type": "number" }, "fuzz_cases": { "type": "integer" },
                "seed": { "type": "integer" }, "save_regressions": { "type": "boolean" }
            }),
            json!([]),
            false,
            false,
            true
        ),
        tool(
            "go",
            "End-to-end hotspot loop (scan -> port -> agent -> verify -> optional strict gate -> enable -> bench) or unit mode.",
            json!({
                "root": root_p, "command": cmd_p, "cdb": { "type": "string" },
                "mode": { "type": "string", "enum": ["fast", "unit"] },
                "agent": { "type": "string" },
                "top": { "type": "integer" },
                "attempts": { "type": "integer" },
                "runs": { "type": "integer" },
                "samples": { "type": "integer" },
                "strict": { "type": "boolean" },
                "float_tolerance": { "type": "number" },
                "min_speedup": { "type": "number" },
                "function": { "type": "string" },
                "lang": { "type": "string" }
            }),
            json!([]),
            false,
            false,
            false
        ),
        tool(
            "enable",
            "Enable a verified hotspot port OR unit crate in place (reversible via `disable` or `FNR_DISABLE=1`).",
            json!({
                "root": root_p, "target": { "type": "string", "description": "Hotspot id or unit crate directory." },
                "crate": crate_p, "force": { "type": "boolean" },
                "pgo": { "type": "boolean" }, "native_cpu": { "type": "boolean" }
            }),
            json!([]),
            false,
            false,
            true
        ),
        tool(
            "disable",
            "Disable a hotspot port (`all: true` disables all) or unit crate, restoring source files byte-for-byte.",
            json!({ "root": root_p, "target": { "type": "string" }, "crate": crate_p, "all": { "type": "boolean" } }),
            json!([]),
            false,
            false,
            true
        ),
        tool(
            "bench",
            "Benchmark either the whole app (hotspot mode) or a unit crate (micro-bench).",
            json!({
                "root": root_p, "crate": crate_p, "runs": { "type": "integer" },
                "min_ms": { "type": "integer" }, "pgo": { "type": "boolean" },
                "native_cpu": { "type": "boolean" }, "resolve_from": resolve_from_p, "node_args": node_args_p
            }),
            json!([]),
            true,
            false,
            true
        ),
        tool(
            "report",
            "Generate Markdown + SVG performance report card for either the app or a unit crate.",
            json!({ "root": root_p, "crate": crate_p, "out": { "type": "string" }, "min_ms": { "type": "integer" }, "node_args": node_args_p }),
            json!([]),
            false,
            false,
            true
        ),
        tool(
            "status",
            "Show current scan, hotspot ports, `rust/` unit crates, and whole-app fallback status.",
            json!({ "root": root_p }),
            json!([]),
            true,
            false,
            true
        ),
        // --- Deep Unit Migration & Synergies ---
        tool(
            "analyze",
            "AST-analyze a repository into ranked migration units, dependencies, and optional whole-app plan.",
            json!({ "root": root_p, "lang": { "type": "string" }, "whole_app": { "type": "boolean" } }),
            json!(["root"]),
            true,
            false,
            true
        ),
        tool(
            "profile",
            "Profile a repository workload using runtime profilers and list hot functions, callers, and `--batch` candidates.",
            json!({ "root": root_p, "command": cmd_p, "lang": { "type": "string" } }),
            json!(["root", "command"]),
            false,
            false,
            true
        ),
        tool(
            "bridge",
            "Scaffold a `rust/<name>` unit crate with bindings (napi/wasm/PyO3/cgo/JNI/FFI/P/Invoke) and optional Rayon `--batch`.",
            json!({ "root": root_p, "unit": { "type": "string" }, "kind": { "type": "string" }, "out": { "type": "string" }, "name": { "type": "string" }, "force": { "type": "boolean" }, "batch": { "type": "array", "items": { "type": "string" } }, "keep": { "type": "array", "items": { "type": "string" } }, "as_json": { "type": "array", "items": { "type": "string" } } }),
            json!(["root", "unit"]),
            false,
            false,
            false
        ),
        tool(
            "refresh",
            "Regenerate bindings after editing signatures in `src/port.rs`.",
            json!({ "crate": crate_p, "batch": { "type": "array", "items": { "type": "string" } } }),
            json!(["crate"]),
            false,
            false,
            true
        ),
        tool(
            "record",
            "Record edge cases, semantic traps, and optional test suite calls into `cases.jsonl`.",
            json!({
                "crate": crate_p, "count": { "type": "integer" }, "inputs": { "type": "string" },
                "module": { "type": "string" }, "resolve_from": resolve_from_p, "node_args": node_args_p,
                "float_tolerance": { "type": "number" }, "test_command": { "type": "array", "items": { "type": "string" } },
                "profile": { "type": "string", "enum": ["host", "android"] }, "device": { "type": "string" }
            }),
            json!(["crate"]),
            false,
            false,
            false
        ),
        tool(
            "build",
            "Build a unit crate (host or android), all hotspot crates, or `--deploy` ship artifacts.",
            json!({
                "root": root_p, "crate": crate_p,
                "profile": { "type": "string", "enum": ["host", "android"] },
                "deploy": { "type": "boolean" },
                "native_cpu": { "type": "boolean" },
                "abi": { "type": "array", "items": { "type": "string" } },
                "api": { "type": "integer" },
                "ndk": { "type": "string" },
                "jni_libs": { "type": "string" }
            }),
            json!([]),
            false,
            false,
            true
        ),
        tool(
            "diff",
            "Replay every recorded case against a unit crate's Rust module.",
            json!({
                "crate": crate_p, "module": { "type": "string" }, "ignore_key_order": { "type": "boolean" },
                "node_args": node_args_p, "float_tolerance": { "type": "number" },
                "profile": { "type": "string", "enum": ["host", "android"] },
                "device": { "type": "string" }, "jni_libs": { "type": "string" }
            }),
            json!(["crate"]),
            true,
            false,
            true
        ),
        tool(
            "fuzz",
            "Property-fuzz a unit crate against the original runtime.",
            json!({
                "crate": crate_p, "cases": { "type": "integer" }, "seed": { "type": "integer" },
                "time": { "type": "number" }, "keep": { "type": "string", "enum": ["mismatches", "all"] },
                "resolve_from": resolve_from_p, "node_args": node_args_p
            }),
            json!(["crate"]),
            false,
            false,
            false
        ),
        tool(
            "shadow",
            "Generate a drop-in shadow module running original and Rust side by side.",
            json!({ "crate": crate_p, "out": { "type": "string" } }),
            json!(["crate"]),
            false,
            false,
            true
        ),
        tool(
            "gate",
            "Run strict Definition-of-Done checks (`fmt`, `clippy -D warnings`, `cargo test`, `unsafe` SAFETY audit, parity) on either a hotspot id or a unit crate.",
            json!({ "root": root_p, "target": { "type": "string" }, "crate": crate_p, "node_args": node_args_p }),
            json!([]),
            true,
            false,
            true
        ),
        tool(
            "escalate",
            "Promote a `.fast/ports/<id>` hotspot port into a `rust/` unit crate with Rayon `--batch`, fuzz, shadow, and reference source.",
            json!({ "root": root_p, "id": id_p, "batch": { "type": "string" } }),
            json!([]),
            false,
            false,
            false
        ),
        tool(
            "map",
            "Generate a port map across all units and crates in a repository.",
            json!({ "repo": root_p, "crates": { "type": "array", "items": { "type": "string" } } }),
            json!(["repo"]),
            true,
            false,
            true
        ),
        tool(
            "ci",
            "Run CI quality & speedup gate (`verify`, strict `gate`, optional `fuzz_cases`, `bench`, `min_speedup`).",
            json!({
                "root": root_p, "target": { "type": "string" }, "id": id_p,
                "float_tolerance": { "type": "number" }, "fuzz_cases": { "type": "integer" },
                "bench": { "type": "boolean" }, "runs": { "type": "integer" },
                "min_speedup": { "type": "number" }, "native_cpu": { "type": "boolean" }
            }),
            json!([]),
            false,
            false,
            true
        ),
        tool(
            "package",
            "Package a verified hotspot port (`dist/<ext>`) or generate an npm/wheel package for a unit crate.",
            json!({
                "root": root_p, "target": { "type": "string" }, "id": id_p, "crate": crate_p,
                "name": { "type": "string" }, "version": { "type": "string" }, "out": { "type": "string" },
                "dry_run": { "type": "boolean" }, "native_cpu": { "type": "boolean" },
                "fallback": { "type": "string" }, "bin": { "type": "array", "items": { "type": "string" } }, "original": { "type": "string" }
            }),
            json!([]),
            false,
            false,
            true
        ),
        // --- Whole-App Strangler-Fig Rewrite ---
        tool(
            "app_record",
            "Record whole-app CLI or HTTP behavior into `rust/app-cases/`.",
            json!({
                "repo": root_p, "run": cmd_p, "http": { "type": "boolean" }, "port": { "type": "integer" },
                "inputs": { "type": "string" }, "normalize": { "type": "array", "items": { "type": "string" } },
                "fresh": { "type": "array", "items": { "type": "string" } }, "float_tolerance": { "type": "number" },
                "effects": { "type": "array", "items": { "type": "string" } },
                "effect_env": { "type": "array", "items": { "type": "string" } },
                "db_reset": { "type": "string" }, "db_dump": { "type": "string" }
            }),
            json!(["repo"]),
            false,
            false,
            false
        ),
        tool(
            "app_replay",
            "Replay recorded whole-app cases against original or Rust front-door/lift binary.",
            json!({ "repo": root_p, "build": { "type": "string" }, "case": { "type": "string" }, "timeout": { "type": "integer" }, "port": { "type": "integer" } }),
            json!(["repo"]),
            true,
            false,
            true
        ),
        tool(
            "app_front_door",
            "Scaffold `rust/app`, the Rust entry point falling back to the original.",
            json!({ "repo": root_p, "library": { "type": "boolean" }, "check": { "type": "boolean" } }),
            json!(["repo"]),
            false,
            false,
            false
        ),
        tool(
            "app_wire",
            "Wire a gated unit into `rust/app` and reduce fallback rate.",
            json!({ "repo": root_p, "unit": { "type": "string" }, "commands": { "type": "array", "items": { "type": "string" } }, "routes": { "type": "array", "items": { "type": "string" } }, "skip_gate": { "type": "boolean" }, "check": { "type": "boolean" } }),
            json!(["repo", "unit"]),
            false,
            false,
            false
        ),
        tool(
            "app_lift",
            "Lift `rust/app` into a pure standalone Rust app with zero fallback.",
            json!({ "repo": root_p }),
            json!(["repo"]),
            false,
            true,
            false
        ),
        // --- Ops ---
        tool(
            "doctor",
            "Inspect Rust toolchain, project source runtimes (or all 14 with `all: true`), AI agents, and build cache sizes.",
            json!({ "root": root_p, "all": { "type": "boolean" } }),
            json!([]),
            true,
            false,
            true
        ),
        tool(
            "clean",
            "Remove shared/local Cargo build caches or `.fast/` artifacts.",
            json!({ "root": root_p, "cache": { "type": "boolean" }, "ports": { "type": "boolean" }, "rust": { "type": "boolean" }, "all": { "type": "boolean" }, "force": { "type": "boolean" }, "dry_run": { "type": "boolean" } }),
            json!([]),
            false,
            true,
            true
        ),
    ])
}

fn batch_strings(args: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["batch_list", "batch"] {
        if let Some(arr) = args[key].as_array() {
            out.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
        } else if let Some(s) = args[key].as_str() {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
    }
    out
}

fn call_with_default_root(name: &str, args: &Value, default_root: &Path) -> Result<Value> {
    let root = args["root"]
        .as_str()
        .or_else(|| args["repo"].as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_root.to_path_buf());
    let req_path = |k: &str| -> Result<PathBuf> {
        args[k]
            .as_str()
            .map(PathBuf::from)
            .with_context(|| format!("missing `{k}`"))
    };
    let opt_path = |k: &str| args[k].as_str().map(PathBuf::from);
    let strings = |k: &str| -> Vec<String> {
        args[k]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let batch_args = || -> Vec<String> { batch_strings(args) };

    Ok(match name {
        "inspect" => {
            let action = args["action"].as_str().context("missing `action`")?;
            return call_with_default_root(action, args, default_root);
        }
        "scaffold" => {
            let action = args["action"].as_str().context("missing `action`")?;
            return call_with_default_root(action, args, default_root);
        }
        "check" => {
            let action = args["action"].as_str().context("missing `action`")?;
            return call_with_default_root(action, args, default_root);
        }
        "ci" => {
            let target = args["target"]
                .as_str()
                .or_else(|| args["id"].as_str())
                .map(str::to_string);
            serde_json::to_value(synergy::run_ci(
                &root,
                synergy::CiOptions {
                    target,
                    float_tolerance: args["float_tolerance"].as_f64(),
                    fuzz: args["fuzz_cases"].as_u64().unwrap_or(0) as usize,
                    bench: args["bench"].as_bool().unwrap_or(false),
                    runs: args["runs"].as_u64().unwrap_or(5) as usize,
                    min_speedup: args["min_speedup"].as_f64(),
                    native_cpu: args["native_cpu"].as_bool().unwrap_or(false),
                },
            )?)?
        }
        "switch" => {
            let action = args["action"].as_str().context("missing `action`")?;
            return call_with_default_root(action, args, default_root);
        }
        "measure" => {
            let action = args["action"].as_str().context("missing `action`")?;
            return call_with_default_root(action, args, default_root);
        }
        "ship" => {
            let action = args["action"].as_str().context("missing `action`")?;
            return call_with_default_root(action, args, default_root);
        }
        "app" => {
            let action = args["action"].as_str().context("missing `action`")?;
            let sub = format!("app_{action}");
            return call_with_default_root(&sub, args, default_root);
        }
        "scan" => {
            let command = if let Some(cdb) = opt_path("cdb") {
                fnr_fast::cfamily::command_from_cdb(&cdb, &strings("command"))?
            } else {
                strings("command")
            };
            let duration_secs = args["duration"].as_u64();
            let load_command = args["load"].as_str().map(str::to_string);
            let out = fnr_fast::pipeline::scan_with_options(
                &root,
                command,
                fnr_fast::pipeline::ScanOptions {
                    duration_secs,
                    load_command: load_command.clone(),
                },
            )?;
            let advice =
                synergy::advise_mode_with_context(&out, duration_secs, load_command.as_deref());
            json!({ "scan": out, "advice": advice })
        }
        "port" => serde_json::to_value(synergy::port_and_enrich(
            &root,
            args["id"].as_str(),
            args["samples"].as_u64().unwrap_or(500) as usize,
        )?)?,
        "verify" => {
            let vopts = fnr_fast::pipeline::VerifyOptions {
                float_tolerance: args["float_tolerance"].as_f64(),
                known_diffs: opt_path("known_diffs"),
                min_speedup: args["min_speedup"].as_f64(),
            };
            if args["all"].as_bool().unwrap_or(false) {
                fnr_fast::pipeline::verify_all_with(&root, &vopts)?;
                json!({ "verified_all": true })
            } else {
                let target_id = args["id"].as_str().or_else(|| args["target"].as_str());
                let (id, v) = fnr_fast::pipeline::verify_with(&root, target_id, &vopts)?;
                let gate_res = if args["gate"].as_bool().unwrap_or(false) {
                    Some(synergy::gate_hotspot_port(&root, Some(&id))?)
                } else {
                    None
                };
                let fuzz_res = match args["fuzz_cases"].as_u64() {
                    Some(n) if n > 0 => Some(synergy::fuzz_hotspot_port(
                        &root,
                        Some(&id),
                        n as usize,
                        args["seed"].as_u64(),
                        args["float_tolerance"].as_f64(),
                        args["save_regressions"].as_bool().unwrap_or(false),
                    )?),
                    _ => None,
                };
                json!({
                    "id": id,
                    "verify": v,
                    "gate": gate_res,
                    "fuzz": fuzz_res,
                    "below_min_speedup": v.below_min_speedup(vopts.min_speedup),
                })
            }
        }
        "go" => {
            let command = if let Some(cdb) = opt_path("cdb") {
                fnr_fast::cfamily::command_from_cdb(&cdb, &strings("command"))?
            } else {
                strings("command")
            };
            anyhow::ensure!(!command.is_empty(), "missing `command`");
            let mode = args["mode"].as_str().unwrap_or("fast");
            if mode == "unit" {
                let lang = args
                    .get("lang")
                    .and_then(Value::as_str)
                    .map(|s| serde_json::from_value(json!(s)))
                    .transpose()?;
                serde_json::to_value(fnr_migrate::go::go(&fnr_migrate::go::Options {
                    root: &root,
                    command: &command,
                    lang,
                    agent: args["agent"].as_str(),
                    function: args["function"].as_str(),
                    count: args["samples"].as_u64().unwrap_or(200) as usize,
                    rounds: args["attempts"].as_u64().unwrap_or(3) as usize,
                })?)?
            } else {
                serde_json::to_value(synergy::go_hotspot(
                    &root,
                    synergy::GoHotspotOptions {
                        command,
                        agent: args["agent"].as_str(),
                        top: args["top"].as_u64().unwrap_or(1) as usize,
                        attempts: args["attempts"].as_u64().unwrap_or(3) as usize,
                        runs: args["runs"].as_u64().unwrap_or(5) as usize,
                        samples: args["samples"].as_u64().unwrap_or(500) as usize,
                        strict: args["strict"].as_bool().unwrap_or(false),
                        float_tolerance: args["float_tolerance"].as_f64(),
                        min_speedup: args["min_speedup"].as_f64(),
                        duration_secs: args["duration"].as_u64(),
                        load_command: args["load"].as_str(),
                        print_progress: false,
                    },
                )?)?
            }
        }
        "enable" => {
            let target = args["target"].as_str().or_else(|| args["crate"].as_str());
            let force = args["force"].as_bool().unwrap_or(false);
            let pgo = args["pgo"].as_bool().unwrap_or(false);
            let native_cpu = args["native_cpu"].as_bool().unwrap_or(false);
            match synergy::detect_target(&root, target) {
                synergy::TargetKind::MigrateCrate(crate_dir) => {
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
                            "{}/{} recorded cases match",
                            d.passed,
                            d.total
                        );
                    }
                    serde_json::to_value(fnr_migrate::enable::enable(&crate_dir)?)?
                }
                synergy::TargetKind::FastPort(id) => {
                    let enabled_id = fnr_fast::pipeline::enable_with_options(
                        &root,
                        id.as_deref(),
                        force,
                        pgo,
                        native_cpu,
                    )?;
                    let deploy = synergy::audit_deploy(&root);
                    json!({ "enabled": enabled_id, "pgo": pgo, "native_cpu": native_cpu, "deploy": deploy })
                }
            }
        }
        "disable" => {
            let target = args["target"].as_str().or_else(|| args["crate"].as_str());
            if args["all"].as_bool().unwrap_or(false) {
                let list = fnr_fast::pipeline::disable_all(&root)?;
                json!({ "disabled": list })
            } else {
                match synergy::detect_target(&root, target) {
                    synergy::TargetKind::MigrateCrate(crate_dir) => {
                        serde_json::to_value(fnr_migrate::enable::disable(&crate_dir)?)?
                    }
                    synergy::TargetKind::FastPort(id) => {
                        let d = fnr_fast::pipeline::disable(&root, id.as_deref())?;
                        json!({ "disabled": d })
                    }
                }
            }
        }
        "bench" => {
            if let Some(c) = opt_path("crate") {
                let resolve_from = opt_path("resolve_from");
                let node_args = strings("node_args");
                let rows = harness::bench(&harness::BenchOptions {
                    crate_dir: &c,
                    min_ms: args["min_ms"].as_u64().unwrap_or(200),
                    resolve_from: resolve_from.as_deref(),
                    node_args: &node_args,
                })?;
                let lang = bridge::Manifest::load(&c)
                    .map(|m| m.language)
                    .unwrap_or_default();
                let verdict = harness::verdict(&rows, lang);
                json!({ "rows": rows, "verdict": verdict })
            } else {
                let pgo = args["pgo"].as_bool().unwrap_or(false);
                let native_cpu = args["native_cpu"].as_bool().unwrap_or(false);
                if pgo || native_cpu {
                    let mut ctx = fnr_fast::state::Ctx::open(&root)?;
                    let enabled_ids: Vec<String> = ctx
                        .state
                        .ports
                        .values()
                        .filter(|p| p.enabled)
                        .map(|p| p.id.clone())
                        .collect();
                    anyhow::ensure!(
                        !enabled_ids.is_empty(),
                        "no hotspot ports are enabled yet: enable one first"
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
                        } else if let Err(log) = fnr_fast::backend::build_with_options(
                            &ctx,
                            &port,
                            fnr_fast::backend::BuildOptions {
                                profile: fnr_fast::backend::BuildProfile::Ship,
                                native_cpu,
                            },
                        )? {
                            anyhow::bail!("ship build failed for {pid}:\n{log}");
                        }
                        if let Some(p) = ctx.state.ports.get_mut(&pid) {
                            p.build_profile = Some(prof_label.to_string());
                        }
                    }
                    ctx.save()?;
                }
                let runs = args["runs"].as_u64().unwrap_or(5) as usize;
                serde_json::to_value(fnr_fast::pipeline::bench(&root, runs)?)?
            }
        }
        "report" => {
            if let Some(c) = opt_path("crate") {
                let out = opt_path("out");
                let node_args = strings("node_args");
                serde_json::to_value(fnr_migrate::card::report(&fnr_migrate::card::Options {
                    crate_dir: &c,
                    out: out.as_deref(),
                    min_ms: args["min_ms"].as_u64().unwrap_or(200),
                    node_args: &node_args,
                })?)?
            } else {
                serde_json::to_value(fnr_fast::pipeline::report(&root)?)?
            }
        }
        "status" => serde_json::to_value(synergy::unified_status(&root)?)?,
        "analyze" => {
            let lang = match args.get("lang").and_then(Value::as_str) {
                Some(s) => serde_json::from_value(json!(s)).context("`lang`")?,
                None => fnr_migrate::lang::Lang::detect(&root),
            };
            let mut rep = analyze::analyze_lang(&root, lang)?;
            if args["whole_app"].as_bool().unwrap_or(false) {
                rep.whole_app = Some(fnr_migrate::whole_app::whole_app(&root, &rep));
            }
            serde_json::to_value(rep)?
        }
        "profile" => {
            let command = strings("command");
            anyhow::ensure!(!command.is_empty(), "missing `command`");
            let lang = args
                .get("lang")
                .and_then(Value::as_str)
                .map(|s| serde_json::from_value(json!(s)))
                .transpose()?;
            serde_json::to_value(fnr_migrate::profile::profile(&root, &command, lang)?)?
        }
        "bridge" => serde_json::to_value(bridge::bridge(&bridge::Options {
            root: &root,
            unit: args["unit"].as_str().context("missing `unit`")?,
            kind: match args["kind"].as_str() {
                Some("napi") => Some(bridge::Kind::Napi),
                Some("wasm") => Some(bridge::Kind::Wasm),
                _ => None,
            },
            out: opt_path("out"),
            name: args["name"].as_str().map(str::to_string),
            force: args["force"].as_bool().unwrap_or(false),
            keep: strings("keep"),
            lang: None,
            batch: batch_args(),
            as_json: strings("as_json"),
            allocator: bridge::Allocator::default(),
        })?)?,
        "refresh" => {
            let c = req_path("crate")?;
            let batch = batch_args();
            serde_json::to_value(if batch.is_empty() {
                bridge::refresh(&c)?
            } else {
                bridge::add_batches(&c, &batch)?
            })?
        }
        "record" => {
            let c = req_path("crate")?;
            let profile = args["profile"].as_str().unwrap_or("host");
            if profile == "host" && !args["device"].is_null() {
                anyhow::bail!("`device` needs profile `android`");
            }
            let mut inputs = opt_path("inputs");
            let test_cmd = strings("test_command");
            if !test_cmd.is_empty() {
                let cap = fnr_migrate::from_tests::capture(&c, &test_cmd)?;
                inputs = Some(fnr_migrate::from_tests::with_own_inputs(
                    &c,
                    inputs.as_deref(),
                    &cap.inputs,
                )?);
            }
            if let Some(t) = args["float_tolerance"].as_f64() {
                let mut m = bridge::Manifest::load(&c)?;
                m.float_tolerance = (fnr_migrate::tolerance::check(t)? > 0.0).then_some(t);
                m.save(&c)?;
            }
            let module = opt_path("module");
            let resolve_from = opt_path("resolve_from");
            let node_args = strings("node_args");
            let opts = harness::RecordOptions {
                crate_dir: &c,
                count: args["count"].as_u64().unwrap_or(200) as usize,
                inputs: inputs.as_deref(),
                module: module.as_deref(),
                resolve_from: resolve_from.as_deref(),
                node_args: &node_args,
            };
            serde_json::to_value(match profile {
                "host" => harness::record(&opts)?,
                "android" => harness::record_android(&opts, args["device"].as_str())?,
                other => anyhow::bail!("unknown record profile `{other}`: use host or android"),
            })?
        }
        "build" => {
            if let Some(c) = opt_path("crate") {
                match args["profile"].as_str().unwrap_or("host") {
                    "android" => {
                        let api = args["api"]
                            .as_u64()
                            .and_then(|v| u32::try_from(v).ok())
                            .unwrap_or(fnr_migrate::android::DEFAULT_API);
                        serde_json::to_value(harness::build_android(
                            &c,
                            &fnr_migrate::android::Options {
                                abis: strings("abi"),
                                api,
                                ndk: opt_path("ndk"),
                                out: opt_path("jni_libs"),
                            },
                        )?)?
                    }
                    "host" => json!({ "artifact": harness::build(&c)? }),
                    other => anyhow::bail!("unknown build profile `{other}`: use host or android"),
                }
            } else if args["deploy"].as_bool() == Some(true) {
                let native_cpu = args["native_cpu"].as_bool().unwrap_or(false);
                let built = fnr_fast::pipeline::build_deploy(&root, native_cpu)?;
                json!({ "built_deploy": built })
            } else {
                fnr_fast::pipeline::build_all(&root)?;
                json!({ "built_all_fast": true })
            }
        }
        "diff" => {
            let c = req_path("crate")?;
            let module = opt_path("module");
            let node_args = strings("node_args");
            let opts = harness::DiffOptions {
                crate_dir: &c,
                module: module.as_deref(),
                ignore_key_order: args["ignore_key_order"].as_bool().unwrap_or(false),
                node_args: &node_args,
                float_tolerance: args["float_tolerance"].as_f64(),
            };
            serde_json::to_value(match args["profile"].as_str().unwrap_or("host") {
                "host" => harness::diff(&opts)?,
                "android" => harness::diff_android(
                    &opts,
                    args["device"].as_str(),
                    args["jni_libs"].as_str().map(Path::new),
                )?,
                other => anyhow::bail!("unknown diff profile `{other}`: use host or android"),
            })?
        }
        "fuzz" => {
            let target = args["target"]
                .as_str()
                .or_else(|| args["crate"].as_str())
                .or_else(|| args["id"].as_str());
            match synergy::detect_target(&root, target) {
                synergy::TargetKind::MigrateCrate(crate_dir) => {
                    let time = args["time"]
                        .as_f64()
                        .and_then(fnr_migrate::process::seconds);
                    let keep = match args["keep"].as_str() {
                        None => None,
                        Some("mismatches") => Some(fuzz::Keep::Mismatches),
                        Some("all") => Some(fuzz::Keep::All),
                        Some(other) => {
                            anyhow::bail!("unknown `keep` `{other}`: use mismatches or all")
                        }
                    };
                    let resolve_from = opt_path("resolve_from");
                    let node_args = strings("node_args");
                    serde_json::to_value(fuzz::fuzz(&fuzz::FuzzOptions {
                        crate_dir: &crate_dir,
                        cases: args["cases"]
                            .as_u64()
                            .or_else(|| args["fuzz_cases"].as_u64())
                            .unwrap_or(200) as usize,
                        seed: args["seed"].as_u64(),
                        time,
                        keep,
                        resolve_from: resolve_from.as_deref(),
                        node_args: &node_args,
                    })?)?
                }
                synergy::TargetKind::FastPort(id) => {
                    serde_json::to_value(synergy::fuzz_hotspot_port(
                        &root,
                        id.as_deref(),
                        args["cases"]
                            .as_u64()
                            .or_else(|| args["fuzz_cases"].as_u64())
                            .unwrap_or(200) as usize,
                        args["seed"].as_u64(),
                        args["float_tolerance"].as_f64(),
                        args["save_regressions"].as_bool().unwrap_or(false),
                    )?)?
                }
            }
        }
        "shadow" => serde_json::to_value(shadow::shadow(
            &req_path("crate")?,
            opt_path("out").as_deref(),
        )?)?,
        "gate" => {
            let target = args["target"].as_str().or_else(|| args["crate"].as_str());
            let node_args = strings("node_args");
            match synergy::detect_target(&root, target) {
                synergy::TargetKind::MigrateCrate(crate_dir) => {
                    serde_json::to_value(gate::gate(&crate_dir, &node_args)?)?
                }
                synergy::TargetKind::FastPort(id) => {
                    serde_json::to_value(synergy::gate_hotspot_port(&root, id.as_deref())?)?
                }
            }
        }
        "escalate" => serde_json::to_value(synergy::escalate_hotspot(
            &root,
            args["id"].as_str(),
            args["batch"].as_str(),
        )?)?,
        "map" => {
            let crates: Vec<PathBuf> = strings("crates").into_iter().map(Into::into).collect();
            serde_json::to_value(port_map::map(&root, &crates)?)?
        }
        "deploy" => serde_json::to_value(synergy::generate_deploy_plan(
            &root,
            args["target"].as_str(),
            args["write"].as_bool().unwrap_or(false),
            args["force"].as_bool().unwrap_or(false),
        )?)?,
        "package" => {
            let target = args["target"]
                .as_str()
                .or_else(|| args["crate"].as_str())
                .or_else(|| args["id"].as_str());
            match synergy::detect_target(&root, target) {
                synergy::TargetKind::MigrateCrate(crate_dir) => serde_json::to_value(
                    fnr_migrate::package::package(&fnr_migrate::package::Options {
                        crate_dir: &crate_dir,
                        name: args["name"].as_str().map(str::to_string),
                        version: args["version"].as_str().map(str::to_string),
                        out: opt_path("out"),
                        dry_run: args["dry_run"].as_bool().unwrap_or(false),
                        fallback: args["fallback"].as_str().map(str::to_string),
                        bin: strings("bin"),
                        original: args["original"].as_str().map(str::to_string),
                    })?,
                )?,
                synergy::TargetKind::FastPort(id) => {
                    serde_json::to_value(synergy::package_hotspot_port_with(
                        &root,
                        id.as_deref(),
                        opt_path("out").as_deref(),
                        args["dry_run"].as_bool().unwrap_or(false),
                        args["native_cpu"].as_bool().unwrap_or(false),
                    )?)?
                }
            }
        }
        "app_record" => {
            let eff = effects::EffectsConfig::from_args(
                &strings("effects"),
                &strings("effect_env"),
                args["db_reset"].as_str(),
                args["db_dump"].as_str(),
            )?;
            serde_json::to_value(app::record(&app::RecordOptions {
                repo: &root,
                run: &strings("run"),
                inputs: opt_path("inputs").as_deref(),
                normalize: &strings("normalize"),
                name: None,
                traps: true,
                timeout_secs: None,
                http: args["http"].as_bool().unwrap_or(false),
                port: args["port"].as_u64().and_then(|p| u16::try_from(p).ok()),
                server_args: &[],
                fresh: &strings("fresh"),
                pace_ms: None,
                float_tolerance: args["float_tolerance"].as_f64(),
                float_files: &[],
                effects: eff,
            })?)?
        }
        "app_replay" => serde_json::to_value(app::replay(&app::ReplayOptions {
            repo: &root,
            build: args["build"].as_str(),
            timeout_secs: args["timeout"].as_u64(),
            port: args["port"].as_u64().and_then(|p| u16::try_from(p).ok()),
            case: args["case"].as_str(),
        })?)?,
        "app_front_door" => {
            serde_json::to_value(front_door::front_door(&front_door::FrontDoorOptions {
                repo: &root,
                library: args["library"].as_bool().unwrap_or(false),
                check: args["check"].as_bool().unwrap_or(true),
            })?)?
        }
        "app_wire" => serde_json::to_value(front_door::wire(&front_door::WireOptions {
            repo: &root,
            unit: args["unit"].as_str().context("missing `unit`")?,
            commands: &strings("commands"),
            routes: &strings("routes"),
            skip_gate: args["skip_gate"].as_bool().unwrap_or(false),
            check: args["check"].as_bool().unwrap_or(true),
        })?)?,
        "app_lift" => serde_json::to_value(lift::lift(&lift::LiftOptions { repo: &root })?)?,
        "doctor" => {
            let all = args["all"].as_bool().unwrap_or(false);
            serde_json::to_value(doctor::run_with_options(&root, all))?
        }
        "clean" => serde_json::to_value(clean::clean(
            &root,
            &clean::CleanOptions {
                cache: args["cache"].as_bool().unwrap_or(false),
                ports: args["ports"].as_bool().unwrap_or(false),
                rust: args["rust"].as_bool().unwrap_or(false),
                all: args["all"].as_bool().unwrap_or(false),
                force: args["force"].as_bool().unwrap_or(false),
                dry_run: args["dry_run"].as_bool().unwrap_or(false),
            },
        )?)?,
        #[cfg(test)]
        "test_sleep" => {
            let ms = args["ms"].as_u64().unwrap_or(0);
            if let Some(marker) = args["marker"].as_str() {
                let path = root.join(marker);
                let prev = std::fs::read_to_string(&path).unwrap_or_default();
                let tag = args["tag"].as_str().unwrap_or("x");
                std::fs::write(&path, format!("{prev}{tag}:start\n"))?;
                std::thread::sleep(std::time::Duration::from_millis(ms));
                let cur = std::fs::read_to_string(&path).unwrap_or_default();
                std::fs::write(&path, format!("{cur}{tag}:end\n"))?;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            json!({ "slept": true })
        }
        _ => anyhow::bail!("unknown tool `{name}`"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_handshake_lists_tools_prompts_resources_and_doctor() {
        let init = handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "fast-n-rust");
        assert!(init["result"]["capabilities"]["resources"].is_object());

        let list_compact =
            handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).unwrap();
        let arr_compact = list_compact["result"]["tools"].as_array().unwrap();
        assert_eq!(arr_compact.len(), 7);
        for t in arr_compact {
            assert!(t["annotations"].is_object());
        }

        let list_full = handle_with_mode(
            &json!({ "jsonrpc": "2.0", "id": 20, "method": "tools/list" }),
            ToolMode::Full,
        )
        .unwrap();
        assert_eq!(list_full["result"]["tools"].as_array().unwrap().len(), 30);

        let inspect_doc = handle(&json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": { "name": "inspect", "arguments": { "action": "doctor", "root": "." } }
        }))
        .unwrap();
        assert_eq!(inspect_doc["result"]["isError"], false);
        assert!(inspect_doc["result"]["structuredContent"]["checks"].is_array());

        let prompts =
            handle(&json!({ "jsonrpc": "2.0", "id": 3, "method": "prompts/list" })).unwrap();
        assert_eq!(prompts["result"]["prompts"].as_array().unwrap().len(), 4);

        let resources =
            handle(&json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" })).unwrap();
        assert_eq!(
            resources["result"]["resources"].as_array().unwrap().len(),
            5
        );
        let read_res = handle(&json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "resources/read",
            "params": { "uri": "fnr://guide/skill" }
        }))
        .unwrap();
        let skill_txt = read_res["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(skill_txt.contains("fast-n-rust"));

        let read_unit = handle(&json!({
            "jsonrpc": "2.0",
            "id": 55,
            "method": "resources/read",
            "params": { "uri": "fnr://guide/unit-porting" }
        }))
        .unwrap();
        let unit_txt = read_unit["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(unit_txt.contains("FnIndex") && unit_txt.contains("trend.stalled"));

        let doc = handle(&json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": { "name": "doctor", "arguments": { "root": "." } }
        }))
        .unwrap();
        assert_eq!(doc["result"]["isError"], false);
        assert!(doc["result"]["structuredContent"]["checks"].is_array());
    }

    #[test]
    fn serve_io_answers_ping_concurrently_and_cancels_int_and_string_ids() {
        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let reqs = [
            json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"test_sleep","arguments":{"ms":120}}}),
            json!({"jsonrpc":"2.0","id":"str-11","method":"tools/call","params":{"name":"test_sleep","arguments":{"ms":150}}}),
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":10}}),
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"str-11"}}),
            json!({"jsonrpc":"2.0","id":12,"method":"ping"}),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";

        let buf = Arc::new(Mutex::new(Vec::new()));
        serve_io(BufReader::new(reqs.as_bytes()), SharedBuf(Arc::clone(&buf))).unwrap();
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let ids: Vec<Value> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["id"].clone())
            .collect();
        assert_eq!(
            ids,
            vec![json!(12)],
            "both cancelled calls must be suppressed: {out}"
        );
    }

    #[test]
    fn initialize_root_uri_propagates_to_resources_and_mutating_calls_serialize() {
        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let tmp = std::env::temp_dir().join(format!("fnr-mcp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".fast")).unwrap();
        std::fs::write(
            tmp.join(".fast/state.json"),
            r#"{"lang":"python","wall":1.25,"base":"app","hotspots":[],"ports":{}}"#,
        )
        .unwrap();

        let uri = format!("file://{}", tmp.display());
        let reqs = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"rootUri":uri}}),
            json!({"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"fnr://project/status"}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"test_sleep","arguments":{"mutate":true,"ms":40,"marker":"order.log","tag":"A"}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"test_sleep","arguments":{"mutate":true,"ms":40,"marker":"order.log","tag":"B"}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"test_sleep","arguments":{"mutate":true,"ms":40,"marker":"order.log","tag":"C"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}}),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";

        let buf = Arc::new(Mutex::new(Vec::new()));
        serve_io(BufReader::new(reqs.as_bytes()), SharedBuf(Arc::clone(&buf))).unwrap();
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let resps: Vec<Value> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        let status_resp = resps.iter().find(|r| r["id"] == 2).unwrap();
        let status_text = status_resp["result"]["contents"][0]["text"]
            .as_str()
            .unwrap();
        assert!(
            status_text.contains("\"wall\": 1.25"),
            "resource did not use initialized rootUri: {status_text}"
        );

        let log = std::fs::read_to_string(tmp.join("order.log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        // Mutating calls A and B must not interleave (`start`/`end` pairs stay atomic),
        // and cancelled queued call C must be skipped.
        assert_eq!(lines.len(), 4, "expected A and B only, got: {lines:?}");
        assert_eq!(lines[0].split(':').next(), lines[1].split(':').next());
        assert_eq!(lines[2].split(':').next(), lines[3].split(':').next());
        assert!(lines[0].ends_with(":start") && lines[1].ends_with(":end"));
        assert!(lines[2].ends_with(":start") && lines[3].ends_with(":end"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn serve_io_answers_ping_and_cancels_when_worker_pool_is_full() {
        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut reqs = Vec::new();
        for i in 1..=MAX_CONCURRENT_WORKERS {
            reqs.push(json!({
                "jsonrpc": "2.0",
                "id": i,
                "method": "tools/call",
                "params": { "name": "test_sleep", "arguments": { "ms": 120 } }
            }));
        }
        reqs.push(json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": { "name": "test_sleep", "arguments": { "ms": 80 } }
        }));
        reqs.push(json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": 99 }
        }));
        reqs.push(json!({ "jsonrpc": "2.0", "id": 100, "method": "ping" }));

        let wire = reqs
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let buf = Arc::new(Mutex::new(Vec::new()));
        serve_io(BufReader::new(wire.as_bytes()), SharedBuf(Arc::clone(&buf))).unwrap();
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let ids: Vec<Value> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["id"].clone())
            .collect();
        assert!(
            ids.contains(&json!(100)),
            "ping must be answered even when worker pool is saturated: {out}"
        );
        assert!(
            !ids.contains(&json!(99)),
            "queued 9th worker cancelled before execution must be suppressed: {out}"
        );
    }

    #[test]
    fn windows_localhost_file_uri_and_schemas_and_batch_strings() {
        assert_eq!(strip_file_uri("file://localhost/C:/repo", true), "C:/repo");
        assert_eq!(strip_file_uri("file:///C:/repo", true), "C:/repo");
        assert_eq!(
            strip_file_uri("file://localhost/tmp/repo", false),
            "/tmp/repo"
        );

        assert_eq!(
            batch_strings(&json!({ "batch_list": ["dedupe:names"] })),
            vec!["dedupe:names"]
        );
        assert_eq!(
            batch_strings(&json!({ "batch": "dedupe:names" })),
            vec!["dedupe:names"]
        );
        assert_eq!(
            batch_strings(&json!({ "batch": ["a:x", "b:y"] })),
            vec!["a:x", "b:y"]
        );

        let compact = compact_tools();
        let compact_arr = compact.as_array().unwrap();
        let inspect = compact_arr.iter().find(|t| t["name"] == "inspect").unwrap();
        assert_eq!(inspect["annotations"]["readOnlyHint"], false);
        let switch = compact_arr.iter().find(|t| t["name"] == "switch").unwrap();
        assert!(switch["inputSchema"]["properties"]["cache"].is_object());
        assert!(switch["inputSchema"]["properties"]["ports"].is_object());
        assert!(switch["inputSchema"]["properties"]["rust"].is_object());

        let full = tools();
        let full_arr = full.as_array().unwrap();
        assert!(full_arr.iter().any(|t| t["name"] == "ci"));
        let build = full_arr.iter().find(|t| t["name"] == "build").unwrap();
        assert!(build["inputSchema"]["properties"]["deploy"].is_object());
        assert!(build["inputSchema"]["properties"]["native_cpu"].is_object());
        let pkg = full_arr.iter().find(|t| t["name"] == "package").unwrap();
        assert_eq!(pkg["inputSchema"]["required"], json!([]));
        assert!(pkg["inputSchema"]["properties"]["target"].is_object());
        assert!(pkg["inputSchema"]["properties"]["native_cpu"].is_object());
    }
}
