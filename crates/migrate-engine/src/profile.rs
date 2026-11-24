//! `profile`: run a real workload under the language's own profiler and say what to port.
//!
//! Porting a leaf function rarely pays: each call into Rust costs more than the leaf's work.
//! What pays is moving a hot loop with its callers, keeping data on the Rust side, and using all
//! cores. This step finds those: hot functions in the repository, by inclusive time, with the
//! callers that spend it, call counts where the profiler has them, and a recommendation each.

use crate::walk::Canonical;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::lang::Lang;

pub const SCHEMA: &str = "rust-anything/profile@1";

#[derive(Debug, Serialize)]
pub struct Caller {
    pub name: String,
    pub file: String,
    pub line: u32,
    /// Time of the callee spent under this caller.
    pub ms: f64,
    /// Calls from this caller, when the profiler counts calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calls: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Hot {
    pub name: String,
    pub file: String,
    pub line: u32,
    /// The analyzed unit holding the function, and whether the unit exports it.
    pub unit: Option<String>,
    pub exported: bool,
    pub self_ms: f64,
    pub total_ms: f64,
    pub total_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calls: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ns_per_call: Option<f64>,
    pub callers: Vec<Caller>,
    pub advice: String,
    /// The unit may move to Rust (not marked keep or blocked).
    pub portable: bool,
    /// Why `bridge` keeps this exported function in the source language (a callback, a RegExp,
    /// a generator...), judged by the same rules `bridge` applies. `go` never picks it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub js_only: Option<String>,
    /// Parameters of the exported function.
    pub params: Vec<String>,
    /// `function:param` for `bridge --batch` when the advice is to batch it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<String>,
    /// Share of this function's time spent waiting on child processes it started, itself or
    /// through the functions it calls. `go` passes over a function that mostly waits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_pct: Option<f64>,
}

impl Hot {
    /// Most of its time goes to waiting on child processes: the same work in Rust still waits.
    pub fn mostly_waits(&self) -> bool {
        self.child_pct.is_some_and(|p| p >= 50.0)
    }
}

/// Time a function in user code (the repository or the workload script) spent waiting on child
/// processes it started (`execSync`, `subprocess.run`, `os.system`...), itself or through a
/// library.
#[derive(Debug, Clone, Serialize)]
pub struct ChildProcess {
    /// The program, when the call passes it as a literal (`sleep` for `execSync("sleep 0.2")`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub file: String,
    /// The call's line when the function makes one such call, else the function's line.
    pub line: u32,
    /// The function that makes the call.
    pub function: String,
    pub ms: f64,
    /// Percent of the whole run.
    pub share: f64,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub language: Lang,
    pub command: Vec<String>,
    pub total_ms: f64,
    /// Share of the run spent in the repository's own code (the rest: runtime, libraries).
    pub repo_pct: f64,
    /// Every function with at least 1% of the run, hottest first; the first `SHOWN` are
    /// serialized and printed, and `go` picks from all of them.
    #[serde(serialize_with = "shown")]
    pub hot: Vec<Hot>,
    /// Functions the repository already compiles to machine code (numba, Cython, JAX).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub compiled: Vec<Compiled>,
    /// Percent of the run spent waiting on child processes; 0 below 1%.
    #[serde(skip_serializing_if = "is_zero")]
    pub child_pct: f64,
    /// Where the run waits on child processes, longest first (each at least 1% of the run).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub child_processes: Vec<ChildProcess>,
}

/// Hot functions in the report and in `--json`.
pub const SHOWN: usize = 25;

fn shown<S: serde::Serializer>(hot: &[Hot], s: S) -> std::result::Result<S::Ok, S::Error> {
    s.collect_seq(hot.iter().take(SHOWN))
}

fn is_zero(x: &f64) -> bool {
    *x == 0.0
}

/// What to do instead of porting when the time goes to child processes.
pub const CHILD_ADVICE: &str = "Porting code will not speed this up: keep one long-lived process \
                                open, batch the commands, or call the API directly.";

impl Report {
    /// "38% of the run waits on child processes (`sleep` from `lib.js:6`)." With several,
    /// "(mostly `sleep` from ...)".
    pub fn child_summary(&self) -> Option<String> {
        if self.child_pct < 1.0 {
            return None;
        }
        let Some(top) = self.child_processes.first() else {
            return Some(format!(
                "{:.0}% of the run waits on child processes.",
                self.child_pct
            ));
        };
        let from = match &top.command {
            Some(c) => format!("`{c}` from `{}:{}`", top.file, top.line),
            None => format!("from `{}` at `{}:{}`", top.function, top.file, top.line),
        };
        let mostly = if self.child_processes.len() > 1 {
            "mostly "
        } else {
            ""
        };
        Some(format!(
            "{:.0}% of the run waits on child processes ({mostly}{from}).",
            self.child_pct
        ))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Compiled {
    pub name: String,
    pub file: String,
    pub line: u32,
    pub by: &'static str,
}

/// Every Python function in the repository a decorator compiles.
fn compiled_in(root: &Path, lang: Lang) -> Vec<Compiled> {
    if lang != Lang::Python {
        return Vec::new();
    }
    let Ok(ws) = crate::lang::python::discover(root) else {
        return Vec::new();
    };
    ws.files
        .iter()
        .filter(|f| f.kind == crate::ts::discover::FileKind::Source)
        .flat_map(|f| {
            let text = std::fs::read_to_string(root.join(&f.path)).unwrap_or_default();
            crate::risks::python_compiled(&text)
                .into_iter()
                .map(|(name, line, by)| Compiled {
                    name,
                    file: f.path.clone(),
                    line,
                    by,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The body of the Python function `name` defined at or just after `line`.
fn python_body(text: &str, name: &str, line: u32) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = (line as usize).saturating_sub(1);
    let def = (start..lines.len().min(start + 20)).find(|&i| {
        let t = lines[i].trim_start();
        t.strip_prefix("def ")
            .or_else(|| t.strip_prefix("async def "))
            .is_some_and(|d| d.starts_with(name) && d[name.len()..].trim_start().starts_with('('))
    })?;
    let indent = |l: &str| l.len() - l.trim_start().len();
    let base = indent(lines[def]);
    let body: Vec<&str> = lines[def + 1..]
        .iter()
        .take_while(|l| l.trim().is_empty() || indent(l) > base)
        .copied()
        .collect();
    Some(body.join("\n"))
}

fn calls(body: &str, name: &str) -> bool {
    body.match_indices(&format!("{name}(")).any(|(i, _)| {
        body[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
    })
}

/// Advice for a hot function that is compiled already, or that calls compiled code.
fn compiled_advice(root: &Path, h: &Hot, compiled: &[Compiled]) -> Option<String> {
    if let Some(c) = compiled
        .iter()
        .find(|c| c.file == h.file && c.name == h.name)
    {
        return Some(format!(
            "Already compiled by {} (`{}:{}`): a Rust port is unlikely to run it much faster. \
             Port it for a pure Rust app, not for speed.",
            c.by, c.file, c.line
        ));
    }
    let text = std::fs::read_to_string(root.join(&h.file)).ok()?;
    let body = python_body(&text, &h.name, h.line)?;
    let called: Vec<&Compiled> = compiled.iter().filter(|c| calls(&body, &c.name)).collect();
    let first = called.first()?;
    let names: Vec<String> = called.iter().map(|c| format!("`{}`", c.name)).collect();
    Some(format!(
        "{} It calls {} compiled by {} (`{}:{}`): the profiler counts that compiled time here, so \
         porting this may gain little; port the Python around those calls, or measure without them.",
        h.advice,
        names.join(", "),
        first.by,
        first.file,
        first.line
    ))
}

/// Loads the project-wide types `lang::exports` reads on this thread (C typedefs, C# shapes), as
/// `bridge` does before it judges a unit.
pub fn use_project_types(root: &Path, lang: Lang) {
    match lang {
        Lang::C => crate::lang::c::use_typedefs(root),
        Lang::Csharp => crate::lang::csharp::use_shapes(root),
        _ => {}
    }
}

/// The exported functions of `file`, read as `bridge` reads them (for TS/JS a sibling `.d.ts` and
/// type aliases applied), each with the reason `bridge` would keep it in the source language. Call
/// [`use_project_types`] first, as `bridge` does.
pub fn exports_judged(
    root: &Path,
    lang: Lang,
    file: &str,
    text: &str,
) -> Vec<(crate::ts::exports::ExportedFn, Option<String>)> {
    if lang == Lang::Ts {
        let types = crate::bridge::types_in_scope(root, file, text);
        crate::bridge::ts_functions(root, file, text, &types)
            .into_iter()
            .map(|f| {
                let reason = crate::bridge::js_only_reason(&f, &types);
                (f, reason)
            })
            .collect()
    } else {
        crate::lang::exports(lang, file, text)
            .0
            .into_iter()
            .map(|f| {
                let reason = crate::bridge::js_only_reason(&f, &[]);
                (f, reason)
            })
            .collect()
    }
}

/// One function's numbers before advice: key is (file, name, line).
#[derive(Default)]
struct Acc {
    self_ms: f64,
    total_ms: f64,
    calls: Option<u64>,
    callers: HashMap<(String, String, u32), (f64, Option<u64>)>,
    /// Waiting on child processes this function started itself.
    spawn_ms: f64,
    /// Waiting on child processes under this function, its callees' included.
    waits_ms: f64,
}

type Key = (String, String, u32);

pub fn profile(root: &Path, command: &[String], lang: Option<Lang>) -> Result<Report> {
    if command.is_empty() {
        bail!(
            "give the workload to run after `--`, e.g. `rust-anything profile . -- node bench.mjs`"
        );
    }
    let root = root
        .canonical()
        .with_context(|| format!("resolving {}", root.display()))?;
    let lang = lang.unwrap_or_else(|| Lang::detect(&root));
    let work = std::env::temp_dir().join(format!("rust-anything-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let no_waits = |(functions, total_ms)| (functions, 0.0, total_ms);
    let (functions, unowned_ms, total_ms) = match lang {
        Lang::Ts => node(&root, command, &work)?,
        Lang::Python => python(&root, command, &work)?,
        Lang::Go => no_waits(go(&root, command, &work)?),
        Lang::Java => no_waits(java(&root, command, &work)?),
        Lang::Kotlin => bail!(
            "`profile` does not run Kotlin workloads yet; use JFR (`jfr print --stack-depth 256`) \
             and port the hot loops it shows"
        ),
        Lang::Ruby => no_waits(ruby(&root, command, &work)?),
        Lang::C => bail!(
            "`profile` runs TS/JS, Python, Go, Java and Ruby workloads; for C, use `perf record -g` \
             and port the hot loops it shows"
        ),
        Lang::Php => bail!(
            "`profile` does not run PHP workloads yet; use SPX or Xdebug's profiler and port the \
             hot loops it shows"
        ),
        Lang::Csharp => bail!(
            "`profile` does not run C# workloads yet; use `dotnet-trace` or your IDE's profiler and \
             port the hot loops it shows"
        ),
        Lang::Powershell => bail!(
            "`profile` does not run PowerShell workloads yet. Time one `pwsh` start first \
             (`Measure-Command {{ pwsh -NoProfile -c 1 }}`): when a tool starts a shell per \
             command, start-up is the slow part and no port fixes it; keep one shell open. For a \
             script whose own logic is slow, time its functions with `Measure-Command` and port \
             those"
        ),
    };
    let _ = std::fs::remove_dir_all(&work);
    Ok(report(
        &root, lang, command, functions, unowned_ms, total_ms,
    ))
}

/// A frame's file: repository-relative, or absolute for user code elsewhere (a workload
/// script); `None` for the runtime and installed libraries.
fn relative(root: &Path, file: &str) -> Option<String> {
    let decoded;
    let path = if let Some(url_path) = file.strip_prefix("file://") {
        decoded = percent_encoding::percent_decode_str(url_path).decode_utf8_lossy();
        decoded.as_ref()
    } else {
        file
    };
    if !path.starts_with('/') && !path.contains(":\\") {
        let rel = path.trim_start_matches("./");
        return (!rel.starts_with(['<', '~']) && !rel.contains(':') && root.join(rel).is_file())
            .then(|| rel.replace('\\', "/")); // node:internal/..., <frozen ...>, ~
    }
    // Installed libraries and runtimes: npm, Python, Ruby gems and stdlib, Go modules and GOROOT.
    if path.split(['/', '\\']).any(|p| {
        matches!(
            p,
            "node_modules"
                | "site-packages"
                | "dist-packages"
                | ".venv"
                | "venv"
                | "gems"
                | "vendor"
        )
    }) || python_runtime(path)
        || ["/lib/ruby/", "/pkg/mod/"].iter().any(|d| path.contains(d))
        || goroot().is_some_and(|g| path.starts_with(g))
    {
        return None;
    }
    let p = PathBuf::from(path);
    Some(match p.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string(),
    })
}

/// A Python installation's library: a `lib` or `lib64` folder holding `python3` or `python3.N`.
fn python_runtime(path: &str) -> bool {
    let parts: Vec<&str> = path.split(['/', '\\']).collect();
    parts.windows(2).any(|w| {
        matches!(w[0], "lib" | "lib64")
            && w[1].strip_prefix("python3").is_some_and(|v| {
                v.is_empty()
                    || v.strip_prefix('.')
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
    })
}

/// Go's own sources, which are not user code.
fn goroot() -> Option<&'static str> {
    static GOROOT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    GOROOT
        .get_or_init(|| {
            let out = crate::process::output(
                Command::new("go").args(["env", "GOROOT"]),
                crate::process::QUICK,
            )
            .ok()?;
            let root = String::from_utf8(out.stdout).ok()?.trim().to_string();
            (!root.is_empty()).then(|| format!("{root}/"))
        })
        .as_deref()
}

/// Whether a frame file is inside the repository (relative).
fn in_repo(file: &str) -> bool {
    !file.starts_with('/')
}

// ---- node --cpu-prof ------------------------------------------------------------------------

/// Functions, ms of waits on child processes no user function owns, and the run's ms.
type Waited = (HashMap<Key, Acc>, f64, f64);

fn node(root: &Path, command: &[String], work: &Path) -> Result<Waited> {
    let dir = work.join("node");
    std::fs::create_dir_all(&dir)?;
    let mut options = std::env::var("NODE_OPTIONS").unwrap_or_default();
    let dir_option = dir
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    options.push_str(&format!(" --cpu-prof --cpu-prof-dir=\"{dir_option}\""));
    let status = crate::process::status(
        Command::new(&command[0])
            .stdout(std::io::stderr())
            .args(&command[1..])
            .current_dir(root)
            .env("NODE_OPTIONS", options.trim()),
        crate::process::TESTS,
    )
    .with_context(|| format!("running {}", command[0]))?;
    if !status.success() {
        bail!("the workload failed ({status})");
    }
    let mut functions: HashMap<Key, Acc> = HashMap::new();
    let mut total = 0.0;
    let mut unowned = 0.0;
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let profile: Value = serde_json::from_str(&std::fs::read_to_string(entry.path())?)?;
        total += merge_cpuprofile(root, &profile, &mut functions, &mut unowned);
    }
    if total == 0.0 {
        bail!("no CPU profile was written: is the workload a node process?");
    }
    Ok((functions, unowned, total))
}

/// A `.cpuprofile` node: its frame when it is user code, its own time, its children.
struct Frame {
    key: Option<Key>,
    self_ms: f64,
    children: Vec<u64>,
    /// `execSync` and its kin: the thread blocks until the child process exits.
    spawn: bool,
}

/// `node:child_process` functions that block until the child exits.
const NODE_SPAWNS: [&str; 3] = ["execSync", "execFileSync", "spawnSync"];

/// Adds one `.cpuprofile` to `functions`, and to `unowned` the waits on child processes with no
/// user frame above them; returns its duration in ms.
fn merge_cpuprofile(
    root: &Path,
    profile: &Value,
    functions: &mut HashMap<Key, Acc>,
    unowned: &mut f64,
) -> f64 {
    let Some(nodes) = profile["nodes"].as_array() else {
        return 0.0;
    };
    let samples = profile["samples"].as_array().map_or(0, Vec::len).max(1);
    let duration_ms = (profile["endTime"].as_f64().unwrap_or(0.0)
        - profile["startTime"].as_f64().unwrap_or(0.0))
        / 1000.0;
    let per_sample = duration_ms / samples as f64;
    let mut by_id: HashMap<u64, Frame> = HashMap::new();
    let mut is_child = HashSet::new();
    for n in nodes {
        let id = n["id"].as_u64().unwrap_or(0);
        let frame = &n["callFrame"];
        let name = frame["functionName"].as_str().unwrap_or("").to_string();
        let url = frame["url"].as_str().unwrap_or("");
        let line = u32::try_from(frame["lineNumber"].as_i64().unwrap_or(0) + 1).unwrap_or(0);
        let key = relative(root, url).map(|file| {
            let shown = if name.is_empty() {
                format!("(anonymous):{line}")
            } else {
                name.clone()
            };
            (file, shown, line)
        });
        let children: Vec<u64> = n["children"]
            .as_array()
            .map_or_else(Vec::new, |c| c.iter().filter_map(Value::as_u64).collect());
        is_child.extend(children.iter().copied());
        let self_ms = n["hitCount"].as_f64().unwrap_or(0.0) * per_sample;
        let spawn = url == "node:child_process" && NODE_SPAWNS.contains(&name.as_str());
        by_id.insert(
            id,
            Frame {
                key,
                self_ms,
                children,
                spawn,
            },
        );
    }
    // Inclusive time per node, depth first.
    fn total(id: u64, by_id: &HashMap<u64, Frame>, memo: &mut HashMap<u64, f64>) -> f64 {
        if let Some(t) = memo.get(&id) {
            return *t;
        }
        let Some(frame) = by_id.get(&id) else {
            return 0.0;
        };
        let t = frame.self_ms
            + frame
                .children
                .iter()
                .map(|c| total(*c, by_id, memo))
                .sum::<f64>();
        memo.insert(id, t);
        t
    }
    let mut memo = HashMap::new();
    // Walk from the roots, tracking the repo frames on the stack: recursion counts once, and a
    // callee's caller is the nearest *named* repo frame above it (callbacks are skipped).
    let roots: Vec<u64> = by_id
        .keys()
        .filter(|id| !is_child.contains(id))
        .copied()
        .collect();
    let mut stack: Vec<(u64, Vec<Key>)> = roots.into_iter().map(|r| (r, Vec::new())).collect();
    while let Some((id, active)) = stack.pop() {
        let Some(Frame {
            key,
            self_ms,
            children,
            spawn,
        }) = by_id.get(&id)
        else {
            continue;
        };
        if *spawn {
            // The outermost blocking call: everything under it is the wait (`execSync` calls
            // `spawnSync`). It goes to the nearest named user frame, as callers do, and counts
            // once for each frame on the stack.
            let waited = total(id, &by_id, &mut memo);
            if let Some(caller) = active
                .iter()
                .rev()
                .find(|c| !c.1.starts_with("(anonymous)"))
                .or(active.last())
            {
                functions.entry(caller.clone()).or_default().spawn_ms += waited;
                let mut seen = HashSet::new();
                for k in active.iter().filter(|k| seen.insert(*k)) {
                    functions.entry(k.clone()).or_default().waits_ms += waited;
                }
            } else {
                *unowned += waited;
            }
            continue;
        }
        let mut next = active.clone();
        if let Some(k) = key {
            let inclusive = total(id, &by_id, &mut memo);
            let acc = functions.entry(k.clone()).or_default();
            acc.self_ms += self_ms;
            if !active.contains(k) {
                acc.total_ms += inclusive;
                if let Some(caller) = active
                    .iter()
                    .rev()
                    .find(|c| !c.1.starts_with("(anonymous)"))
                    && caller != k
                {
                    acc.callers.entry(caller.clone()).or_default().0 += inclusive;
                }
            }
            // Anonymous callbacks stay on the stack too; the caller lookup above skips them.
            next.push(k.clone());
        }
        for c in children {
            stack.push((*c, next.clone()));
        }
    }
    duration_ms
}

// ---- Python cProfile ------------------------------------------------------------------------

const PSTATS_PY: &str = r#"
import json, os, pstats, sys, sysconfig
stats = pstats.Stats(sys.argv[1]).stats
stdlib = os.path.realpath(sysconfig.get_paths()["stdlib"])
def in_stdlib(file):
    return file[:1] not in ("", "<", "~") and os.path.dirname(os.path.realpath(file)) == stdlib
out = []
for (file, line, name), (cc, nc, tt, ct, callers) in stats.items():
    out.append({
        "file": file, "line": line, "name": name, "calls": nc, "self": tt * 1000, "total": ct * 1000,
        "stdlib": in_stdlib(file),
        "callers": [{"file": f, "line": l, "name": n, "calls": c[1], "total": c[3] * 1000} for (f, l, n), c in callers.items()],
    })
json.dump(out, open(sys.argv[2], "w"))
"#;

fn python(root: &Path, command: &[String], work: &Path) -> Result<Waited> {
    let program = Path::new(&command[0])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !program.starts_with("python") {
        bail!(
            "for Python, give the interpreter and script: `rust-anything profile . -- python3 bench.py`"
        );
    }
    let stats = work.join("profile.pstats");
    let start = std::time::Instant::now();
    let status = crate::process::status(
        Command::new(&command[0])
            .stdout(std::io::stderr())
            .args(["-m", "cProfile", "-o"])
            .arg(&stats)
            .args(&command[1..])
            .current_dir(root),
        crate::process::TESTS,
    )
    .with_context(|| format!("running {}", command[0]))?;
    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    if !status.success() {
        bail!("the workload failed ({status})");
    }
    let json = work.join("profile.json");
    let script = work.join("pstats_json.py");
    std::fs::write(&script, PSTATS_PY)?;
    let status = crate::process::status(
        Command::new(&command[0])
            .stdout(std::io::stderr())
            .arg(&script)
            .arg(&stats)
            .arg(&json),
        crate::process::TESTS,
    )?;
    if !status.success() {
        bail!("reading the cProfile output failed");
    }
    let entries: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&json)?)?;
    let (functions, unowned) = ingest_entries(root, &entries);
    Ok((functions, unowned, total_ms))
}

/// Function entries from a tracing profiler (cProfile, the Ruby tracer): file, line, name,
/// calls, self and total ms, and callers with their calls and ms. Also returns the ms of waits on
/// child processes that no user function owns.
fn ingest_entries(root: &Path, entries: &[Value]) -> (HashMap<Key, Acc>, f64) {
    let mut functions: HashMap<Key, Acc> = HashMap::new();
    for e in entries {
        let raw_file = e["file"].as_str().unwrap_or("");
        let Some(file) = relative(root, raw_file) else {
            continue;
        };
        let key = (
            file,
            e["name"].as_str().unwrap_or("").to_string(),
            u32::try_from(e["line"].as_u64().unwrap_or(0)).unwrap_or(0),
        );
        let acc = functions.entry(key).or_default();
        acc.self_ms += e["self"].as_f64().unwrap_or(0.0);
        acc.total_ms += e["total"].as_f64().unwrap_or(0.0);
        acc.calls = Some(acc.calls.unwrap_or(0) + e["calls"].as_u64().unwrap_or(0));
        for c in e["callers"].as_array().into_iter().flatten() {
            let Some(cfile) = relative(root, c["file"].as_str().unwrap_or("")) else {
                continue;
            };
            let ckey = (
                cfile,
                c["name"].as_str().unwrap_or("").to_string(),
                u32::try_from(c["line"].as_u64().unwrap_or(0)).unwrap_or(0),
            );
            let slot = acc.callers.entry(ckey).or_default();
            slot.0 += c["total"].as_f64().unwrap_or(0.0);
            slot.1 = Some(slot.1.unwrap_or(0) + c["calls"].as_u64().unwrap_or(0));
        }
    }
    let unowned = child_waits(root, entries, &mut functions);
    (functions, unowned)
}

/// Python's calls that wait for a child process: `subprocess.run` and its kin, a `Popen`'s
/// `communicate`, `wait` and `__exit__` (`with Popen(...)`), and `os.system` (a Python wrapper on some versions, a builtin on
/// others).
fn python_spawn(file: &str, name: &str) -> bool {
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    (base == "subprocess.py"
        && matches!(
            name,
            "run"
                | "call"
                | "check_call"
                | "check_output"
                | "getoutput"
                | "getstatusoutput"
                | "communicate"
                | "wait"
                | "__exit__"
        ))
        || (base == "os.py" && name == "system")
        || (file == "~"
            && matches!(
                name,
                "<built-in method posix.system>" | "<built-in method nt.system>"
            ))
}

/// Waits on child processes in a tracing profile (Python's `subprocess` calls). The caller edges
/// are followed through library frames too (`fetch` -> a library's `run_git` ->
/// `subprocess.run`): each user function gets the waits under it (`waits_ms`) and the ones it
/// starts with no user function in between (`spawn_ms`).
fn child_waits(root: &Path, entries: &[Value], functions: &mut HashMap<Key, Acc>) -> f64 {
    let raw = |v: &Value| -> Key {
        (
            v["file"].as_str().unwrap_or("").to_string(),
            v["name"].as_str().unwrap_or("").to_string(),
            u32::try_from(v["line"].as_u64().unwrap_or(0)).unwrap_or(0),
        )
    };
    // The profiled interpreter's standard library, not a repository or dependency module that
    // happens to be called `subprocess.py`.
    let stdlib: HashSet<&str> = entries
        .iter()
        .filter(|e| e["stdlib"].as_bool() == Some(true))
        .filter_map(|e| e["file"].as_str())
        .collect();
    let is_spawn =
        |k: &Key| python_spawn(&k.0, &k.1) && (k.0 == "~" || stdlib.contains(k.0.as_str()));
    if !entries.iter().any(|e| is_spawn(&raw(e))) {
        return 0.0;
    }
    let mut ids: HashMap<Key, usize> = HashMap::new();
    let mut keys: Vec<Key> = Vec::new();
    let mut id = |k: Key| -> usize {
        *ids.entry(k.clone()).or_insert_with(|| {
            keys.push(k);
            keys.len() - 1
        })
    };
    let mut edges: Vec<(usize, usize, f64)> = Vec::new(); // (caller, callee, ms)
    let mut spawn_edges: Vec<(usize, f64)> = Vec::new(); // (caller, ms)
    let mut totals: Vec<(usize, f64)> = Vec::new();
    for e in entries {
        let me = raw(e);
        let spawner = is_spawn(&me);
        let i = id(me);
        totals.push((i, e["total"].as_f64().unwrap_or(0.0)));
        for c in e["callers"].as_array().into_iter().flatten() {
            let ck = raw(c);
            let ms = c["total"].as_f64().unwrap_or(0.0);
            if spawner {
                // The outermost waiting call: `subprocess.run` calls `communicate`, which calls
                // `wait`; each wait counts once.
                if !is_spawn(&ck) {
                    spawn_edges.push((id(ck), ms));
                }
            } else if !is_spawn(&ck) {
                edges.push((id(ck), i, ms));
            }
        }
    }
    let n = keys.len();
    let mut g = Graph {
        total: vec![0.0; n],
        spawn: vec![0.0; n],
        callees: vec![Vec::new(); n],
        callers: vec![Vec::new(); n],
    };
    for (i, t) in totals {
        g.total[i] += t;
    }
    for (c, ms) in spawn_edges {
        g.spawn[c] += ms;
    }
    for (caller, callee, ms) in edges {
        if caller != callee && g.total[callee] > 0.0 {
            g.callees[caller].push((callee, (ms / g.total[callee]).min(1.0)));
            g.callers[callee].push(caller);
        }
    }
    let user: Vec<Option<Key>> = keys
        .iter()
        .map(|k| {
            (!is_spawn(k))
                .then(|| relative(root, &k.0).map(|f| (f, k.1.clone(), k.2)))
                .flatten()
        })
        .collect();
    let waits = g.confirmed();
    // Waits started by library frames and reaching a frame through library frames only, so a
    // user function's own waits are not listed again for its user caller, nor for itself when
    // it recurses through a library (`ast.NodeVisitor`).
    let foreign = g.waits(|c| user[c].is_none());
    let mut owned = 0.0;
    for (i, k) in user.iter().enumerate() {
        let Some(acc) = k.as_ref().and_then(|k| functions.get_mut(k)) else {
            continue;
        };
        acc.waits_ms += waits[i];
        let started = g.spawn[i]
            + g.callees[i]
                .iter()
                .filter(|(c, _)| user[*c].is_none())
                .map(|(c, share)| share * foreign[*c])
                .sum::<f64>();
        acc.spawn_ms += started;
        owned += started;
    }
    (g.spawn.iter().sum::<f64>() - owned).max(0.0)
}

/// A call graph over a tracing profile's frames.
struct Graph {
    total: Vec<f64>,
    /// Waits on child processes started by this frame itself.
    spawn: Vec<f64>,
    /// caller -> [(callee, share of the callee's time spent under this caller)]
    callees: Vec<Vec<(usize, f64)>>,
    callers: Vec<Vec<usize>>,
}

impl Graph {
    /// Waits under each frame, counting only the frames `only` accepts: its own, plus each
    /// callee's in the share of the callee's time spent under it, never more than its own time.
    /// Recursion (a cycle of calls) shares one total, so no wait goes around a cycle twice.
    fn waits(&self, only: impl Fn(usize) -> bool) -> Vec<f64> {
        let n = self.total.len();
        // Only frames above a wait matter.
        let mut relevant = vec![false; n];
        let mut todo: Vec<usize> = (0..n).filter(|&i| self.spawn[i] > 0.0 && only(i)).collect();
        while let Some(i) = todo.pop() {
            if !std::mem::replace(&mut relevant[i], true) {
                todo.extend(self.callers[i].iter().copied().filter(|&c| !relevant[c]));
            }
        }
        let mut waits = vec![0.0; n];
        let mut component = vec![usize::MAX; n];
        for (id, scc) in sccs(&self.callees, &relevant).into_iter().enumerate() {
            for &m in &scc {
                component[m] = id;
            }
            let own: f64 = scc
                .iter()
                .filter(|&&m| only(m))
                .map(|&m| self.spawn[m])
                .sum();
            let under: f64 = scc
                .iter()
                .flat_map(|&m| self.callees[m].iter())
                .filter(|(c, _)| relevant[*c] && component[*c] != id && only(*c))
                .map(|(c, share)| share * waits[*c])
                .sum();
            for &m in &scc {
                waits[m] = (own + under).min(self.total[m].max(self.spawn[m]));
            }
        }
        waits
    }
}

impl Graph {
    /// Waits known to happen under each frame: its own, plus from each callee the part the
    /// callee's other callers cannot account for (its waits less the time it spent under them).
    /// A per-caller share would charge a shared helper's waits to callers that only ran its CPU
    /// work. Calls within a recursive component confirm nothing for the other members.
    fn confirmed(&self) -> Vec<f64> {
        let n = self.total.len();
        let mut relevant = vec![false; n];
        let mut todo: Vec<usize> = (0..n).filter(|&i| self.spawn[i] > 0.0).collect();
        while let Some(i) = todo.pop() {
            if !std::mem::replace(&mut relevant[i], true) {
                todo.extend(self.callers[i].iter().copied().filter(|&c| !relevant[c]));
            }
        }
        let mut waits = vec![0.0; n];
        let mut component = vec![usize::MAX; n];
        for (id, scc) in sccs(&self.callees, &relevant).into_iter().enumerate() {
            for &m in &scc {
                component[m] = id;
            }
            for &m in &scc {
                let under: f64 = self.callees[m]
                    .iter()
                    .filter(|(c, _)| relevant[*c] && component[*c] != id)
                    .map(|&(c, share)| (waits[c] - (1.0 - share) * self.total[c]).max(0.0))
                    .sum();
                waits[m] = (self.spawn[m] + under).min(self.total[m].max(self.spawn[m]));
            }
        }
        waits
    }
}

/// Strongly connected components of the relevant frames, callees before their callers
/// (Tarjan's algorithm, without recursion).
fn sccs(edges: &[Vec<(usize, f64)>], relevant: &[bool]) -> Vec<Vec<usize>> {
    let n = edges.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut out = Vec::new();
    let mut next = 0;
    for start in (0..n).filter(|&i| relevant[i]) {
        if index[start] != usize::MAX {
            continue;
        }
        index[start] = next;
        low[start] = next;
        next += 1;
        stack.push(start);
        on_stack[start] = true;
        // (frame, next edge to look at)
        let mut work = vec![(start, 0usize)];
        while let Some(&(v, e)) = work.last() {
            if let Some(&(w, _)) = edges[v].get(e) {
                if let Some(top) = work.last_mut() {
                    top.1 += 1;
                }
                if !relevant[w] {
                    continue;
                }
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                let mut scc = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    scc.push(w);
                    if w == v {
                        break;
                    }
                }
                out.push(scc);
            }
        }
    }
    out
}

// ---- stack samples (Go pprof, Java JFR) -----------------------------------------------------

/// Adds sampled stacks, each root first with `None` for frames outside user code, and the time
/// it stands for. A function's total counts once per sample (recursion); its caller is the
/// nearest named user frame above it (lambdas and closures are skipped).
fn merge_samples(samples: &[(Vec<Option<Key>>, f64)], functions: &mut HashMap<Key, Acc>) {
    for (stack, ms) in samples {
        let mut seen: Vec<&Key> = Vec::new();
        let mut named: Option<&Key> = None;
        for frame in stack {
            let Some(k) = frame else { continue };
            if !seen.contains(&k) {
                seen.push(k);
                let acc = functions.entry(k.clone()).or_default();
                acc.total_ms += ms;
                if let Some(caller) = named.filter(|c| *c != k) {
                    acc.callers.entry(caller.clone()).or_default().0 += ms;
                }
            }
            if !k.1.starts_with("(anonymous)") {
                named = Some(k);
            }
        }
        if let Some(Some(leaf)) = stack.last() {
            functions.entry(leaf.clone()).or_default().self_ms += ms;
        }
    }
}

/// A Go function's own name: `example.com/x/pkg.Slugify` is `Slugify`, `pkg.(*T).M` is `M`;
/// closures (`Outer.func1`) are anonymous.
fn go_name(full: &str) -> String {
    let tail = full.rsplit('/').next().unwrap_or(full);
    let rest = tail.split_once('.').map_or(tail, |(_, r)| r);
    let parts: Vec<&str> = rest.split('.').collect();
    match parts.as_slice() {
        [.., outer, f] if f.starts_with("func") && f[4..].chars().all(|c| c.is_ascii_digit()) => {
            format!("(anonymous) in {outer}")
        }
        [.., last] => last.to_string(),
        [] => rest.to_string(),
    }
}

/// `go tool pprof -raw` output: samples (leaf first, by location id) and locations (each one or
/// more frames, innermost first: `func file:line:col s=start`).
fn parse_pprof_raw(root: &Path, raw: &str) -> Vec<(Vec<Option<Key>>, f64)> {
    let mut locations: HashMap<u64, Vec<Option<Key>>> = HashMap::new();
    let mut samples: Vec<(Vec<u64>, f64)> = Vec::new();
    let mut section = "";
    let mut current: Option<u64> = None;
    // Which sample value is CPU time: `samples/count cpu/nanoseconds` puts it second.
    let mut value_index = 1;
    for line in raw.lines() {
        match line.trim_end() {
            "Samples:" => {
                section = "samples";
                continue;
            }
            "Locations" => {
                section = "locations";
                continue;
            }
            "Mappings" => {
                section = "";
                continue;
            }
            _ => {}
        }
        match section {
            "samples" if line.contains(':') => {
                let (values, ids) = line.split_once(':').unwrap_or_default();
                let values: Vec<f64> = values
                    .split_whitespace()
                    .filter_map(|v| v.parse().ok())
                    .collect();
                let ids: Vec<u64> = ids
                    .split_whitespace()
                    .filter_map(|v| v.parse().ok())
                    .collect();
                if let Some(ns) = values.get(value_index).or(values.last()) {
                    samples.push((ids, ns / 1e6));
                }
            }
            "samples" => {
                let kinds: Vec<&str> = line.split_whitespace().collect();
                if let Some(i) = kinds.iter().position(|k| k.ends_with("/nanoseconds")) {
                    value_index = i;
                }
            }
            "locations" => {
                let text = line.trim_start();
                let frame_text = match text.split_once(": ") {
                    Some((id, rest)) if id.chars().all(|c| c.is_ascii_digit()) => {
                        current = id.parse().ok();
                        // `0x4c2ded M=1 func file:line:col s=start`
                        rest.split_whitespace()
                            .skip(2)
                            .collect::<Vec<_>>()
                            .join(" ")
                    }
                    _ => text.to_string(),
                };
                let Some(id) = current else { continue };
                let mut parts = frame_text.split_whitespace();
                let (Some(func), Some(place)) = (parts.next(), parts.next()) else {
                    continue;
                };
                let start = parts
                    .find_map(|p| p.strip_prefix("s="))
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                let file = place.rsplitn(3, ':').nth(2).unwrap_or(place);
                let key = relative(root, file).map(|f| (f, go_name(func), start));
                locations.entry(id).or_default().push(key);
            }
            _ => {}
        }
    }
    samples
        .into_iter()
        .map(|(ids, ms)| {
            // Leaf first, and inlined frames innermost first: reversed, the stack is root first.
            let mut stack: Vec<Option<Key>> = ids
                .iter()
                .flat_map(|id| locations.get(id).cloned().unwrap_or_default())
                .collect();
            stack.reverse();
            (stack, ms)
        })
        .collect()
}

/// Go: `go test` with `-cpuprofile` (Go has no profiler to attach from outside), read with
/// `go tool pprof -raw`.
fn go(root: &Path, command: &[String], work: &Path) -> Result<(HashMap<Key, Acc>, f64)> {
    let program = Path::new(&command[0])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if program != "go" || command.get(1).map(String::as_str) != Some("test") {
        bail!(
            "for Go, profile a benchmark or test: `rust-anything profile . -- go test -run XXX -bench . ./pkg` \
             (Go profiles only from inside the process; `go test` adds -cpuprofile)"
        );
    }
    let out = work.join("cpu.pprof");
    let status = crate::process::status(
        Command::new(&command[0])
            .stdout(std::io::stderr())
            .arg("test")
            .arg(format!("-cpuprofile={}", out.display()))
            .args(&command[2..])
            .current_dir(root),
        crate::process::TESTS,
    )
    .with_context(|| format!("running {}", command[0]))?;
    if !status.success() {
        bail!("the workload failed ({status})");
    }
    let raw = crate::process::output(
        Command::new("go")
            .args(["tool", "pprof", "-raw"])
            .arg(&out)
            .current_dir(root),
        crate::process::TESTS,
    )?;
    if !raw.status.success() {
        bail!(
            "go tool pprof failed: {}",
            String::from_utf8_lossy(&raw.stderr)
        );
    }
    let samples = parse_pprof_raw(root, &String::from_utf8_lossy(&raw.stdout));
    let total: f64 = samples.iter().map(|(_, ms)| ms).sum();
    let mut functions = HashMap::new();
    merge_samples(&samples, &mut functions);
    Ok((functions, total))
}

/// The line declaring method `name` in Java source: the nearest declaration at or above `near`.
fn java_declaration(text: &str, name: &str, near: u32) -> Option<u32> {
    let needle = format!("{name}(");
    let mut best = None;
    for (i, line) in text.lines().enumerate() {
        let n = u32::try_from(i + 1).unwrap_or(u32::MAX);
        if n > near.max(1) {
            break;
        }
        let Some(at) = line.find(&needle) else {
            continue;
        };
        let before = line[..at].trim_end();
        // A declaration names a type or modifier before the method: `static int pages(`.
        let declares = before
            .ends_with(|c: char| c.is_alphanumeric() || matches!(c, '>' | ']' | '_'))
            && !before.contains('=')
            && !before.contains("return")
            && !before.contains("new ")
            && !before.ends_with('.')
            && !line.trim_end().ends_with(';');
        if declares && at > 0 && line.as_bytes()[at - 1].is_ascii_whitespace() {
            best = Some(n);
        }
    }
    best
}

/// Java: Flight Recorder's execution samples, read with `jfr print --json`.
fn java(root: &Path, command: &[String], work: &Path) -> Result<(HashMap<Key, Acc>, f64)> {
    let recording = work.join("profile.jfr");
    let mut options = std::env::var("JAVA_TOOL_OPTIONS").unwrap_or_default();
    options.push_str(&format!(
        " -XX:StartFlightRecording=filename={},settings=profile -XX:FlightRecorderOptions=stackdepth=256",
        recording.display()
    ));
    let status = crate::process::status(
        Command::new(&command[0])
            .stdout(std::io::stderr())
            .args(&command[1..])
            .current_dir(root)
            .env("JAVA_TOOL_OPTIONS", options.trim()),
        crate::process::TESTS,
    )
    .with_context(|| format!("running {}", command[0]))?;
    if !status.success() {
        bail!("the workload failed ({status})");
    }
    let out = crate::process::output(
        Command::new("jfr")
            .args([
                "print",
                "--json",
                "--stack-depth",
                "256",
                "--events",
                "jdk.ExecutionSample",
            ])
            .arg(&recording),
        crate::process::TESTS,
    )
    .context("running jfr (it comes with the JDK)")?;
    if !out.status.success() {
        bail!("jfr print failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let json: Value = serde_json::from_slice(&out.stdout).context("jfr output")?;
    // Sources by class: `com/example/text/Strings` -> its file in the repository.
    let mut sources: HashMap<String, String> = HashMap::new();
    for entry in crate::walk::builder(root).build() {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "java")
            && let Ok(rel) = path.strip_prefix(root)
        {
            let rel = rel.to_string_lossy().replace('\\', "/");
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let package = text
                .lines()
                .find_map(|l| {
                    l.trim()
                        .strip_prefix("package ")
                        .map(|p| p.trim_end_matches(';').trim().to_string())
                })
                .unwrap_or_default();
            let class = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let fq = if package.is_empty() {
                class
            } else {
                format!("{}/{class}", package.replace('.', "/"))
            };
            sources.insert(fq, rel);
        }
    }
    const SAMPLE_MS: f64 = 10.0; // the `profile` settings sample Java threads every 10 ms
    let mut raw: Vec<Vec<Option<(String, String, u32)>>> = Vec::new();
    for event in json["recording"]["events"].as_array().into_iter().flatten() {
        let frames = event["values"]["stackTrace"]["frames"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut stack: Vec<Option<(String, String, u32)>> = frames
            .iter()
            .map(|f| {
                let class = f["method"]["type"]["name"].as_str()?;
                let outer = class.split('$').next().unwrap_or(class);
                let file = sources.get(outer)?;
                let name = f["method"]["name"].as_str()?.to_string();
                let line = u32::try_from(f["lineNumber"].as_i64().unwrap_or(0)).unwrap_or(0);
                Some((file.clone(), name, line))
            })
            .collect();
        stack.reverse();
        raw.push(stack);
    }
    // Key each method by its declaration line, found from the lowest sampled line.
    let mut lowest: HashMap<(String, String), u32> = HashMap::new();
    for (file, name, line) in raw.iter().flatten().flatten() {
        let e = lowest.entry((file.clone(), name.clone())).or_insert(*line);
        *e = (*e).min(*line);
    }
    let mut texts: HashMap<String, String> = HashMap::new();
    let mut key = |file: &str, name: &str| -> Key {
        if name.starts_with("lambda$") {
            return (file.to_string(), format!("(anonymous) {name}"), 0);
        }
        let text = texts
            .entry(file.to_string())
            .or_insert_with(|| std::fs::read_to_string(root.join(file)).unwrap_or_default());
        let near = lowest
            .get(&(file.to_string(), name.to_string()))
            .copied()
            .unwrap_or(0);
        let line = java_declaration(text, name, near).unwrap_or(near);
        (file.to_string(), name.to_string(), line)
    };
    let samples: Vec<(Vec<Option<Key>>, f64)> = raw
        .iter()
        .map(|stack| {
            let keys = stack
                .iter()
                .map(|f| f.as_ref().map(|(file, name, _)| key(file, name)))
                .collect();
            (keys, SAMPLE_MS)
        })
        .collect();
    let total = samples.len() as f64 * SAMPLE_MS;
    if samples.is_empty() {
        bail!("no samples were recorded: run the workload longer (Java is sampled every 10 ms)");
    }
    let mut functions = HashMap::new();
    merge_samples(&samples, &mut functions);
    Ok((functions, total))
}

// ---- Ruby: a call tracer --------------------------------------------------------------------

/// Loaded with `RUBYOPT=-r`: times every Ruby method call on the main thread (inclusive, self,
/// callers) and writes cProfile-like entries at exit. Tracing slows the program; the shares stay
/// comparable.
const RUBY_TRACER: &str = r#"
require "json"

module RustAnythingProfile
  STATS = Hash.new { |h, k| h[k] = { calls: 0, self: 0.0, total: 0.0, callers: Hash.new { |c, ck| c[ck] = [0, 0.0] } } }
  STACK = []
  ACTIVE = Hash.new(0)
  MAIN = Thread.current
  TRACE = TracePoint.new(:call, :return) do |tp|
    next unless Thread.current == MAIN
    now = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    if tp.event == :call
      path = tp.path
      path = File.expand_path(path) unless path.start_with?("<")
      key = [path, tp.lineno, tp.method_id.to_s]
      ACTIVE[key] += 1
      STACK << [key, now, 0.0]
    elsif (frame = STACK.pop)
      key, start, child = frame
      elapsed = now - start
      ACTIVE[key] -= 1
      outer = ACTIVE[key].zero?
      s = STATS[key]
      s[:calls] += 1
      s[:self] += elapsed - child
      s[:total] += elapsed if outer
      if (parent = STACK.last)
        parent[2] += elapsed
        c = s[:callers][parent[0]]
        c[0] += 1
        c[1] += elapsed if outer
      end
    end
  end
  TRACE.enable

  at_exit do
    TRACE.disable
    out = STATS.map do |(file, line, name), s|
      { file: file, line: line, name: name, calls: s[:calls], self: s[:self] * 1000, total: s[:total] * 1000,
        callers: s[:callers].map { |(f, l, n), (calls, total)| { file: f, line: l, name: n, calls: calls, total: total * 1000 } } }
    end
    File.write(ENV.fetch("RUST_ANYTHING_PROFILE_OUT"), JSON.generate(out))
  end
end
"#;

/// Ruby: the tracer above, loaded into any Ruby command through `RUBYOPT`.
fn ruby(root: &Path, command: &[String], work: &Path) -> Result<(HashMap<Key, Acc>, f64)> {
    let tracer = work.join("rust_anything_profile.rb");
    std::fs::write(&tracer, RUBY_TRACER)?;
    let out = work.join("profile.json");
    let mut rubyopt = std::env::var("RUBYOPT").unwrap_or_default();
    rubyopt.push_str(&format!(" -r{}", tracer.display()));
    let start = std::time::Instant::now();
    let status = crate::process::status(
        Command::new(&command[0])
            .stdout(std::io::stderr())
            .args(&command[1..])
            .current_dir(root)
            .env("RUBYOPT", rubyopt.trim())
            .env("RUST_ANYTHING_PROFILE_OUT", &out),
        crate::process::TESTS,
    )
    .with_context(|| format!("running {}", command[0]))?;
    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    if !status.success() {
        bail!("the workload failed ({status})");
    }
    let entries: Vec<Value> = serde_json::from_str(
        &std::fs::read_to_string(&out)
            .context("the Ruby tracer wrote nothing: is the workload a Ruby process?")?,
    )?;
    Ok((ingest_entries(root, &entries).0, total_ms))
}

// ---- the report -----------------------------------------------------------------------------

/// The argument a caller passes per loop item: an arrow or for-of variable among `f`'s call
/// arguments in the caller's source. `None` when the source does not show one.
fn batch_param(root: &Path, caller: &Key, callee: &str, params: &[String]) -> Option<String> {
    let text = std::fs::read_to_string(root.join(&caller.0)).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let start = (caller.2 as usize).saturating_sub(1);
    let body = lines.get(start..(start + 80).min(lines.len()))?.join("\n");
    let mut loop_vars: Vec<String> = Vec::new();
    for (i, _) in body.match_indices("=>") {
        let before = body[..i].trim_end().trim_end_matches(')');
        let name: String = before
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if !name.is_empty() {
            loop_vars.push(name);
        }
    }
    for word in ["const ", "let ", "var ", "for "] {
        for (i, _) in body.match_indices(word) {
            let rest = &body[i + word.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if rest[name.len()..].trim_start().starts_with("of ")
                || rest[name.len()..].trim_start().starts_with("in ")
            {
                loop_vars.push(name);
            }
        }
    }
    let (more, index_vars) = loop_variables(&body);
    loop_vars.extend(more);
    let call = body.find(&format!("{callee}("))?;
    let args_text = &body[call + callee.len() + 1..];
    let mut depth = 0;
    let mut args = vec![String::new()];
    for c in args_text.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth == 0 => break,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                args.push(String::new());
                continue;
            }
            _ => {}
        }
        args.last_mut()?.push(c);
    }
    // The item itself (`t`), or the item at the loop index (`texts[i]`).
    let per_item = |a: &str| {
        let a = a.trim();
        loop_vars.iter().any(|v| v == a)
            || a.strip_suffix(']')
                .and_then(|a| a.split_once('['))
                .is_some_and(|(list, i)| {
                    !list.is_empty() && index_vars.iter().any(|v| v == i.trim())
                })
    };
    // In nested loops the innermost one runs over the items: its variable is declared last.
    let declared_at = |a: &str| {
        let a = a.trim();
        let var = a
            .strip_suffix(']')
            .and_then(|a| a.split_once('['))
            .map_or(a, |(_, i)| i.trim());
        regex::Regex::new(&format!(r"\b{}\b", regex::escape(var)))
            .ok()
            .and_then(|re| re.find(&body).map(|m| m.start()))
            .unwrap_or(0)
    };
    let position = args
        .iter()
        .enumerate()
        .filter(|(_, a)| per_item(a))
        .max_by_key(|(_, a)| declared_at(a))?
        .0;
    params.get(position).cloned()
}

/// Loop variables in Go, Java and Ruby source: `(items, indexes)`. Items: `for _, t := range`,
/// `for (String t : texts)`, `|t|`; indexes: `for i := range`, `for (int i = 0;`.
fn loop_variables(body: &str) -> (Vec<String>, Vec<String>) {
    let ident = |s: &str| -> String {
        s.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect()
    };
    let last_ident = |s: &str| -> String {
        let s = s.trim_end();
        let rev: String = s
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        rev.chars().rev().collect()
    };
    let (mut items, mut indexes) = (Vec::new(), Vec::new());
    for (i, _) in body.match_indices("for") {
        if body[..i]
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let rest = &body[i + 3..];
        let line = rest.split(['{', '\n']).next().unwrap_or(rest);
        if let Some((vars, _)) = line.split_once(":= range") {
            // Go: `for i, t := range xs` (t is the item), `for i := range xs` (an index).
            let vars: Vec<String> = vars.split(',').map(|v| v.trim().to_string()).collect();
            match vars.as_slice() {
                [index, item] => {
                    items.push(item.clone());
                    indexes.push(index.clone());
                }
                [index] => indexes.push(index.clone()),
                _ => {}
            }
        } else if let Some(inner) = line.trim_start().strip_prefix('(') {
            // Java: `for (String t : texts)` or `for (int i = 0; ...)`.
            if let Some((decl, _)) = inner.split_once(" : ").filter(|(d, _)| !d.contains(';')) {
                items.push(last_ident(decl));
            } else if let Some((decl, _)) = inner.split_once('=') {
                indexes.push(last_ident(decl));
            }
        }
    }
    // Ruby blocks: `{ |t| ... }`, `do |t, i|`.
    for (i, _) in body.match_indices('|') {
        let before = body[..i].trim_end();
        if before.ends_with('{') || before.ends_with("do") {
            let first = ident(body[i + 1..].trim_start());
            if !first.is_empty() {
                items.push(first);
            }
        }
    }
    items.retain(|v| !v.is_empty() && v != "_");
    indexes.retain(|v| !v.is_empty() && v != "_");
    (items, indexes)
}

fn report(
    root: &Path,
    lang: Lang,
    command: &[String],
    functions: HashMap<Key, Acc>,
    unowned_ms: f64,
    total_ms: f64,
) -> Report {
    let analysis = crate::analyze::analyze_lang(root, lang).ok();
    let unit_of = |file: &str| -> Option<(String, bool)> {
        let a = analysis.as_ref()?;
        let u = a.units.iter().find(|u| u.files.iter().any(|f| f == file))?;
        let portable = !matches!(
            u.status,
            crate::analyze::Status::Keep | crate::analyze::Status::Blocked
        );
        Some((u.id.clone(), portable))
    };
    // Each hot file's exported functions, read and judged once, as `bridge` reads and judges
    // them.
    use_project_types(root, lang);
    let mut judged: HashMap<String, Vec<(crate::ts::exports::ExportedFn, Option<String>)>> =
        HashMap::new();
    for ((file, _, _), a) in &functions {
        if in_repo(file)
            && total_ms > 0.0
            && a.total_ms / total_ms >= 0.01
            && !judged.contains_key(file)
        {
            let exports = std::fs::read_to_string(root.join(file))
                .map(|text| exports_judged(root, lang, file, &text))
                .unwrap_or_default();
            judged.insert(file.clone(), exports);
        }
    }
    let exports_of = |file: &str,
                      name: &str,
                      line: u32|
     -> Option<(crate::ts::exports::ExportedFn, Option<String>)> {
        let exports = judged.get(file)?;
        exports
            .iter()
            .find(|(f, _)| f.name == name && f.line == line)
            .or_else(|| exports.iter().find(|(f, _)| f.name == name))
            .cloned()
    };
    let repo_self: f64 = functions
        .iter()
        .filter(|(k, _)| in_repo(&k.0))
        .map(|(_, a)| a.self_ms)
        .sum();
    let mut calls_in: HashMap<&str, Vec<SpawnCall>> = HashMap::new();
    let mut child_processes: Vec<ChildProcess> = functions
        .iter()
        .filter(|(_, a)| total_ms > 0.0 && a.spawn_ms / total_ms >= 0.01)
        .map(|((file, name, line), a)| {
            let calls = calls_in
                .entry(file.as_str())
                .or_insert_with(|| spawn_calls_in(root, lang, file));
            let (line, command) = spawn_site(calls, name, *line).unwrap_or((*line, None));
            ChildProcess {
                command,
                file: file.clone(),
                line,
                function: name.clone(),
                ms: a.spawn_ms,
                share: 100.0 * a.spawn_ms / total_ms,
            }
        })
        .collect();
    child_processes.sort_by(|a, b| b.ms.total_cmp(&a.ms));
    // Every wait counts once, at the frame that starts it, so the parts add up to the whole.
    let child_pct = if total_ms > 0.0 {
        (100.0 * (unowned_ms + functions.values().map(|a| a.spawn_ms).sum::<f64>()) / total_ms)
            .min(100.0)
    } else {
        0.0
    };
    let child_pct = if child_pct >= 1.0 { child_pct } else { 0.0 };
    let mut hot: Vec<Hot> = functions
        .into_iter()
        .filter(|((file, _, _), a)| in_repo(file) && total_ms > 0.0 && a.total_ms / total_ms >= 0.01)
        .map(|((file, name, line), a)| {
            let unit = unit_of(&file);
            let (export, js_only) = match exports_of(&file, &name, line) {
                Some((f, reason)) => (Some(f), reason),
                None => (None, None),
            };
            let mut callers: Vec<Caller> = a
                .callers
                .iter()
                .map(|((f, n, l), (ms, calls))| Caller { name: n.clone(), file: f.clone(), line: *l, ms: *ms, calls: *calls })
                .collect();
            callers.sort_by(|x, y| y.ms.total_cmp(&x.ms));
            callers.truncate(5);
            let ns_per_call = a.calls.filter(|c| *c > 0).map(|c| a.total_ms * 1e6 / c as f64);
            let total_pct = 100.0 * a.total_ms / total_ms;
            let top = callers.first();
            let small_per_call = ns_per_call.is_some_and(|ns| ns < 20_000.0) || (a.calls.is_none() && top.is_some_and(|c| c.ms >= 0.5 * a.total_ms));
            let params: Vec<String> = export.as_ref().map(|e| e.params.iter().map(|p| p.name.clone()).collect()).unwrap_or_default();
            let child_pct = (a.total_ms > 0.0 && a.waits_ms / a.total_ms >= 0.01)
                .then(|| (100.0 * a.waits_ms / a.total_ms).min(100.0));
            let mut batch_spec = None;
            let advice = match (&unit, &export) {
                _ if child_pct.is_some_and(|p| p >= 50.0) => format!(
                    "Hot, but {:.0}% of its time waits on child processes. {CHILD_ADVICE}",
                    child_pct.unwrap_or_default()
                ),
                (Some((_, true)), Some(_)) if js_only.is_some() => format!(
                    "Hot, but `bridge` keeps `{name}` in {}: {}.",
                    lang.name(),
                    js_only.as_deref().unwrap_or_default()
                ),
                (Some((u, true)), Some(_)) if small_per_call && top.is_some() => {
                    let c = top.expect("checked");
                    let batch = batch_param(root, &(c.file.clone(), c.name.clone(), c.line), &name, &params);
                    batch_spec = batch.as_ref().map(|p| format!("{name}:{p}"));
                    let calls = a.calls.map_or_else(String::new, |n| format!(", {n} calls"));
                    match batch {
                        Some(param) => format!(
                            "Called in a loop by `{}` ({}:{}{calls}): a call per item would cost more than the work. \
                             Batch it, keeping the items in Rust and using all cores: \
                             `fnr bridge {} {u} --batch {name}:{param}`, then prepare the items once in a \
                             `{}Index` in port.rs. Better still, port `{}` with it.",
                            c.name, c.file, c.line, root.display(), crate::codegen_batch::pascal(&name), c.name
                        ),
                        None => format!(
                            "Mostly called by `{}` ({}:{}{calls}): port `{}` together with `{name}`, so one call into Rust does the whole loop.",
                            c.name, c.file, c.line, c.name
                        ),
                    }
                }
                (Some((u, true)), Some(_)) => format!(
                    "Hot and exported by `{u}`: port it (`fnr bridge {} {u}`), then check `fnr bench`.",
                    root.display()
                ),
                (Some((u, true)), None) => format!("Hot inside `{u}`: port the exported function that calls it, with it."),
                (Some((u, false)), _) => format!("Hot, but `{u}` must stay in the source language (see `fnr analyze`)."),
                (None, _) => "Hot, outside the analyzed units.".into(),
            };
            Hot {
                portable: unit.as_ref().is_some_and(|(_, p)| *p),
                js_only,
                params,
                batch: batch_spec,
                child_pct,
                unit: unit.map(|(u, _)| u),
                exported: export.is_some(),
                name,
                file,
                line,
                self_ms: a.self_ms,
                total_ms: a.total_ms,
                total_pct,
                calls: a.calls,
                ns_per_call,
                callers,
                advice,
            }
        })
        .collect();
    hot.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
    let compiled = compiled_in(root, lang);
    for h in &mut hot {
        if let Some(advice) = compiled_advice(root, h, &compiled) {
            h.advice = advice;
        }
    }
    Report {
        schema: SCHEMA,
        language: lang,
        command: command.to_vec(),
        total_ms,
        repo_pct: if total_ms > 0.0 {
            (100.0 * repo_self / total_ms).min(100.0)
        } else {
            0.0
        },
        compiled,
        hot,
        child_pct,
        child_processes,
    }
}

/// Where the function `name` defined at `line` starts its child process: the call's line, and
/// the program when the call names it literally. A call in a callback counts for the function
/// around it, as the wait does. `None` when there is no such call, or several that start
/// different programs.
fn spawn_site(calls: &[SpawnCall], name: &str, line: u32) -> Option<(u32, Option<String>)> {
    // A file's top level: Python's `<module>`, Node's module function on line 1.
    let top = name == "<module>" || (line == 1 && name.starts_with("(anonymous)"));
    let own: Vec<&SpawnCall> = calls
        .iter()
        .filter(|c| {
            c.scopes.last() == Some(&line) || c.owner == Some(line) || (top && c.scopes.is_empty())
        })
        .collect();
    let sites = if own.is_empty() {
        calls.iter().filter(|c| c.scopes.contains(&line)).collect()
    } else {
        own
    };
    let first = sites.first()?;
    let same = sites
        .iter()
        .all(|s| s.program.is_some() && s.program == first.program);
    (sites.len() == 1 || same).then(|| (first.line, first.program.clone()))
}

/// The calls in a repository file that start a child process and wait for it.
fn spawn_calls_in(root: &Path, lang: Lang, file: &str) -> Vec<SpawnCall> {
    let inside = Path::new(file)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !inside {
        return Vec::new(); // `/abs/bench.py`, `../bench.py`, `C:\\...`: not the repository's
    }
    let Ok(text) = std::fs::read_to_string(root.join(file)) else {
        return Vec::new();
    };
    match lang {
        Lang::Ts => js_spawn_calls(file, &text),
        Lang::Python => python_spawn_calls(&text),
        _ => Vec::new(),
    }
}

/// A call that starts a child process and waits for it.
#[derive(Debug, Clone, PartialEq)]
struct SpawnCall {
    /// First lines of the functions around the call, innermost last; empty at the top level.
    scopes: Vec<u32>,
    /// The function the profile charges the wait to: in JS the innermost named one (a callback
    /// passed as an argument has no name), else the innermost.
    owner: Option<u32>,
    line: u32,
    /// The program, when the call names it literally.
    program: Option<String>,
}

/// The program in a command line: `sleep` in `sleep 0.2`. `interpolated` when the text goes on
/// with a computed part (`ls ${dir}` gives `ls`, `${bin} run` and `git${x}` give nothing).
/// Leading `NAME=value` words are the child's environment, not the program, and are never shown.
fn first_word(command: &str, interpolated: bool) -> Option<String> {
    let mut rest = command.trim_start();
    loop {
        let word = rest.split_whitespace().next()?;
        let assignment = word.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        let after = rest[word.len()..].trim_start();
        if assignment {
            if word.contains(['"', '\'']) {
                return None; // `TOKEN='a b' deploy`: where the value ends is the shell's business
            }
            rest = after;
            continue;
        }
        // The word must end before any computed part, and be safe in a markdown table.
        let complete = !interpolated || rest.len() > word.len();
        let plain = !word.contains(['|', '`', '$', '{']) && !word.chars().any(char::is_control);
        return (complete && plain).then(|| word.to_string());
    }
}

/// The executable a call names on its own (`execFileSync(file, args)`, an argv list): the whole
/// text, spaces included, when it is safe in a markdown table.
fn whole_program(path: &str) -> Option<String> {
    (!path.trim().is_empty() && !path.contains(['|', '`']) && !path.chars().any(char::is_control))
        .then(|| path.to_string())
}

/// Node's blocking `child_process` calls in a JS/TS file, read with oxc.
fn js_spawn_calls(path: &str, source: &str) -> Vec<SpawnCall> {
    use oxc_ast::ast::{Argument, ArrowFunctionExpression, CallExpression, Expression, Function};
    use oxc_ast_visit::{Visit, walk};
    use oxc_syntax::scope::ScopeFlags;

    struct Calls {
        /// Byte offset of every newline, to turn offsets into lines without rescanning.
        newlines: Vec<usize>,
        /// First line of each enclosing function, and whether V8 gives it a name.
        scopes: Vec<(u32, bool)>,
        /// Where the functions passed straight to a call start: V8 shows them as anonymous.
        callbacks: HashSet<u32>,
        out: Vec<SpawnCall>,
    }
    impl Calls {
        fn line(&self, offset: u32) -> u32 {
            let before = self.newlines.partition_point(|&n| n < offset as usize);
            u32::try_from(before + 1).unwrap_or(u32::MAX)
        }
    }
    impl<'a> Visit<'a> for Calls {
        fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
            let named = it.id.is_some() || !self.callbacks.contains(&it.span.start);
            self.scopes.push((self.line(it.span.start), named));
            walk::walk_function(self, it, flags);
            self.scopes.pop();
        }
        fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
            let named = !self.callbacks.contains(&it.span.start);
            self.scopes.push((self.line(it.span.start), named));
            walk::walk_arrow_function_expression(self, it);
            self.scopes.pop();
        }
        fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
            let callee = match &it.callee {
                Expression::Identifier(id) => Some(id.name.as_str()),
                Expression::StaticMemberExpression(m) => Some(m.property.name.as_str()),
                _ => None,
            };
            for a in &it.arguments {
                match a {
                    Argument::ArrowFunctionExpression(f) => {
                        self.callbacks.insert(f.span.start);
                    }
                    Argument::FunctionExpression(f) if f.id.is_none() => {
                        self.callbacks.insert(f.span.start);
                    }
                    _ => {}
                }
            }
            if let Some(callee) = callee.filter(|n| NODE_SPAWNS.contains(n)) {
                // `execSync` takes a shell command line; the others an executable and its args.
                let shell = callee == "execSync";
                let program = match it.arguments.first() {
                    Some(Argument::StringLiteral(s)) => {
                        let text = s.value.display().to_string();
                        if shell {
                            first_word(&text, false)
                        } else {
                            whole_program(&text)
                        }
                    }
                    Some(Argument::TemplateLiteral(t)) => t.quasis.first().and_then(|q| {
                        let text = q
                            .value
                            .cooked
                            .as_ref()
                            .map_or_else(|| q.value.raw.to_string(), |c| c.display().to_string());
                        let interpolated = !t.expressions.is_empty();
                        if shell {
                            first_word(&text, interpolated)
                        } else if interpolated {
                            None
                        } else {
                            whole_program(&text)
                        }
                    }),
                    _ => None,
                };
                let owner = self
                    .scopes
                    .iter()
                    .rev()
                    .find(|(_, named)| *named)
                    .or(self.scopes.last())
                    .map(|(l, _)| *l);
                self.out.push(SpawnCall {
                    scopes: self.scopes.iter().map(|(l, _)| *l).collect(),
                    owner,
                    line: self.line(it.span.start),
                    program,
                });
            }
            walk::walk_call_expression(self, it);
        }
    }
    let source_type =
        oxc_span::SourceType::from_path(path).unwrap_or_else(|_| oxc_span::SourceType::ts());
    let allocator = oxc_allocator::Allocator::default();
    let ret = oxc_parser::Parser::new(&allocator, source, source_type).parse();
    let mut calls = Calls {
        newlines: source.match_indices('\n').map(|(i, _)| i).collect(),
        scopes: Vec::new(),
        callbacks: HashSet::new(),
        out: Vec::new(),
    };
    calls.visit_program(&ret.program);
    calls.out
}

/// Python's `subprocess` and `os.system` calls in a file, read with tree-sitter: through the
/// module under any name (`import subprocess as sp`), or a function imported from it.
fn python_spawn_calls(source: &str) -> Vec<SpawnCall> {
    use crate::lang::syntax;
    const SUBPROCESS: [&str; 7] = [
        "run",
        "call",
        "check_call",
        "check_output",
        "getoutput",
        "getstatusoutput",
        "Popen",
    ];
    let spawns = |module: &str, name: &str| {
        (module == "subprocess" && SUBPROCESS.contains(&name))
            || (module == "os" && name == "system")
    };
    let Some(tree) = syntax::parse(&tree_sitter_python::LANGUAGE.into(), source) else {
        return Vec::new();
    };
    let root = tree.root_node();
    let field = |n: tree_sitter::Node, f: &str| {
        n.child_by_field_name(f)
            .map_or("", |c| syntax::text(c, source))
    };
    let name_and_alias = |c: tree_sitter::Node| match c.kind() {
        "aliased_import" => (field(c, "name"), field(c, "alias")),
        _ => (syntax::text(c, source), syntax::text(c, source)),
    };
    let mut modules: HashMap<&str, &str> = HashMap::new();
    let mut imported: HashSet<&str> = HashSet::new();
    syntax::walk(root, |n| match n.kind() {
        "import_statement" => {
            for c in syntax::named_children(n) {
                let (module, alias) = name_and_alias(c);
                if matches!(module, "subprocess" | "os") {
                    modules.insert(alias, module);
                }
            }
        }
        "import_from_statement" => {
            let module = field(n, "module_name");
            let mut cursor = n.walk();
            for c in n.children_by_field_name("name", &mut cursor) {
                let (name, alias) = name_and_alias(c);
                if spawns(module, name) {
                    imported.insert(alias);
                }
            }
        }
        _ => {}
    });
    let mut out = Vec::new();
    syntax::walk(root, |n| {
        if n.kind() != "call" {
            return;
        }
        let Some(callee) = n.child_by_field_name("function") else {
            return;
        };
        let starts = match callee.kind() {
            "identifier" => imported.contains(syntax::text(callee, source)),
            "attribute" => modules
                .get(field(callee, "object"))
                .is_some_and(|m| spawns(m, field(callee, "attribute"))),
            _ => false,
        };
        if !starts {
            return;
        }
        // The functions around the call, by the line cProfile gives them: a decorated function
        // starts at its first decorator.
        let mut scopes = Vec::new();
        let mut up = n.parent();
        while let Some(p) = up {
            if matches!(
                p.kind(),
                "function_definition" | "class_definition" | "lambda"
            ) {
                let start = p
                    .parent()
                    .filter(|d| d.kind() == "decorated_definition")
                    .unwrap_or(p);
                scopes.push(syntax::line(start));
            }
            up = p.parent();
        }
        scopes.reverse();
        let program = n
            .child_by_field_name("arguments")
            .and_then(|args| {
                let all = syntax::named_children(args);
                all.iter()
                    .find(|a| a.kind() == "keyword_argument" && field(**a, "name") == "args")
                    .and_then(|a| a.child_by_field_name("value"))
                    .or_else(|| {
                        all.first()
                            .copied()
                            .filter(|a| a.kind() != "keyword_argument")
                    })
            })
            .and_then(|a| python_program(a, source));
        out.push(SpawnCall {
            owner: scopes.last().copied(),
            scopes,
            line: syntax::line(n),
            program,
        });
    });
    out
}

/// The program a Python argument names: `"sleep 0.2"`, `["git", "log"]`, `("make",)`.
fn python_program(arg: tree_sitter::Node, source: &str) -> Option<String> {
    use crate::lang::syntax;
    let string = |s: tree_sitter::Node| {
        let parts = syntax::named_children(s);
        let content: String = parts
            .iter()
            .take_while(|p| p.kind() != "interpolation")
            .filter(|p| p.kind() == "string_content")
            .map(|p| syntax::text(*p, source))
            .collect();
        (content, parts.iter().any(|p| p.kind() == "interpolation"))
    };
    match arg.kind() {
        // An argv list: its first item is the executable, spaces and all.
        "list" | "tuple" => {
            let first = *syntax::named_children(arg).first()?;
            let (content, interpolated) = (first.kind() == "string").then(|| string(first))?;
            if interpolated {
                None
            } else {
                whole_program(&content)
            }
        }
        "string" => {
            let (content, interpolated) = string(arg);
            first_word(&content, interpolated)
        }
        _ => None,
    }
}

pub fn markdown(r: &Report) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "# Profile: `{}`\n\n{:.0} ms in all; {:.0}% of it in the repository's own code.\n\n\
         | Function | File | Total | Self | Calls | Unit |\n| --- | --- | ---: | ---: | ---: | --- |\n",
        r.command.join(" "),
        r.total_ms,
        r.repo_pct
    );
    for h in r.hot.iter().take(SHOWN) {
        let _ = writeln!(
            out,
            "| `{}` | `{}:{}` | {:.0}% ({:.0} ms) | {:.0} ms | {} | {} |",
            h.name,
            h.file,
            h.line,
            h.total_pct,
            h.total_ms,
            h.self_ms,
            h.calls.map_or_else(|| "-".into(), |c| c.to_string()),
            h.unit
                .as_deref()
                .map_or_else(|| "-".into(), |u| format!("`{u}`"))
        );
    }
    if let Some(summary) = r.child_summary() {
        let _ = write!(out, "\n## Child processes\n\n{summary} {CHILD_ADVICE}\n");
        if !r.child_processes.is_empty() {
            out.push_str("\n| Command | Called from | Time |\n| --- | --- | ---: |\n");
        }
        for c in &r.child_processes {
            let _ = writeln!(
                out,
                "| {} | `{}` (`{}:{}`) | {:.0}% ({:.0} ms) |",
                c.command
                    .as_deref()
                    .map_or_else(|| "-".into(), |c| format!("`{c}`")),
                c.function,
                c.file,
                c.line,
                c.share,
                c.ms
            );
        }
    }
    if !r.compiled.is_empty() {
        out.push_str(
            "\n## Already compiled\n\nThese run as machine code already; Rust is unlikely to make them \
             much faster:\n\n",
        );
        for c in &r.compiled {
            let _ = writeln!(out, "- `{}` (`{}:{}`): {}", c.name, c.file, c.line, c.by);
        }
    }
    out.push_str("\n## What to port\n\n");
    let mut seen = BTreeMap::new();
    for h in r.hot.iter().take(SHOWN).filter(|h| h.unit.is_some()) {
        if seen.insert(h.name.clone(), ()).is_some() {
            continue;
        }
        let _ = writeln!(
            out,
            "- **`{}`** ({:.0}%): {}",
            h.name, h.total_pct, h.advice
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compiled_code_changes_the_advice() {
        let root = std::env::temp_dir().join(format!("rust-anything-numba-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sim")).unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project]\nname = \"sim\"\n").unwrap();
        std::fs::write(
            root.join("sim/physics.py"),
            "from numba import njit\n\n@njit(cache=True)\ndef step(x, dt):\n    return x + dt\n\n\
             def run(xs):\n    out = []\n    for x in xs:\n        out.append(step(x, 0.1))\n    return out\n\n\
             def other(xs):\n    return [x * 2 for x in xs]\n",
        )
        .unwrap();
        let compiled = compiled_in(&root, Lang::Python);
        assert_eq!(compiled.len(), 1);
        assert_eq!(
            (compiled[0].name.as_str(), compiled[0].line, compiled[0].by),
            ("step", 4, "numba")
        );
        let hot = |name: &str, line: u32| Hot {
            name: name.into(),
            file: "sim/physics.py".into(),
            line,
            unit: None,
            exported: true,
            self_ms: 1.0,
            total_ms: 1.0,
            total_pct: 50.0,
            calls: None,
            ns_per_call: None,
            callers: Vec::new(),
            advice: "Port it.".into(),
            portable: true,
            js_only: None,
            params: vec!["x".into()],
            batch: None,
            child_pct: None,
        };
        let own = compiled_advice(&root, &hot("step", 3), &compiled).unwrap();
        assert!(
            own.starts_with("Already compiled by numba (`sim/physics.py:4`)"),
            "{own}"
        );
        let caller = compiled_advice(&root, &hot("run", 7), &compiled).unwrap();
        assert!(
            caller.starts_with("Port it. It calls `step` compiled by numba"),
            "{caller}"
        );
        assert!(compiled_advice(&root, &hot("other", 13), &compiled).is_none());
        assert!(compiled_in(&root, Lang::Ts).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn functions_bridge_keeps_in_js_are_not_offered_for_porting() {
        let root =
            std::env::temp_dir().join(format!("rust-anything-js-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"demo","type":"module"}"#,
        )
        .unwrap();
        // A typed callback in TypeScript, and the same shape in JS typed by a sibling `.d.ts`.
        std::fs::write(
            root.join("lib.ts"),
            "export function sumBy<T>(items: T[], pick: (item: T) => number): number {\n  \
             let s = 0;\n  for (const it of items) s += pick(it);\n  return s;\n}\n\
             export function double(n: number): number { return n * 2; }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("plug.js"),
            "export function each(items, visit) { for (const it of items) visit(it); }\n\
             export function half(n) { return n / 2; }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("plug.d.ts"),
            "export declare function each<T>(items: T[], visit: (item: T) => void): void;\n\
             export declare function half(n: number): number;\n",
        )
        .unwrap();
        let acc = |total_ms: f64| Acc {
            self_ms: total_ms,
            total_ms,
            ..Default::default()
        };
        let functions: HashMap<Key, Acc> = [
            (("lib.ts".to_string(), "sumBy".to_string(), 1), acc(60.0)),
            (("plug.js".to_string(), "each".to_string(), 1), acc(20.0)),
            (("plug.js".to_string(), "half".to_string(), 2), acc(10.0)),
            (("lib.ts".to_string(), "double".to_string(), 6), acc(5.0)),
        ]
        .into_iter()
        .collect();
        let r = report(
            &root,
            Lang::Ts,
            &["node".into(), "bench.mjs".into()],
            functions,
            0.0,
            100.0,
        );
        std::fs::remove_dir_all(&root).unwrap();

        let hot = |name: &str| r.hot.iter().find(|h| h.name == name).unwrap();
        for name in ["sumBy", "each"] {
            let h = hot(name);
            assert_eq!(
                h.js_only.as_deref(),
                Some("takes or returns a function"),
                "{name}"
            );
            assert!(
                h.advice.starts_with(&format!("Hot, but `bridge` keeps `{name}` in TypeScript/JavaScript: takes or returns a function.")),
                "{}",
                h.advice
            );
        }
        for name in ["half", "double"] {
            let h = hot(name);
            assert_eq!(h.js_only, None, "{name}");
            assert!(h.advice.starts_with("Hot and exported by"), "{}", h.advice);
        }
        // `go` passes over the hottest functions, which `bridge` would keep in JS.
        assert_eq!(crate::go::pick(&r, None).unwrap().function, "half");
        let named = crate::go::pick(&r, Some("sumBy")).unwrap_err().to_string();
        assert!(named.contains("takes or returns a function"), "{named}");
    }

    #[test]
    fn a_python_generator_is_not_offered_for_porting() {
        let root =
            std::env::temp_dir().join(format!("rust-anything-py-js-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("demo")).unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(root.join("demo/__init__.py"), "").unwrap();
        std::fs::write(
            root.join("demo/mod.py"),
            "def squares(xs):\n    for x in xs:\n        yield x * x\n\n\n\
             def collatz(n):\n    steps = 0\n    while n != 1:\n        n = n // 2 if n % 2 == 0 else 3 * n + 1\n        steps += 1\n    return steps\n",
        )
        .unwrap();
        let acc = |total_ms: f64| Acc {
            self_ms: total_ms,
            total_ms,
            ..Default::default()
        };
        let functions: HashMap<Key, Acc> = [
            (
                ("demo/mod.py".to_string(), "squares".to_string(), 1),
                acc(60.0),
            ),
            (
                ("demo/mod.py".to_string(), "collatz".to_string(), 6),
                acc(30.0),
            ),
        ]
        .into_iter()
        .collect();
        let r = report(
            &root,
            Lang::Python,
            &["python3".into(), "bench.py".into()],
            functions,
            0.0,
            100.0,
        );
        std::fs::remove_dir_all(&root).unwrap();

        let squares = r.hot.iter().find(|h| h.name == "squares").unwrap();
        assert!(
            squares
                .js_only
                .as_deref()
                .is_some_and(|r| r.starts_with("generator")),
            "{:?}",
            squares.js_only
        );
        assert!(
            squares
                .advice
                .starts_with("Hot, but `bridge` keeps `squares` in Python: generator"),
            "{}",
            squares.advice
        );
        assert_eq!(crate::go::pick(&r, None).unwrap().function, "collatz");
    }

    #[test]
    fn a_redefined_python_function_is_judged_by_the_definition_that_ran() {
        let root =
            std::env::temp_dir().join(format!("rust-anything-py-redef-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("demo")).unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(root.join("demo/__init__.py"), "").unwrap();
        std::fs::write(
            root.join("demo/mod.py"),
            "def calc(xs):\n    for x in xs:\n        yield x * x\n\n\n\
             def calc(n):\n    steps = 0\n    while n != 1:\n        n = n // 2 if n % 2 == 0 else 3 * n + 1\n        steps += 1\n    return steps\n",
        )
        .unwrap();
        let functions: HashMap<Key, Acc> = [(
            ("demo/mod.py".to_string(), "calc".to_string(), 6),
            Acc {
                self_ms: 90.0,
                total_ms: 90.0,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        let r = report(
            &root,
            Lang::Python,
            &["python3".into(), "bench.py".into()],
            functions,
            0.0,
            100.0,
        );
        std::fs::remove_dir_all(&root).unwrap();

        let calc = r.hot.iter().find(|h| h.name == "calc").unwrap();
        assert_eq!(calc.js_only, None, "{}", calc.advice);
        assert_eq!(crate::go::pick(&r, None).unwrap().function, "calc");
    }

    #[test]
    fn frames_outside_user_code_are_dropped() {
        let root = Path::new("/repo");
        assert_eq!(relative(root, "node:internal/main"), None);
        assert_eq!(relative(root, "file:///repo/node_modules/x/index.js"), None);
        assert_eq!(relative(root, "/usr/lib/python3.13/json/__init__.py"), None);
        assert_eq!(
            relative(root, "file:///repo/src/fuzzy.ts").as_deref(),
            Some("src/fuzzy.ts")
        );
        // A workload script elsewhere is user code, shown with its full path.
        assert_eq!(
            relative(root, "/tmp/bench.ts").as_deref(),
            Some("/tmp/bench.ts")
        );
        assert!(in_repo("src/fuzzy.ts") && !in_repo("/tmp/bench.ts"));
    }

    #[test]
    fn a_relative_script_path_under_the_repository_is_kept() {
        let root = std::env::temp_dir().join(format!("ra-profile-rel-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("bench.py"), "").unwrap();
        assert_eq!(relative(&root, "bench.py").as_deref(), Some("bench.py"));
        assert_eq!(relative(&root, "./bench.py").as_deref(), Some("bench.py"));
        assert_eq!(relative(&root, "missing.py"), None);
        assert_eq!(relative(&root, "<frozen runpy>"), None);
        assert_eq!(relative(&root, "~"), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn node(id: u64, name: &str, url: &str, line: i64, hits: u64, children: &[u64]) -> Value {
        json!({ "id": id, "hitCount": hits, "children": children,
                "callFrame": { "functionName": name, "url": url, "lineNumber": line - 1 } })
    }

    #[test]
    fn node_writes_profile_to_a_temp_path_with_spaces() {
        if !Command::new("node")
            .arg("--version")
            .status()
            .is_ok_and(|s| s.success())
        {
            return;
        }
        let dir = crate::app::temp_dir("profile with spaces").unwrap();
        let root = dir.canonicalize().unwrap();
        let command = ["node".into(), "-e".into(), "console.log('workload')".into()];
        let result = super::node(&root, &command, &root);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(result.unwrap().2 > 0.0);
    }

    #[test]
    fn cpuprofile_times_and_callers() {
        let f = "file:///repo/src/search.ts";
        // (root) -> matched -> (anonymous) -> fuzzyScore -> fuzzyScore (recursion); 1 ms a sample.
        let profile = json!({
            "startTime": 0, "endTime": 10_000, "samples": vec![0; 10],
            "nodes": [
                node(1, "(root)", "", 0, 0, &[2]),
                node(2, "matched", f, 9, 1, &[3]),
                node(3, "", f, 10, 1, &[4]),
                node(4, "fuzzyScore", f, 2, 5, &[5]),
                node(5, "fuzzyScore", f, 2, 3, &[]),
            ],
        });
        let mut functions = HashMap::new();
        let ms = merge_cpuprofile(Path::new("/repo"), &profile, &mut functions, &mut 0.0);
        assert_eq!(ms, 10.0);
        let key = |n: &str, l: u32| ("src/search.ts".to_string(), n.to_string(), l);
        let fuzzy = &functions[&key("fuzzyScore", 2)];
        assert_eq!(
            (fuzzy.self_ms, fuzzy.total_ms),
            (8.0, 8.0),
            "recursion counts once"
        );
        // The caller is the named function, not the arrow callback in between.
        assert_eq!(
            fuzzy.callers.keys().collect::<Vec<_>>(),
            [&key("matched", 9)]
        );
        assert_eq!(functions[&key("matched", 9)].total_ms, 10.0);
        assert!(functions.contains_key(&key("(anonymous):10", 10)));
    }

    #[test]
    fn cpuprofile_blocking_child_processes_go_to_the_caller() {
        let f = "file:///repo/lib.js";
        let cp = "node:child_process";
        // main -> runAll -> execSync -> spawnSync (6 ms waiting); main -> crunch (3 ms);
        // main -> spawn (async: starting it is all the profile sees).
        let profile = json!({
            "startTime": 0, "endTime": 10_000, "samples": vec![0; 10],
            "nodes": [
                node(1, "(root)", "", 0, 0, &[2]),
                node(2, "main", f, 1, 0, &[3, 6, 7]),
                node(3, "runAll", f, 3, 0, &[4]),
                // runAll's `xs.map((x) => execSync(...))`: the wait is runAll's, as callers are.
                node(4, "", f, 4, 0, &[8]),
                node(8, "execSync", cp, 945, 0, &[5]),
                node(5, "spawnSync", cp, 823, 6, &[]),
                node(6, "crunch", f, 11, 3, &[]),
                node(7, "spawn", cp, 700, 1, &[]),
            ],
        });
        let mut functions = HashMap::new();
        merge_cpuprofile(Path::new("/repo"), &profile, &mut functions, &mut 0.0);
        let at = |n: &str, l: u32| &functions[&("lib.js".to_string(), n.to_string(), l)];
        assert_eq!(
            (at("runAll", 3).spawn_ms, at("runAll", 3).waits_ms),
            (6.0, 6.0)
        );
        assert_eq!((at("main", 1).spawn_ms, at("main", 1).waits_ms), (0.0, 6.0));
        assert_eq!(
            at("main", 1).total_ms,
            10.0,
            "the wait is still the caller's time"
        );
        assert_eq!(
            (at("crunch", 11).spawn_ms, at("crunch", 11).waits_ms),
            (0.0, 0.0)
        );
        let callback = at("(anonymous):4", 4);
        assert_eq!((callback.spawn_ms, callback.waits_ms), (0.0, 6.0));
    }

    #[test]
    fn python_waits_on_subprocess_go_to_the_repository_caller() {
        let root = Path::new("/repo");
        let sp = "/usr/lib64/python3.12/subprocess.py";
        let lib = "/repo/.venv/lib/python3.12/site-packages/gitlib/cmd.py";
        let entry = |file: &str, name: &str, line: u64, total: f64, callers: Value| {
            json!({"file": file, "line": line, "name": name, "calls": 1, "self": 0.0,
                   "total": total, "stdlib": file.starts_with("/usr/lib64/python3.12/"),
                   "callers": callers})
        };
        let caller = |file: &str, name: &str, line: u64, total: f64| json!({"file": file, "line": line, "name": name, "calls": 1, "total": total});
        let (bench, tool) = ("/repo/bench.py", "/repo/tool.py");
        let entries = [
            entry(bench, "<module>", 1, 200.0, json!([])),
            entry(
                tool,
                "run_all",
                5,
                90.0,
                json!([caller(bench, "<module>", 1, 90.0)]),
            ),
            entry(
                tool,
                "shell_all",
                12,
                10.0,
                json!([caller(bench, "<module>", 1, 10.0)]),
            ),
            entry(
                sp,
                "run",
                464,
                85.0,
                json!([caller(tool, "run_all", 5, 85.0)]),
            ),
            // Called from `subprocess.run`, not from user code: counted once, through `run`.
            entry(
                sp,
                "communicate",
                1090,
                84.0,
                json!([caller(sp, "run", 464, 84.0)]),
            ),
            entry(
                "~",
                "<built-in method posix.system>",
                0,
                9.0,
                json!([caller(tool, "shell_all", 12, 9.0)]),
            ),
            // Mutual recursion, a wait at the bottom: the wait is not fed around the cycle, and
            // only the function that starts it is known to wait.
            entry(
                tool,
                "even",
                20,
                40.0,
                json!([
                    caller(bench, "<module>", 1, 40.0),
                    caller(tool, "odd", 25, 40.0)
                ]),
            ),
            entry(
                tool,
                "odd",
                25,
                40.0,
                json!([caller(tool, "even", 20, 40.0)]),
            ),
            entry(
                sp,
                "check_output",
                400,
                2.0,
                json!([caller(tool, "even", 20, 2.0)]),
            ),
            // A library between user code and the wait: `fetch` -> `gitlib.execute` ->
            // `with Popen(...)`, which waits in `__exit__`.
            entry(
                tool,
                "fetch",
                30,
                30.0,
                json!([caller(bench, "<module>", 1, 30.0)]),
            ),
            entry(
                lib,
                "execute",
                10,
                30.0,
                json!([caller(tool, "fetch", 30, 30.0)]),
            ),
            entry(
                sp,
                "__exit__",
                1100,
                25.0,
                json!([caller(lib, "execute", 10, 25.0)]),
            ),
            // A user function recursing through a library (`ast.NodeVisitor`): its wait is its
            // own, once.
            entry(
                tool,
                "visit_Call",
                40,
                12.0,
                json!([
                    caller(bench, "<module>", 1, 12.0),
                    caller("/usr/lib64/python3.12/ast.py", "visit", 410, 12.0)
                ]),
            ),
            entry(
                "/usr/lib64/python3.12/ast.py",
                "visit",
                410,
                12.0,
                json!([caller(
                    "/usr/lib64/python3.12/ast.py",
                    "generic_visit",
                    420,
                    12.0
                )]),
            ),
            entry(
                "/usr/lib64/python3.12/ast.py",
                "generic_visit",
                420,
                12.0,
                json!([caller(tool, "visit_Call", 40, 12.0)]),
            ),
            entry(
                sp,
                "run",
                464,
                10.0,
                json!([caller(tool, "visit_Call", 40, 10.0)]),
            ),
            // A repository module that happens to be called `subprocess.py` is user code.
            entry(
                "/repo/pkg/subprocess.py",
                "run",
                3,
                20.0,
                json!([caller(bench, "<module>", 1, 20.0)]),
            ),
        ];
        let (functions, _) = ingest_entries(root, &entries);
        let at = |file: &str, n: &str, l: u32| {
            let a = &functions[&(file.to_string(), n.to_string(), l)];
            (a.spawn_ms, a.waits_ms)
        };
        assert_eq!(at("tool.py", "run_all", 5), (85.0, 85.0));
        assert_eq!(at("tool.py", "shell_all", 12), (9.0, 9.0));
        assert_eq!(at("tool.py", "even", 20), (2.0, 2.0));
        assert_eq!(at("tool.py", "odd", 25), (0.0, 0.0));
        assert_eq!(at("tool.py", "fetch", 30), (25.0, 25.0));
        assert_eq!(at("tool.py", "visit_Call", 40), (10.0, 10.0));
        assert_eq!(
            at("bench.py", "<module>", 1),
            (0.0, 85.0 + 9.0 + 2.0 + 25.0 + 10.0)
        );
        assert_eq!(at("pkg/subprocess.py", "run", 3), (0.0, 0.0));
        assert_eq!(
            functions[&("pkg/subprocess.py".into(), "run".into(), 3)].total_ms,
            20.0
        );
        assert!(
            !functions
                .keys()
                .any(|k| k.0.ends_with("cmd.py") || k.0.contains("lib64")),
            "{:?}",
            functions.keys()
        );
        // Each wait is started by one user function, so the parts add up.
        let started: f64 = functions.values().map(|a| a.spawn_ms).sum();
        assert_eq!(started, 85.0 + 9.0 + 2.0 + 25.0 + 10.0);
    }

    #[test]
    fn time_waiting_on_child_processes_is_reported_and_not_offered_for_porting() {
        let root =
            std::env::temp_dir().join(format!("rust-anything-children-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"demo","type":"module"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("lib.js"),
            "import { execSync } from \"node:child_process\";\n\n\
             export function runAll(n) {\n  let out = 0;\n  for (let i = 0; i < n; i++) {\n    \
             out += execSync(\"sleep 0.2\").length;\n  }\n  return out;\n}\n\n\
             export function crunch(n) {\n  let s = 0;\n  for (let i = 0; i < n; i++) s = (s + i * i) % 1000003;\n  return s;\n}\n",
        )
        .unwrap();
        let build = |wait: f64, cpu: f64| {
            let functions: HashMap<Key, Acc> = [
                (
                    ("lib.js".to_string(), "runAll".to_string(), 3),
                    Acc {
                        total_ms: wait,
                        spawn_ms: wait,
                        waits_ms: wait,
                        ..Default::default()
                    },
                ),
                (
                    ("lib.js".to_string(), "crunch".to_string(), 11),
                    Acc {
                        self_ms: cpu,
                        total_ms: cpu,
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect();
            report(
                &root,
                Lang::Ts,
                &["node".into(), "bench.mjs".into()],
                functions,
                0.0,
                100.0,
            )
        };
        let r = build(40.0, 55.0);
        let r_spawns = build(60.0, 30.0);
        let quiet = build(0.0, 90.0);
        assert_eq!(
            (r.child_pct, r_spawns.child_pct, quiet.child_pct),
            (40.0, 60.0, 0.0)
        );
        std::fs::remove_dir_all(&root).unwrap();

        let c = &r.child_processes[0];
        assert_eq!(
            (
                c.command.as_deref(),
                c.file.as_str(),
                c.line,
                c.function.as_str(),
                c.share
            ),
            (Some("sleep"), "lib.js", 6, "runAll", 40.0)
        );
        let run_all = r.hot.iter().find(|h| h.name == "runAll").unwrap();
        assert_eq!(run_all.child_pct, Some(100.0));
        assert!(run_all.advice.contains(CHILD_ADVICE), "{}", run_all.advice);
        assert_eq!(
            r.hot.iter().find(|h| h.name == "crunch").unwrap().child_pct,
            None
        );
        let md = markdown(&r);
        assert!(
            md.contains("## Child processes\n\n40% of the run waits on child processes (`sleep` from `lib.js:6`). Porting code will not speed this up"),
            "{md}"
        );
        assert!(
            md.contains("| `sleep` | `runAll` (`lib.js:6`) | 40% (40 ms) |"),
            "{md}"
        );
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["child_processes"][0]["command"], "sleep");
        // `go` passes over the waiting function; named, it says why.
        assert_eq!(crate::go::pick(&r, None).unwrap().function, "crunch");
        let named = crate::go::pick(&r, Some("runAll")).unwrap_err().to_string();
        assert!(
            named.contains("100% of its time waiting on child processes"),
            "{named}"
        );
        // When child processes take most of the run, `go` stops unless a function is named.
        let stop = crate::go::pick(&r_spawns, None).unwrap_err().to_string();
        assert!(
            stop.starts_with("60% of the run waits on child processes"),
            "{stop}"
        );
        assert!(stop.contains("--function"), "{stop}");
        assert_eq!(
            crate::go::pick(&r_spawns, Some("crunch")).unwrap().function,
            "crunch"
        );
        // Nothing waits: no section, no field.
        assert!(quiet.child_processes.is_empty());
        assert!(!markdown(&quiet).contains("Child processes"));
        assert!(
            serde_json::to_value(&quiet)
                .unwrap()
                .get("child_processes")
                .is_none()
        );
        assert!(
            serde_json::to_value(&quiet)
                .unwrap()
                .get("child_pct")
                .is_none()
        );
        assert!(
            serde_json::to_value(&quiet).unwrap()["hot"][0]
                .get("child_pct")
                .is_none()
        );
    }

    #[test]
    fn runtime_library_paths_are_matched_by_component() {
        let root = Path::new("/repo");
        assert_eq!(
            relative(root, "/repo/lib64/python3_tools/work.py").as_deref(),
            Some("lib64/python3_tools/work.py")
        );
        assert_eq!(
            relative(root, "/repo/lib/python3_tools/work.py").as_deref(),
            Some("lib/python3_tools/work.py")
        );
        for installed in [
            "/usr/lib64/python3.12/subprocess.py",
            "/usr/lib/python3.9/os.py",
            "/usr/lib/python3/dist-packages/yaml/__init__.py",
            "/usr/lib/python3/apt/package.py",
        ] {
            assert_eq!(relative(root, installed), None, "{installed}");
        }
    }

    #[test]
    fn cpuprofile_waits_with_no_user_frame_still_count_for_the_run() {
        let lib = "file:///repo/node_modules/timers-lib/index.js";
        let cp = "node:child_process";
        // A library's timer callback blocks in execSync for 7 ms; no user frame is on the stack.
        let profile = json!({
            "startTime": 0, "endTime": 10_000, "samples": vec![0; 10],
            "nodes": [
                node(1, "(root)", "", 0, 0, &[2, 6]),
                node(2, "tick", lib, 3, 0, &[3]),
                node(3, "execSync", cp, 945, 0, &[4]),
                node(4, "spawnSync", cp, 823, 7, &[]),
                node(6, "main", "file:///repo/app.js", 1, 0, &[7]),
                node(7, "execSync", cp, 945, 2, &[]),
            ],
        });
        let mut functions = HashMap::new();
        let mut unowned = 0.0;
        merge_cpuprofile(Path::new("/repo"), &profile, &mut functions, &mut unowned);
        assert_eq!(unowned, 7.0, "only the wait no user function owns");
        assert_eq!(
            functions[&("app.js".to_string(), "main".to_string(), 1)].spawn_ms,
            2.0
        );
        let r = report(
            Path::new("/nonexistent-rust-anything"),
            Lang::Ts,
            &["node".into()],
            functions,
            unowned,
            10.0,
        );
        assert_eq!(r.child_pct, 90.0);
    }

    #[test]
    fn python_waits_come_from_the_standard_library_only() {
        let root = Path::new("/repo");
        let sp = "/usr/lib/python3.12/subprocess.py";
        let fake = "/repo/.venv/lib/python3.12/site-packages/runner/subprocess.py";
        let lib = "/repo/.venv/lib/python3.12/site-packages/sched/jobs.py";
        let entry = |file: &str, name: &str, total: f64, stdlib: bool, callers: Value| {
            json!({"file": file, "line": 1, "name": name, "calls": 1, "self": 0.0,
                   "total": total, "stdlib": stdlib, "callers": callers})
        };
        let caller = |file: &str, name: &str, total: f64| json!({"file": file, "line": 1, "name": name, "calls": 1, "total": total});
        let entries = [
            entry("/repo/app.py", "crunch", 50.0, false, json!([])),
            // A dependency's own `subprocess.py` with a CPU-bound `run`.
            entry(
                fake,
                "run",
                45.0,
                false,
                json!([caller("/repo/app.py", "crunch", 45.0)]),
            ),
            // A library job with no user frame above it waits 20 ms.
            entry(lib, "job", 20.0, false, json!([])),
            entry(sp, "run", 20.0, true, json!([caller(lib, "job", 20.0)])),
        ];
        let (functions, unowned) = ingest_entries(root, &entries);
        let crunch = &functions[&("app.py".to_string(), "crunch".to_string(), 1)];
        assert_eq!((crunch.spawn_ms, crunch.waits_ms), (0.0, 0.0));
        assert_eq!(unowned, 20.0);
    }

    #[test]
    fn shared_helpers_and_recursion_do_not_confirm_a_wait() {
        let root = Path::new("/repo");
        let sp = "/usr/lib/python3.12/subprocess.py";
        let entry = |name: &str, total: f64, callers: Value| {
            json!({"file": "/repo/app.py", "line": 1, "name": name, "calls": 1, "self": 0.0,
                   "total": total, "stdlib": false, "callers": callers})
        };
        let caller = |name: &str, total: f64| json!({"file": "/repo/app.py", "line": 1, "name": name, "calls": 1, "total": total});
        let spawn = |total: f64, by: &str| {
            json!({"file": sp, "line": 506, "name": "run", "calls": 1, "self": 0.0,
                   "total": total, "stdlib": true, "callers": [caller(by, total)]})
        };
        let entries = [
            entry("<module>", 400.0, json!([])),
            // `parse` runs `helper` for 80 ms of CPU, `fetch` for 80 ms of waiting.
            entry("parse", 80.0, json!([caller("<module>", 80.0)])),
            entry("fetch", 80.0, json!([caller("<module>", 80.0)])),
            entry(
                "helper",
                160.0,
                json!([caller("parse", 80.0), caller("fetch", 80.0)]),
            ),
            spawn(80.0, "helper"),
        ];
        let (functions, _) = ingest_entries(root, &entries);
        let waits = |n: &str| functions[&("app.py".to_string(), n.to_string(), 1)].waits_ms;
        assert_eq!(waits("helper"), 80.0);
        assert_eq!(waits("parse"), 0.0, "no wait is confirmed under parse");

        // One call of `a` waits 60 ms; `a -> b` and `b -> a` only compute.
        let entries = [
            entry("<module>", 200.0, json!([])),
            entry(
                "a",
                100.0,
                json!([caller("<module>", 100.0), caller("b", 30.0)]),
            ),
            entry("b", 80.0, json!([caller("a", 80.0)])),
            spawn(60.0, "a"),
        ];
        let (functions, _) = ingest_entries(root, &entries);
        let waits = |n: &str| functions[&("app.py".to_string(), n.to_string(), 1)].waits_ms;
        assert_eq!((waits("a"), waits("b")), (60.0, 0.0));
    }

    #[test]
    fn go_looks_past_the_rows_the_report_shows() {
        let root = std::env::temp_dir().join(format!("rust-anything-rows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"demo","type":"module"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("lib.js"),
            "export function parse(text) {\n  return text.split(\",\").length;\n}\n",
        )
        .unwrap();
        let mut functions: HashMap<Key, Acc> = (0..25)
            .map(|i| {
                (
                    ("lib.js".to_string(), format!("wait{i}"), 100 + i),
                    Acc {
                        total_ms: 40.0,
                        waits_ms: 40.0,
                        spawn_ms: if i == 0 { 40.0 } else { 0.0 },
                        ..Default::default()
                    },
                )
            })
            .collect();
        functions.insert(
            ("lib.js".to_string(), "parse".to_string(), 1),
            Acc {
                self_ms: 30.0,
                total_ms: 30.0,
                ..Default::default()
            },
        );
        let r = report(&root, Lang::Ts, &["node".into()], functions, 0.0, 100.0);
        std::fs::remove_dir_all(&root).unwrap();
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["hot"].as_array().unwrap().len(), 25);
        assert!(!markdown(&r).contains("`parse`"));
        assert_eq!(crate::go::pick(&r, None).unwrap().function, "parse");
    }

    #[test]
    fn executable_paths_stay_whole() {
        let js = "import { execFileSync, spawnSync, execSync } from \"node:child_process\";\n\
                  execFileSync(\"/opt/My Tools/worker\", []);\n\
                  spawnSync(\"ffmpeg\", [\"-i\", \"a b\"]);\n\
                  execSync(\"sleep 0.2\");\n\
                  execFileSync(`/opt/${dir}/worker`, []);\n";
        let programs: Vec<Option<String>> = js_spawn_calls("a.js", js)
            .into_iter()
            .map(|c| c.program)
            .collect();
        assert_eq!(
            programs,
            [
                Some("/opt/My Tools/worker".into()),
                Some("ffmpeg".into()),
                Some("sleep".into()),
                None
            ]
        );
        let py = "import subprocess\nsubprocess.run([\"/opt/My Tools/worker\", \"-x\"])\n\
                  subprocess.run(\"git status\", shell=True)\n";
        let programs: Vec<Option<String>> = python_spawn_calls(py)
            .into_iter()
            .map(|c| c.program)
            .collect();
        assert_eq!(
            programs,
            [Some("/opt/My Tools/worker".into()), Some("git".into())]
        );
    }

    #[test]
    fn a_callback_wait_and_a_direct_call_that_disagree_name_no_program() {
        let src = "import { execSync } from \"node:child_process\";\n\
                   export function sync(xs, cold) {\n\
                   \x20 if (cold) execSync(\"git status\");\n\
                   \x20 xs.forEach((x) => execSync(`sleep ${x}`));\n\
                   }\n\
                   export function outer() {\n\
                   \x20 function inner() { return execSync(\"git log\"); }\n\
                   \x20 execSync(\"make\");\n\
                   \x20 return inner();\n\
                   }\n";
        let calls = js_spawn_calls("a.js", src);
        assert_eq!(spawn_site(&calls, "sync", 2), None);
        assert_eq!(
            spawn_site(&calls, "outer", 6),
            Some((8, Some("make".into()))),
            "a named inner function owns its own wait"
        );
        assert_eq!(
            spawn_site(&calls, "inner", 7),
            Some((7, Some("git".into())))
        );
    }

    #[test]
    fn the_program_in_a_command_line() {
        assert_eq!(first_word("sleep 0.2", false).as_deref(), Some("sleep"));
        assert_eq!(
            first_word("TOKEN=abc LANG=C deploy --now", false).as_deref(),
            Some("deploy")
        );
        assert_eq!(
            first_word("TOKEN=abc", false),
            None,
            "never shows an assignment"
        );
        assert_eq!(first_word("TOKEN='abc def' deploy", false), None);
        assert_eq!(first_word("ls ", true).as_deref(), Some("ls"));
        assert_eq!(
            first_word("git", true),
            None,
            "`git${{x}}`: the program is computed"
        );
        assert_eq!(first_word("a|b c", false), None);
        assert_eq!(first_word("$HOME/bin/x run", false), None);
    }

    #[test]
    fn javascript_spawn_calls_and_their_programs() {
        let src = "import { execSync, execFileSync } from \"node:child_process\";\n\
                   import * as cp from \"node:child_process\";\n\
                   execSync(\"make\");\n\
                   export function runAll(xs) {\n\
                   \x20 // old: execSync(\"echo hi\")\n\
                   \x20 const sep = '}';\n\
                   \x20 return xs.map((x) =>\n\
                   \x20   execSync(`sleep ${x}`).length);\n\
                   }\n\
                   export function both() { return execSync(\"git status\") + cp.execSync(\"npm test\"); }\n\
                   export function many(\n\
                   \x20 opts: { cwd: string },\n\
                   ): void {\n\
                   \x20 execFileSync(\n\
                   \x20   \"ffmpeg\",\n\
                   \x20   [\"-i\", opts.cwd],\n\
                   \x20 );\n\
                   \x20 cp.spawnSync(`${opts.cwd}/bin`, []);\n\
                   }\n";
        let calls = js_spawn_calls("lib.ts", src);
        let summary: Vec<(Vec<u32>, u32, Option<&str>)> = calls
            .iter()
            .map(|c| (c.scopes.clone(), c.line, c.program.as_deref()))
            .collect();
        assert_eq!(
            summary,
            [
                (vec![], 3, Some("make")),
                (vec![4, 7], 8, Some("sleep")),
                (vec![10], 10, Some("git")),
                (vec![10], 10, Some("npm")),
                (vec![11], 14, Some("ffmpeg")),
                (vec![11], 18, None),
            ]
        );
    }

    #[test]
    fn python_spawn_calls_and_their_programs() {
        let src = "import os\nimport subprocess as sp\nfrom subprocess import check_output as co\n\n\
                   sp.run([\"make\"])\n\n\
                   def run_all(\n    n: int,\n) -> None:\n    \"\"\"e.g. sp.run([\"echo\"])\"\"\"\n    \
                   sp.run(\n        [\"git\", \"status\"],\n    )\n\n\n\
                   @cache\ndef shell():\n    os.system(\"\"\"sleep 0.1\"\"\")\n    co(args=(\"ls\", \"-l\"))\n    \
                   sp.check_call(f\"{tool} run\")\n    sp.Popen(f\"tar {flags}\")\n    \
                   other.run(\"nope\")\n    run(\"not imported\")\n";
        let calls = python_spawn_calls(src);
        let summary: Vec<(Vec<u32>, u32, Option<&str>)> = calls
            .iter()
            .map(|c| (c.scopes.clone(), c.line, c.program.as_deref()))
            .collect();
        assert_eq!(
            summary,
            [
                (vec![], 5, Some("make")),
                (vec![7], 11, Some("git")),
                (vec![16], 18, Some("sleep")),
                (vec![16], 19, Some("ls")),
                (vec![16], 20, None),
                (vec![16], 21, Some("tar")),
            ]
        );
    }

    #[test]
    fn the_site_of_a_functions_child_process() {
        let root = std::env::temp_dir().join(format!("rust-anything-sites-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("tool.py"),
            "import os, subprocess\n\nsubprocess.run([\"make\"])\n\n\
             def same():\n    subprocess.run([\"git\", \"fetch\"])\n    subprocess.check_output([\"git\", \"log\"])\n\n\
             def mixed(x):\n    os.system(\"make\")\n    subprocess.run(x)\n\n\
             def other(p):\n    return p.check_output()\n",
        )
        .unwrap();
        std::fs::write(
            root.join("lib.js"),
            "import { execSync } from \"node:child_process\";\nexecSync(\"make\");\n\
             export function runAll(xs) {\n  return xs.map((x) => execSync(`sleep ${x}`));\n}\n",
        )
        .unwrap();
        let (py_calls, js_calls) = (
            spawn_calls_in(&root, Lang::Python, "tool.py"),
            spawn_calls_in(&root, Lang::Ts, "lib.js"),
        );
        let py = |name: &str, line| spawn_site(&py_calls, name, line);
        let js = |name: &str, line| spawn_site(&js_calls, name, line);
        assert!(spawn_calls_in(&root, Lang::Python, "/elsewhere/tool.py").is_empty());
        assert!(spawn_calls_in(&root, Lang::Python, "../tool.py").is_empty());
        let got = (
            py("<module>", 1),
            py("same", 5),
            py("mixed", 9),
            py("other", 13),
            js("(anonymous):1", 1),
            js("runAll", 3),
            js("(anonymous):4", 4),
        );
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            got.0,
            Some((3, Some("make".into()))),
            "the top level of a script"
        );
        assert_eq!(
            got.1,
            Some((6, Some("git".into()))),
            "two calls, one program"
        );
        assert_eq!(
            got.2, None,
            "two calls, two programs: the function's line is shown"
        );
        assert_eq!(
            got.3, None,
            "`p.check_output()` is not `subprocess.check_output`"
        );
        assert_eq!(got.4, Some((2, Some("make".into()))));
        assert_eq!(
            got.5,
            Some((4, Some("sleep".into()))),
            "a call in a callback"
        );
        assert_eq!(got.6, Some((4, Some("sleep".into()))));
    }

    #[test]
    fn cpuprofile_recognizes_encoded_paths_in_the_repository() {
        let root = Path::new("/repo/work area");
        let profile = json!({
            "startTime": 0,
            "endTime": 10_000,
            "samples": [1],
            "nodes": [node(1, "hot", "file:///repo/work%20area/step%23one%25.mjs", 0, 1, &[])],
        });
        let mut functions = HashMap::new();
        merge_cpuprofile(root, &profile, &mut functions, &mut 0.0);
        assert!(
            functions.contains_key(&("step#one%.mjs".into(), "hot".into(), 0)),
            "{:?}",
            functions.keys()
        );
        assert_eq!(
            relative(root, "/repo/work area/step%20.mjs").as_deref(),
            Some("step%20.mjs")
        );
    }

    #[test]
    fn finds_the_argument_that_varies_per_item() {
        let root =
            std::env::temp_dir().join(format!("rust-anything-profile-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("search.ts"),
            "export function matched(query: string, all: string[]) {\n  return all.map(key => ({ key, score: fuzzyScore(query, key) }))\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("search.py"),
            "def best(q, keys):\n    for k in keys:\n        yield score(q, k)\n",
        )
        .unwrap();
        let params = ["query".to_string(), "text".to_string()];
        let caller = |file: &str, name: &str| (file.to_string(), name.to_string(), 1);
        assert_eq!(
            batch_param(
                &root,
                &caller("search.ts", "matched"),
                "fuzzyScore",
                &params
            )
            .as_deref(),
            Some("text")
        );
        assert_eq!(
            batch_param(&root, &caller("search.py", "best"), "score", &params).as_deref(),
            Some("text")
        );
        std::fs::write(
            root.join("nested.ts"),
            "function main() {\n  for (const query of queries) {\n    for (const text of items) {\n      if (score(query, text) > 0) hits++\n    }\n  }\n}\n",
        )
        .unwrap();
        assert_eq!(
            batch_param(&root, &caller("nested.ts", "main"), "score", &params).as_deref(),
            Some("text"),
            "the innermost loop runs over the items"
        );
        // `query` is the same for every item: no batch parameter to give.
        assert_eq!(
            batch_param(&root, &caller("search.ts", "matched"), "missing", &params),
            None
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn go_names_and_pprof() {
        assert_eq!(go_name("example.com/strkit/strkit.Slugify"), "Slugify");
        assert_eq!(go_name("testing.(*B).runN"), "runN");
        assert_eq!(
            go_name("example.com/x/pkg.Outer.func2"),
            "(anonymous) in Outer"
        );
        let raw = "Samples:\nsamples/count cpu/nanoseconds\n          2   20000000: 1 2 \n          1   10000000: 3 2 \n\
                   Locations\n     1: 0x1 M=1 golang.org/x/text.Map /home/u/go/pkg/mod/golang.org/x/text/map.go:568:0 s=528\n\
                   \x20            example.com/x/pkg.Slugify /repo/pkg/a.go:21:0 s=18\n\
                   \x20    2: 0x2 M=1 example.com/x/pkg.slugAll /repo/pkg/a_test.go:24:0 s=21\n\
                   \x20    3: 0x3 M=1 example.com/x/pkg.Slugify /repo/pkg/a.go:25:0 s=18\nMappings\n";
        let samples = parse_pprof_raw(Path::new("/repo"), raw);
        assert_eq!(samples.len(), 2);
        let key = |f: &str, n: &str, l: u32| Some((f.to_string(), n.to_string(), l));
        // Root first; the inlined Slugify sits between slugAll and strings.Map (not user code).
        assert_eq!(
            samples[0],
            (
                vec![
                    key("pkg/a_test.go", "slugAll", 21),
                    key("pkg/a.go", "Slugify", 18),
                    None
                ],
                20.0
            )
        );
        let mut functions = HashMap::new();
        merge_samples(&samples, &mut functions);
        let slug = &functions[&("pkg/a.go".to_string(), "Slugify".to_string(), 18)];
        assert_eq!((slug.total_ms, slug.self_ms), (30.0, 10.0));
        assert_eq!(slug.callers.values().map(|c| c.0).sum::<f64>(), 30.0);
    }

    #[test]
    fn java_declarations() {
        let src = "class Strings {\n    public static String slugify(String text, int max) {\n        \
                   String s = slugify(text, 0);\n        return s;\n    }\n}\n";
        assert_eq!(java_declaration(src, "slugify", 4), Some(2));
        assert_eq!(java_declaration(src, "slugify", 1), None);
    }

    #[test]
    fn loop_variables_in_go_java_ruby() {
        let (items, indexes) = loop_variables(
            "for _, t := range texts {\n\tSlugify(t, 30)\n}\nfor i := range xs {}\n\
             for (String w : words) {}\nfor (int j = 0; j < n; j++) {}\ntexts.map { |x| f(x) }\nxs.each do |y, k|\n",
        );
        assert_eq!(items, ["t", "w", "x", "y"]);
        assert_eq!(
            indexes,
            ["_", "i", "j"]
                .into_iter()
                .filter(|v| *v != "_")
                .collect::<Vec<_>>()
        );
    }
}
