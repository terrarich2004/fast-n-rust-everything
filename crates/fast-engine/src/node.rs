//! Node.js backend (JavaScript and TypeScript): V8 CPU profile, call recording through a temporary
//! block appended to the module, a napi-rs crate, and a reversible shim that rebinds the function.

use crate::backend::{self, Profile, Recording};
use crate::hotspot::Hotspot;
use crate::python::DISABLE_ENV;
use crate::state::{Ctx, Port, Verify};
use crate::util::{combined, forward_slashes, output, relative, slug, tail};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const RUNNER: &str = include_str!("node/runner.mjs");
const RECORDER: &str = include_str!("node/recorder.cjs");

pub fn is_node(program: &str) -> bool {
    Runtime::of(program).is_some()
}

/// The JavaScript runtime the app runs on. Bun and Deno run the same ports: they load Node-API
/// addons, and write the same .cpuprofile files as Node with --cpu-prof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Node,
    Bun,
    Deno,
}

impl Runtime {
    pub fn of(program: &str) -> Option<Runtime> {
        match crate::util::program_stem(program).as_str() {
            "node" | "nodejs" => Some(Runtime::Node),
            "bun" => Some(Runtime::Bun),
            "deno" => Some(Runtime::Deno),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Runtime::Node => "node",
            Runtime::Bun => "bun",
            Runtime::Deno => "deno",
        }
    }

    /// The app's own arguments: without the "run" of  bun run app.ts  or  deno run -A app.ts.
    fn app_args(self, args: &[String]) -> &[String] {
        match args.first() {
            Some(a) if a == "run" && self != Runtime::Node => &args[1..],
            _ => args,
        }
    }

    /// Arguments that run the app with fast-anything's options first.
    fn with(self, options: &[&str], args: &[String]) -> Vec<String> {
        let mut out: Vec<String> = if self == Runtime::Deno {
            vec!["run".into()]
        } else {
            vec![]
        };
        out.extend(options.iter().map(|o| o.to_string()));
        out.extend(self.app_args(args).iter().cloned());
        out
    }

    /// Arguments that run a script of fast-anything's (it needs files, env vars and the addon).
    fn script(self, script: &Path) -> Vec<String> {
        let mut out: Vec<String> = if self == Runtime::Deno {
            vec!["run".into(), "-A".into()]
        } else {
            vec![]
        };
        out.push(script.to_string_lossy().into_owned());
        out
    }
}

/// The runtime behind a resolved executable (node unless it's bun or deno).
fn runtime_of(exe: &str) -> Runtime {
    Runtime::of(exe).unwrap_or(Runtime::Node)
}

pub fn resolve(program: &str, root: &Path) -> Result<String> {
    let prog = if program.contains(['/', '\\']) {
        root.join(program)
    } else {
        PathBuf::from(program)
    };
    let runtime = Runtime::of(program).unwrap_or(Runtime::Node);
    let (args, minimum): (&[&str], u32) = match runtime {
        Runtime::Node => (
            &["-p", "process.execPath + '\\n' + process.versions.node"],
            20,
        ),
        Runtime::Bun => (&["-p", "process.execPath + '\\n' + Bun.version"], 1),
        Runtime::Deno => (
            &["eval", "-p", "Deno.execPath() + '\\n' + Deno.version.deno"],
            2,
        ),
    };
    let out = output(
        Command::new(&prog).args(args).current_dir(root),
        runtime.name(),
    )?;
    ensure!(
        out.status.success(),
        "{program} failed:\n{}",
        tail(&combined(&out), 5)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = text.lines();
    let exe = lines.next().unwrap_or_default().trim().to_string();
    let major: u32 = lines
        .next()
        .unwrap_or_default()
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    let label = match runtime {
        Runtime::Node => "Node.js",
        Runtime::Bun => "Bun",
        Runtime::Deno => "Deno",
    };
    ensure!(
        major >= minimum,
        "{label} {minimum} or newer is needed (found {major})"
    );
    Ok(exe)
}

/// The script in "node [flags] script.js args" (or "bun run ...", "deno run ...").
fn entry(args: &[String]) -> Option<&String> {
    let args = if args.first().is_some_and(|a| a == "run") {
        &args[1..]
    } else {
        args
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if matches!(
            a.as_str(),
            "-r" | "--require"
                | "--import"
                | "--loader"
                | "--experimental-loader"
                | "-C"
                | "--conditions"
        ) {
            it.next();
        } else if !a.starts_with('-') {
            return Some(a);
        }
    }
    None
}

/// Split a `package.json` script string into argv tokens, returning `None` if it uses shell operators.
fn split_script_words(cmd: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            None if matches!(c, '&' | '|' | ';' | '>' | '<' | '`') => return None,
            None if c == '$' && chars.peek() == Some(&'(') => return None,
            None => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    Some(words)
}

fn is_env_assignment(word: &str) -> bool {
    let Some((key, _)) = word.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !key.chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn resolve_node_loader(root: &Path, spec: &str) -> Option<String> {
    let script = format!("console.log(import.meta.resolve({spec:?}))");
    Command::new("node")
        .args(["-e", &script])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_pkg_script(root: &Path, script: &str) -> Result<String> {
    let pkg_path = root.join("package.json");
    let text = fs::read_to_string(&pkg_path)
        .with_context(|| format!("couldn't read {} to unwrap `{script}`", pkg_path.display()))?;
    let val: Value = serde_json::from_str(&text)
        .with_context(|| format!("invalid JSON in {}", pkg_path.display()))?;
    val.get("scripts")
        .and_then(|s| s.get(script))
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("script `{script}` not found in {}", pkg_path.display()))
}

fn unwrap_pkg_manager(command: &[String], root: &Path) -> Result<Option<Vec<String>>> {
    let mgr = crate::util::program_stem(command.first().map(String::as_str).unwrap_or_default());
    if !matches!(mgr.as_str(), "npm" | "pnpm" | "yarn") {
        return Ok(None);
    }
    let Some(arg1) = command.get(1).map(String::as_str) else {
        return Ok(None);
    };
    let (script_name, extra_start) = match (mgr.as_str(), arg1) {
        ("npm", "run" | "run-script") => (
            command
                .get(2)
                .map(String::as_str)
                .context("name the script after `npm run`, e.g. `fnr scan -- npm run start`")?,
            3usize,
        ),
        ("npm", "start" | "test") => (arg1, 2usize),
        ("pnpm" | "yarn", "run") => (
            command.get(2).map(String::as_str).with_context(|| {
                format!("name the script after `{mgr} run`, e.g. `fnr scan -- {mgr} run start`")
            })?,
            3usize,
        ),
        ("pnpm" | "yarn", s) if !s.starts_with('-') => (s, 2usize),
        _ => return Ok(None),
    };
    let raw_script = read_pkg_script(root, script_name)?;
    let Some(mut words) = split_script_words(&raw_script) else {
        bail!(
            "`{mgr} {script_name}` runs a compound shell command (`{raw_script}`). Pass the inner command directly instead, e.g. `fnr scan -- node app.js`."
        );
    };
    if words.is_empty() {
        bail!("script `{script_name}` in package.json is empty");
    }
    if let Some(first_env) = words.first().filter(|w| is_env_assignment(w)).cloned() {
        let rest = words[1..].join(" ");
        bail!(
            "`{mgr} {script_name}` sets environment variable `{first_env}`. Set it in your shell and run the inner command directly:\n    {first_env} fnr scan -- {rest}"
        );
    }
    let tool = crate::util::program_stem(&words[0]);
    if matches!(
        tool.as_str(),
        "vite" | "next" | "webpack" | "nuxt" | "astro" | "react-scripts" | "remix" | "svelte-kit"
    ) {
        bail!(
            "`{script_name}` runs a dev bundler (`{tool}`), which wraps your app in a build server. Profile a short script calling the slow function directly (e.g. `fnr scan -- node .fast/bench_driver.ts`), or for HTTP servers run `fnr scan --duration 4 --load \"curl ...\" -- <server-cmd>`. For browser/Vercel code, escalate to WASM (`fnr escalate <id>` + `fnr bridge . <unit> --kind wasm`)."
        );
    }
    let mut tail = &command[extra_start..];
    if tail.first().is_some_and(|a| a == "--") {
        tail = &tail[1..];
    }
    words.extend(tail.iter().cloned());
    if let Some(inner) = unwrap_node_runner(&words, root)? {
        Ok(Some(inner))
    } else {
        Ok(Some(words))
    }
}

/// Unwrap `npm`/`pnpm`/`yarn`, `tsx`, `ts-node`, or `npx [-y] tsx|ts-node|node` into a direct runtime command.
pub fn unwrap_node_runner(command: &[String], root: &Path) -> Result<Option<Vec<String>>> {
    if let Some(unwrapped) = unwrap_pkg_manager(command, root)? {
        return Ok(Some(unwrapped));
    }
    let Some(first) = command.first() else {
        return Ok(None);
    };
    let stem = crate::util::program_stem(first);
    let rest: &[String] = if stem == "npx" {
        let mut slice = &command[1..];
        while slice.first().is_some_and(|a| a == "-y" || a == "--yes") {
            slice = &slice[1..];
        }
        slice
    } else {
        command
    };
    let Some(tool_raw) = rest.first() else {
        return Ok(None);
    };
    let tool = crate::util::program_stem(tool_raw);
    let tail_args = &rest[1..];
    if matches!(tool.as_str(), "node" | "nodejs") && stem == "npx" && !tail_args.is_empty() {
        return Ok(Some(
            std::iter::once("node".to_string())
                .chain(tail_args.iter().cloned())
                .collect(),
        ));
    }
    if tool == "tsx" && !tail_args.is_empty() {
        let mut out = vec!["node".to_string()];
        if let Some(loader_url) = resolve_node_loader(root, "tsx") {
            out.push("--import".into());
            out.push(loader_url);
        }
        out.extend(tail_args.iter().cloned());
        return Ok(Some(out));
    }
    if tool == "ts-node" && !tail_args.is_empty() {
        let mut out = vec!["node".to_string()];
        if let Some(loader_url) = resolve_node_loader(root, "ts-node/esm")
            .or_else(|| resolve_node_loader(root, "ts-node/register"))
        {
            out.push("--import".into());
            out.push(loader_url);
        }
        out.extend(tail_args.iter().cloned());
        return Ok(Some(out));
    }
    Ok(None)
}

/// Extract loader/import flags before the entry script (e.g. `--import ...`) so `verify`/`fuzz`
/// can load TypeScript files with the same module hooks as `scan`.
fn passthrough_flags(args: &[String]) -> Vec<String> {
    let args = if args.first().is_some_and(|a| a == "run") {
        &args[1..]
    } else {
        args
    };
    let mut flags = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if matches!(
            a.as_str(),
            "-r" | "--require"
                | "--import"
                | "--loader"
                | "--experimental-loader"
                | "-C"
                | "--conditions"
        ) {
            flags.push(a.clone());
            if let Some(v) = it.next() {
                flags.push(v.clone());
            }
        } else if a.starts_with("--import=")
            || a.starts_with("--loader=")
            || a.starts_with("--conditions=")
            || a.starts_with("--require=")
        {
            flags.push(a.clone());
        } else if !a.starts_with('-') {
            break;
        }
    }
    flags
}

fn runner(ctx: &Ctx, node: &str, mode: &str, job: Value) -> Result<Output> {
    let script = ctx.fast.join("runner.mjs");
    fs::write(&script, RUNNER)?;
    let job_path = ctx.fast.join(format!("job-{mode}.json"));
    fs::write(&job_path, serde_json::to_vec_pretty(&job)?)?;
    let runtime = runtime_of(node);
    let mut cmd = Command::new(node);
    let extra = if mode != "profile" && ctx.state.command.len() > 1 {
        passthrough_flags(&ctx.state.command[1..])
    } else {
        Vec::new()
    };
    let mut argv = runtime.script(&script);
    if !extra.is_empty() && runtime == Runtime::Node {
        let mut combined_argv = extra;
        combined_argv.extend(argv);
        argv = combined_argv;
    }
    let out = output(
        cmd.args(argv)
            .arg(mode)
            .arg(&job_path)
            .current_dir(&ctx.root)
            .stdin(Stdio::null()),
        runtime.name(),
    )?;
    if !out.status.success() {
        bail!(
            "the {} {mode} step failed ({}):\n{}",
            runtime.name(),
            out.status,
            tail(&combined(&out), 30)
        );
    }
    Ok(out)
}

pub fn profile(ctx: &Ctx, node: &str, args: &[String]) -> Result<Profile> {
    profile_with_options(ctx, node, args, crate::backend::ProfileOptions::default())
}

pub fn profile_with_options(
    ctx: &Ctx,
    node: &str,
    args: &[String],
    opts: crate::backend::ProfileOptions,
) -> Result<Profile> {
    let script = entry(args).context("name the script to run, e.g.  fnr scan -- node app.js")?;
    let dir = ctx.fast.join("cpuprof");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    let runtime = runtime_of(node);
    let dir_text = dir.to_string_lossy().into_owned();
    let mut prof_opts = vec![
        "--cpu-prof".to_string(),
        "--cpu-prof-interval".to_string(),
        "250".to_string(),
        "--cpu-prof-dir".to_string(),
        dir_text,
    ];
    let mut duration_ms: Option<String> = None;
    if let Some(secs) = opts.duration_secs {
        ensure!(secs > 0, "--duration must be at least 1 second");
        let timer = ctx.fast.join("stop-timer.cjs");
        fs::write(
            &timer,
            "const ms = Number(process.env.FNR_PROFILE_DURATION_MS || 0);\n\
             if (ms > 0) {\n\
               const t = setTimeout(() => { process.exitCode = 0; process.exit(0); }, ms);\n\
               if (t && typeof t.unref === 'function') t.unref();\n\
             }\n",
        )?;
        let timer_text = timer.to_string_lossy().into_owned();
        match runtime {
            Runtime::Node => {
                prof_opts.push("--require".to_string());
                prof_opts.push(timer_text);
            }
            Runtime::Bun => {
                prof_opts.push("--preload".to_string());
                prof_opts.push(timer_text);
            }
            Runtime::Deno => {
                bail!(
                    "--duration timed profiling is currently supported for Python, Node.js, and Bun"
                );
            }
        }
        duration_ms = Some((secs * 1000).to_string());
    }
    let prof_refs: Vec<&str> = prof_opts.iter().map(String::as_str).collect();
    let mut cmd = Command::new(node);
    cmd.args(runtime.with(&prof_refs, args))
        .current_dir(&ctx.root)
        .stdin(Stdio::null());
    if let Some(ms) = &duration_ms {
        cmd.env("FNR_PROFILE_DURATION_MS", ms);
    }
    let mut load_driver = crate::util::spawn_load_command(&ctx.root, opts.load_command.as_deref())?;
    let out = output(&mut cmd, runtime.name());
    if let Some(ref mut d) = load_driver {
        d.finish();
    }
    let out = out?;
    ensure!(
        out.status.success(),
        "the app failed ({}):\n{}",
        out.status,
        tail(&combined(&out), 30)
    );
    let result = ctx.fast.join("profile.json");
    let job =
        json!({ "root": ctx.root, "dir": dir, "entry": ctx.root.join(script), "out": result });
    runner(ctx, node, "profile", job)?;
    serde_json::from_str(&fs::read_to_string(&result)?).context("reading the profile")
}

/// The entry script's main block, if its main code sits under one.
const MAIN_GUARD: &[&str] = &[
    "if(import.meta.main)",
    "if(require.main===module)",
    "if(module===require.main)",
];

/// Add a marked block to a module: at the end, or in the entry script just before its main block,
/// so the app's main code already sees it. Function declarations are hoisted, so they exist there.
fn add_marked(text: &str, id: &str, body: &str, entry: bool) -> String {
    if entry {
        backend::insert_marked(text, "//", id, body, |t| {
            backend::line_starting(t, MAIN_GUARD)
        })
    } else {
        backend::append_marked(text, "//", id, body)
    }
}

/// Re-run the app with a recorder wrapped around the function. The module gets a temporary
/// block appended (a copy waits in .fast/backup) and is restored byte for byte afterwards.
pub fn record(
    ctx: &Ctx,
    node: &str,
    args: &[String],
    hs: &Hotspot,
    out: &Path,
    limit: usize,
) -> Result<Recording> {
    let file = ctx.root.join(&hs.file);
    let original = fs::read_to_string(&file).with_context(|| format!("reading {}", hs.file))?;
    let backup_dir = ctx.fast.join("backup");
    fs::create_dir_all(&backup_dir)?;
    let backup = backup_dir.join(slug(&hs.file));
    fs::write(&backup, &original)?;

    let f = &target(&hs.name, hs.owner.as_deref(), hs.instance_method);
    let wrap =
        format!("{f} = globalThis.__fastAnythingWrap ? globalThis.__fastAnythingWrap({f}) : {f};");
    let entry = hs.module.as_deref() == Some("__main__");
    let reassignable = redeclare(&original, "const", "let", f)
        .or_else(|| redeclare_fn_as_let(&original, f))
        .unwrap_or_else(|| original.clone());
    fs::write(
        &file,
        add_marked(&reassignable, &format!("recording {}", hs.id), &wrap, entry),
    )?;
    let recorder = ctx.fast.join("recorder.cjs");
    fs::write(&recorder, RECORDER)?;
    let meta = out.with_extension("meta.json");
    let runtime = runtime_of(node);
    let recorder_text = recorder.to_string_lossy().into_owned();
    let mut options: Vec<&str> = match runtime {
        Runtime::Node => vec!["--require", &recorder_text],
        Runtime::Bun | Runtime::Deno => vec!["--preload", &recorder_text],
    };
    if runtime == Runtime::Deno {
        // the recorder reads its settings from env vars and writes its results to files; Deno
        // rejects a permission given twice, or next to -A
        let given = |flag: &str| {
            args.iter()
                .any(|a| a == "-A" || a == "--allow-all" || a.starts_with(flag))
        };
        options.extend(
            ["--allow-env", "--allow-read", "--allow-write"]
                .into_iter()
                .filter(|f| !given(f)),
        );
    }
    let mut cmd = Command::new(node);
    cmd.args(runtime.with(&options, args))
        .current_dir(&ctx.root)
        .env("FAST_ANYTHING_RECORD_OUT", out)
        .env("FAST_ANYTHING_RECORD_META", &meta)
        .env("FAST_ANYTHING_RECORD_LIMIT", limit.to_string())
        .env(DISABLE_ENV, "1")
        .stdin(Stdio::null());
    if hs.instance_method {
        cmd.env(
            "FAST_ANYTHING_SELF_FIELDS",
            serde_json::to_string(&hs.self_fields)?,
        );
    } else {
        cmd.env_remove("FAST_ANYTHING_SELF_FIELDS");
    }
    if let Some(secs) = ctx.state.duration_secs {
        cmd.env("FNR_RECORD_DURATION_MS", (secs * 1000).to_string());
    } else {
        cmd.env_remove("FNR_RECORD_DURATION_MS");
    }
    let mut load_driver =
        crate::util::spawn_load_command(&ctx.root, ctx.state.load_command.as_deref())?;
    let run = output(&mut cmd, runtime.name());
    if let Some(ref mut d) = load_driver {
        d.finish();
    }
    fs::write(&file, &original)
        .with_context(|| format!("restoring {} (a copy is in {})", hs.file, backup.display()))?;
    let _ = fs::remove_file(&backup);
    let run = run?;
    ensure!(
        run.status.success(),
        "the app failed while recording ({}):\n{}",
        run.status,
        tail(&combined(&run), 30)
    );
    serde_json::from_str(&fs::read_to_string(&meta)?).context("reading the recording summary")
}

// ------------------------------------------------------------------ the napi-rs crate

fn split_union(s: &str) -> Vec<String> {
    let (mut depth, mut cur, mut out) = (0i32, String::new(), vec![]);
    for c in s.chars() {
        match c {
            '<' | '[' | '(' | '{' => depth += 1,
            '>' | ']' | ')' | '}' => depth -= 1,
            '|' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur.trim().to_string());
    out.into_iter().filter(|p| !p.is_empty()).collect()
}

const TYPED_ARRAYS: &[&str] = &[
    "Float64Array",
    "Float32Array",
    "Int32Array",
    "Uint32Array",
    "Int16Array",
    "Uint16Array",
    "Int8Array",
    "Uint8Array",
    "Uint8ClampedArray",
];

/// Rust type for a TypeScript annotation, if there's an obvious one.
pub fn rust_type(annotation: &str) -> Option<String> {
    rust_type_with_records(annotation, &BTreeSet::new())
}

fn rust_type_with_records(annotation: &str, records: &BTreeSet<String>) -> Option<String> {
    let a = annotation.split_whitespace().collect::<Vec<_>>().join(" ");
    let parts = split_union(&a);
    let solid: Vec<&String> = parts
        .iter()
        .filter(|p| *p != "null" && *p != "undefined")
        .collect();
    if solid.len() < parts.len() {
        return match solid.as_slice() {
            [one] => rust_type_with_records(one, records).map(|t| format!("Option<{t}>")),
            _ => None,
        };
    }
    if parts.len() != 1 {
        return None;
    }
    let a = a.trim();
    if records.contains(a) {
        return Some(a.to_string());
    }
    Some(match a {
        "number" => "f64".into(),
        "string" => "String".into(),
        "boolean" => "bool".into(),
        "void" | "undefined" => "()".into(),
        "Buffer" => "Buffer".into(),
        t if TYPED_ARRAYS.contains(&t) => t.into(),
        _ => {
            let inner = a
                .strip_suffix("[]")
                .map(|x| x.trim_start_matches("readonly ").to_string())
                .or_else(|| {
                    a.strip_prefix("Array<")
                        .and_then(|x| x.strip_suffix('>'))
                        .map(String::from)
                })
                .or_else(|| {
                    a.strip_prefix("ReadonlyArray<")
                        .and_then(|x| x.strip_suffix('>'))
                        .map(String::from)
                });
            let set_inner = a.strip_prefix("Set<").and_then(|x| x.strip_suffix('>'));
            let tuple = a
                .strip_prefix('[')
                .and_then(|x| x.strip_suffix(']'))
                .map(|x| {
                    x.split(',')
                        .map(|p| rust_type_with_records(p.trim(), records))
                        .collect::<Option<Vec<String>>>()
                });
            if let Some(Some(parts)) = tuple {
                // A JS tuple is an array; a same-typed one maps to a Vec.
                if parts.is_empty() || parts.iter().any(|p| p != &parts[0]) {
                    return None;
                }
                format!("Vec<{}>", parts[0])
            } else if let Some(inner) = inner {
                format!("Vec<{}>", rust_type_with_records(&inner, records)?)
            } else if let Some(inner) = set_inner {
                // napi converts a JS Set into a HashSet, whose items must hash: strings yes, f64 no
                let inner =
                    rust_type_with_records(inner.trim(), records).filter(|t| t == "String")?;
                format!("std::collections::HashSet<{inner}>")
            } else {
                // a JS Map would arrive empty (napi's HashMap reads a plain object's own keys), so
                // only Record<string, T> maps to one
                let v = a
                    .strip_prefix("Record<string,")
                    .and_then(|x| x.strip_suffix('>'))?;
                format!(
                    "std::collections::HashMap<String, {}>",
                    rust_type_with_records(v.trim(), records)?
                )
            }
        }
    })
}

fn snake(name: &str) -> String {
    let mut s = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                s.push('_');
            }
            s.push(c.to_ascii_lowercase());
        } else if c == '$' {
            s.push('_');
        } else {
            s.push(c);
        }
    }
    match s.as_str() {
        "type" | "match" | "fn" | "ref" | "move" | "loop" | "mod" | "use" | "impl" | "trait"
        | "struct" | "enum" | "where" | "mut" | "self" | "super" | "crate" | "box" | "gen"
        | "dyn" | "unsafe" | "extern" | "pub" | "as" | "in" => {
            format!("r#{s}")
        }
        _ => s,
    }
}

fn contains_ident(text: &str, name: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut start = 0;
    while let Some(pos) = text[start..].find(name) {
        let at = start + pos;
        let before_ok = at == 0 || !text[..at].chars().next_back().is_some_and(ident);
        let after = at + name.len();
        let after_ok = after == text.len() || !text[after..].chars().next().is_some_and(ident);
        if before_ok && after_ok {
            return true;
        }
        start = at + name.len();
    }
    false
}

fn mentions_type(text: Option<&str>, name: &str) -> bool {
    text.is_some_and(|t| contains_ident(t, name))
}

fn record_structs(hs: &Hotspot) -> (BTreeSet<String>, String) {
    let mut active = BTreeSet::new();
    for name in hs.records.keys() {
        let used = hs
            .params
            .iter()
            .any(|p| mentions_type(p.annotation.as_deref(), name))
            || mentions_type(hs.returns.as_deref(), name);
        if used {
            active.insert(name.clone());
        }
    }
    // Pull in transitively referenced record types.
    loop {
        let mut added = false;
        let current: Vec<String> = active.iter().cloned().collect();
        for name in current {
            if let Some(fields) = hs.records.get(&name) {
                for cand in hs.records.keys() {
                    if !active.contains(cand)
                        && fields
                            .iter()
                            .any(|f| mentions_type(f.annotation.as_deref(), cand))
                    {
                        active.insert(cand.clone());
                        added = true;
                    }
                }
            }
        }
        if !added {
            break;
        }
    }
    // Keep only structs whose every field resolves cleanly.
    loop {
        let before = active.len();
        let snapshot = active.clone();
        active.retain(|name| {
            hs.records.get(name).is_some_and(|fields| {
                !fields.is_empty()
                    && fields.iter().all(|f| {
                        f.annotation
                            .as_deref()
                            .and_then(|a| rust_type_with_records(a, &snapshot))
                            .is_some()
                    })
            })
        });
        if active.len() == before {
            break;
        }
    }
    let mut defs = String::new();
    for name in &active {
        let Some(fields) = hs.records.get(name) else {
            continue;
        };
        defs.push_str(&format!(
            "#[napi(object)]\n#[derive(Clone, Debug)]\npub struct {name} {{\n"
        ));
        for f in fields {
            let Some(ty) = f
                .annotation
                .as_deref()
                .and_then(|a| rust_type_with_records(a, &active))
            else {
                continue;
            };
            let rs_field = snake(&f.name);
            if rs_field != f.name {
                defs.push_str(&format!("    #[napi(js_name = \"{}\")]\n", f.name));
            }
            defs.push_str(&format!("    pub {rs_field}: {ty},\n"));
        }
        defs.push_str("}\n\n");
    }
    (active, defs)
}

fn infer_recorded_type(values: &[&Value]) -> Option<String> {
    let mut has_null_or_undef = false;
    let mut concrete = Vec::new();
    for v in values {
        if v.is_null() || v.get("$undefined").is_some() {
            has_null_or_undef = true;
        } else {
            concrete.push(*v);
        }
    }
    if concrete.is_empty() {
        return None;
    }
    let base = if concrete
        .iter()
        .all(|v| v.is_f64() || v.is_i64() || v.is_u64())
    {
        "f64".to_string()
    } else if concrete.iter().all(|v| v.is_string()) {
        "String".to_string()
    } else if concrete.iter().all(|v| v.is_boolean()) {
        "bool".to_string()
    } else if concrete.iter().all(|v| v.is_array()) {
        let elems: Vec<&Value> = concrete
            .iter()
            .filter_map(|v| v.as_array())
            .flatten()
            .collect();
        if elems.is_empty() {
            return None;
        }
        if elems.iter().all(|v| v.is_f64() || v.is_i64() || v.is_u64()) {
            "Vec<f64>".to_string()
        } else if elems.iter().all(|v| v.is_string()) {
            "Vec<String>".to_string()
        } else if elems.iter().all(|v| v.is_boolean()) {
            "Vec<bool>".to_string()
        } else {
            return None;
        }
    } else {
        return None;
    };
    Some(if has_null_or_undef {
        format!("Option<{base}>")
    } else {
        base
    })
}

fn infer_types_from_recordings(
    recordings: Option<&Path>,
    param_count: usize,
) -> (Vec<Option<String>>, Option<String>) {
    let Some(path) = recordings else {
        return (vec![None; param_count], None);
    };
    let Ok(text) = fs::read_to_string(path) else {
        return (vec![None; param_count], None);
    };
    let cases: Vec<Value> = text
        .lines()
        .take(100)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let mut param_types = Vec::with_capacity(param_count);
    for i in 0..param_count {
        let vals: Vec<&Value> = cases
            .iter()
            .filter_map(|c| c.get("args")?.as_array()?.get(i))
            .collect();
        param_types.push(infer_recorded_type(&vals));
    }
    let ret_vals: Vec<&Value> = cases.iter().filter_map(|c| c.get("result")).collect();
    (param_types, infer_recorded_type(&ret_vals))
}

pub fn lib_rs(hs: &Hotspot, port: &Port) -> String {
    lib_rs_with_recordings(hs, port, None)
}

fn lib_rs_with_recordings(hs: &Hotspot, port: &Port, recordings: Option<&Path>) -> String {
    let (records, struct_defs) = record_structs(hs);
    let (inferred_params, inferred_ret) = infer_types_from_recordings(recordings, hs.params.len());
    let params: Vec<String> = hs
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let ty = p
                .annotation
                .as_deref()
                .and_then(|a| rust_type_with_records(a, &records))
                .or_else(|| inferred_params.get(i).cloned().flatten())
                .unwrap_or_else(|| "serde_json::Value".into());
            format!("{}: {ty}", snake(&p.name))
        })
        .collect();
    let ret = hs
        .returns
        .as_deref()
        .and_then(|a| rust_type_with_records(a, &records))
        .or(inferred_ret)
        .unwrap_or_else(|| "serde_json::Value".into());
    format!(
        "//! Rust port of {id} ({file}:{line}), generated by fast-n-rust.\n\
         //! Replace the todo!() with a faithful, fast port. TASK.md has the original and real calls.\n\
         //! Check it with:  fnr verify {id}\n\n\
         use napi::bindgen_prelude::*;\n\
         use napi_derive::napi;\n\n\
         {struct_defs}\
         #[napi(js_name = \"{js}\", catch_unwind)]\n\
         #[allow(unused_variables)]\n\
         pub fn {rust}({params}) -> Result<{ret}> {{\n    todo!(\"port {id}\")\n}}\n",
        id = port.id,
        file = hs.file,
        line = hs.line,
        js = hs.name,
        rust = snake(&hs.name),
        params = params.join(", "),
    )
}

fn cargo_toml(port: &Port) -> String {
    format!(
        "# Rust port of {id}, generated by fast-n-rust.\n\
         [package]\nname = \"{ext}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n\
         [lib]\nname = \"{ext}\"\ncrate-type = [\"cdylib\"]\n\n\
         [dependencies]\nnapi = {{ version = \"3\", features = [\"serde-json\"] }}\nnapi-derive = \"3\"\nserde_json = \"1\"\n\n\
         [build-dependencies]\nnapi-build = \"2\"\n\n{profile}",
        id = port.id,
        ext = port.ext,
        profile = backend::RELEASE_PROFILE,
    )
}

/// A recorded value the way JavaScript would print it.
fn js_value(v: &Value) -> String {
    match v {
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(js_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => match map.iter().next() {
            Some((tag, Value::Array(pairs))) if tag == "$object" => {
                let inner: Vec<String> = pairs
                    .iter()
                    .filter_map(|p| p.as_array())
                    .filter(|p| p.len() == 2)
                    .map(|p| format!("{}: {}", p[0].as_str().unwrap_or("?"), js_value(&p[1])))
                    .collect();
                format!("{{ {} }}", inner.join(", "))
            }
            Some((tag, x)) if tag == "$typed" => {
                format!("{}{}", x[0].as_str().unwrap_or("TypedArray"), x[1])
            }
            Some((tag, x)) if tag == "$bigint" => format!("{}n", x.as_str().unwrap_or("0")),
            Some((tag, _)) if tag == "$undefined" => "undefined".into(),
            Some((tag, x)) if tag == "$float" => x.as_str().unwrap_or("NaN").to_string(),
            Some((tag, Value::Array(items))) if tag == "$set" => {
                format!(
                    "Set {{{}}}",
                    items.iter().map(js_value).collect::<Vec<_>>().join(", ")
                )
            }
            Some((tag, Value::Array(pairs))) if tag == "$map" => {
                let inner: Vec<String> = pairs
                    .iter()
                    .filter_map(|p| p.as_array())
                    .filter(|p| p.len() == 2)
                    .map(|p| format!("{} => {}", js_value(&p[0]), js_value(&p[1])))
                    .collect();
                format!("Map {{ {} }}", inner.join(", "))
            }
            _ => v.to_string(),
        },
        _ => v.to_string(),
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

fn sample_calls(path: &Path, func: &str, n: usize) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return vec![];
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .take(n)
        .map(|case| {
            let args: Vec<String> = case["args"]
                .as_array()
                .map(|a| a.iter().map(js_value).collect())
                .unwrap_or_default();
            let outcome = match case.get("raises") {
                Some(e) => format!("throws {}", e.as_str().unwrap_or("an error")),
                None => js_value(&case["result"]),
            };
            format!(
                "{}  ->  {}",
                clip(&format!("{func}({})", args.join(", ")), 110),
                clip(&outcome, 60)
            )
        })
        .collect()
}

pub fn task_md(hs: &Hotspot, helpers: &[Hotspot], port: &Port, crate_dir: &Path) -> String {
    let fence = "```";
    let lang = if hs.file.ends_with(".ts") || hs.file.ends_with(".mts") || hs.file.ends_with(".cts")
    {
        "ts"
    } else {
        "js"
    };
    let samples = sample_calls(&crate_dir.join("recordings.jsonl"), &hs.name, 8).join("\n");
    let mut notes: String = hs
        .warnings
        .iter()
        .map(|w| format!("- Note: {w}.\n"))
        .collect();
    if hs.instance_method {
        let fields = if hs.self_fields.is_empty() {
            "no `this` fields".to_string()
        } else {
            hs.self_fields
                .iter()
                .map(|f| format!("`this.{f}` -> `this_{f}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        notes.push_str(&format!(
            "- Note: `{}` is a read-only instance method; lifted {fields}.\n",
            hs.qualname
        ));
    }
    format!(
        "# Port `{id}` to Rust\n\n\
You're in a Rust crate (napi-rs) fast-n-rust (`fnr`) generated for this: `{dir}`.\n\
Write `{func}` in `src/lib.rs` so it behaves **exactly** like the JavaScript below, and runs fast.\n\
Only edit files in this folder. Don't touch the JavaScript project.\n\n\
## The original ({file}:{line})\n\n{fence}{lang}\n{source}\n{fence}\n\n\
{helpers}## Real calls\n\n\
fnr recorded {recorded} real calls (out of {seen} while the app ran). A few:\n\n{fence}\n{samples}\n{fence}\n\n\
## Rules\n\n\
- Same result for every input, edge cases included: empty input, zero, negatives, NaN and infinity, non-ASCII text, very long input. verify tests these too.\n\
- JavaScript strings are UTF-16: `.length`, indexing and `charCodeAt` count UTF-16 code units. \
Use `s.encode_utf16()` for anything index- or length-based (bytes are fine after checking `is_ascii()`).\n\
- JavaScript numbers are f64. Integer math is exact only up to 2^53; use `f64` unless the values are clearly small integers.\n\
- Results are compared with a relative tolerance of 1e-9. Keep the same order of float operations.\n\
- Where JavaScript throws, return `Err(Error::from_reason(...))`.\n\
- Each call crosses from JavaScript into Rust: keep conversions cheap, don't clone big inputs.\n\
- Make it fast: no needless allocation, borrow, `with_capacity`, fast paths for common cases.\n\
- Keep `#[napi(js_name = \"{func}\", catch_unwind)]` as it is.\n\
{notes}\n\
## Check\n\n\
Run `cargo check` here. Then fnr builds the crate in release mode and replays all {recorded} recorded calls against it\n\
(`fnr verify {id}`).\n",
        id = port.id,
        dir = crate_dir.display(),
        func = hs.name,
        file = hs.file,
        line = hs.line,
        source = hs.source,
        recorded = port.recorded,
        seen = crate::util::thousands(port.calls_seen),
        helpers = helpers_md(hs, helpers),
    )
}

pub fn scaffold(ctx: &Ctx, hs: &Hotspot, helpers: &[Hotspot], port: &Port) -> Result<String> {
    let dir = ctx.crate_dir(port);
    fs::create_dir_all(dir.join("src"))?;
    fs::create_dir_all(dir.join(".cargo"))?;
    fs::write(dir.join("Cargo.toml"), cargo_toml(port))?;
    fs::write(
        dir.join("build.rs"),
        "fn main() {\n    napi_build::setup();\n}\n",
    )?;
    fs::write(dir.join(".cargo/config.toml"), backend::CARGO_CONFIG)?;
    let lib = dir.join("src/lib.rs");
    if !lib.exists() {
        let rec_path = dir.join("recordings.jsonl");
        fs::write(&lib, lib_rs_with_recordings(hs, port, Some(&rec_path)))?;
    }
    let task = task_md(hs, helpers, port, &dir);
    fs::write(dir.join("TASK.md"), &task)?;
    Ok(task)
}

/// TASK.md section listing the same-file functions the target calls, to port alongside it.
pub fn helpers_md(hs: &Hotspot, helpers: &[Hotspot]) -> String {
    if hs.calls.is_empty() {
        return String::new();
    }
    let mut out = "## Helpers it calls\n\nPort these as private Rust functions in the same crate (they stay in JavaScript for everyone else).\n\n".to_string();
    for name in &hs.calls {
        match helpers.iter().find(|h| &h.name == name) {
            Some(h) => out.push_str(&format!("```js\n{}\n```\n\n", h.source)),
            None => out.push_str(&format!("- `{name}`: read it in {}\n", hs.file)),
        }
    }
    out
}

pub fn verify(ctx: &Ctx, port: &Port) -> Result<Verify> {
    verify_with(ctx, port, None, None)
}

pub fn verify_with(
    ctx: &Ctx,
    port: &Port,
    float_tolerance: Option<f64>,
    known_diffs: Option<&Path>,
) -> Result<Verify> {
    let node = ctx.runtime()?;
    let dir = ctx.crate_dir(port);
    let out = dir.join("verify.json");
    let default_kd = dir.join("known-diffs.jsonl");
    let kd = known_diffs
        .map(Path::to_path_buf)
        .or_else(|| default_kd.is_file().then_some(default_kd));
    let job = json!({
        "addon": ctx.lib_dir().join(format!("{}.node", port.ext)),
        "js_name": port.func,
        "func": port.func,
        "owner": port.owner,
        "instance_method": port.instance_method,
        "self_fields": port.self_fields,
        "module_file": ctx.root.join(&port.file),
        "cases": dir.join("recordings.jsonl"),
        "out": out,
        "budget": 0.5,
        "float_tolerance": float_tolerance,
        "known_diffs": kd,
    });
    // The original may not be exported: a block appended for this run hands it to the runner from
    // inside the module, then the file goes back byte for byte.
    let file = ctx.root.join(&port.file);
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    let hand_over = format!(
        "if (globalThis.__fastAnythingOriginal) globalThis.__fastAnythingOriginal({});",
        target(&port.func, port.owner.as_deref(), port.instance_method)
    );
    let mut patches = crate::native::Patches::new(ctx)?;
    patches.write(
        &file,
        &backend::append_marked(&text, "//", &format!("verifying {}", port.id), &hand_over),
    )?;
    let run = runner(ctx, node, "verify", job);
    patches.restore()?;
    run?;
    let mut v: Verify =
        serde_json::from_str(&fs::read_to_string(&out)?).context("reading verify results")?;
    v.built = true;
    Ok(v)
}

pub fn fuzz_cases(
    ctx: &Ctx,
    port: &Port,
    cases_path: &Path,
    out_path: &Path,
    float_tolerance: Option<f64>,
) -> Result<crate::native::NativeFuzzReport> {
    let node = ctx.runtime()?;
    let file = ctx.root.join(&port.file);
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    let hand_over = format!(
        "if (globalThis.__fastAnythingOriginal) globalThis.__fastAnythingOriginal({});",
        target(&port.func, port.owner.as_deref(), port.instance_method)
    );
    let job = json!({
        "addon": ctx.lib_dir().join(format!("{}.node", port.ext)),
        "js_name": port.func,
        "func": port.func,
        "owner": port.owner,
        "instance_method": port.instance_method,
        "self_fields": port.self_fields,
        "module_file": file,
        "cases": cases_path,
        "out": out_path,
        "float_tolerance": float_tolerance,
    });
    let mut patches = crate::native::Patches::new(ctx)?;
    patches.write(
        &file,
        &backend::append_marked(&text, "//", &format!("verifying {}", port.id), &hand_over),
    )?;
    let run = runner(ctx, node, "fuzz", job);
    patches.restore()?;
    run?;
    serde_json::from_str(&fs::read_to_string(out_path)?).context("reading node fuzz results")
}

// ------------------------------------------------------------------ the shim

/// ES module or CommonJS, the way Node decides it.
fn is_esm(root: &Path, file: &Path) -> bool {
    match file.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "mjs" | "mts" => return true,
        "cjs" | "cts" => return false,
        _ => {}
    }
    let mut dir = file.parent();
    while let Some(d) = dir {
        if let Ok(text) = fs::read_to_string(d.join("package.json")) {
            return serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["type"].as_str().map(|t| t == "module"))
                .unwrap_or(false);
        }
        if d == root {
            break;
        }
        dir = d.parent();
    }
    false
}

/// How the module refers to the function: its name, Class.name for a static method, or Class.prototype.name for an instance method.
fn target(name: &str, owner: Option<&str>, instance_method: bool) -> String {
    match (owner, instance_method) {
        (Some(class), true) => format!("{class}.prototype.{name}"),
        (Some(class), false) => format!("{class}.{name}"),
        (None, _) => name.to_string(),
    }
}

fn is_ts_file(path: &str) -> bool {
    matches!(
        Path::new(path).extension().and_then(|e| e.to_str()),
        Some("ts" | "mts" | "cts" | "tsx")
    )
}

pub fn shim(port: &Port, addon_rel: &str, esm: bool) -> String {
    let addon_name = addon_rel.rsplit('/').next().unwrap_or(addon_rel);
    let target_expr = target(&port.func, port.owner.as_deref(), port.instance_method);
    let req_alias = format!("_fnr_req_{}", port.ext);
    let res_alias = format!("_fnr_res_{}", port.ext);
    let (load_default, load_override) = if esm {
        (
            format!(
                "{req_alias}(import.meta.url)(\"{addon_rel}\").{}",
                port.func
            ),
            format!(
                "{req_alias}(import.meta.url)({res_alias}(process.env.FNR_LIB_DIR, \"{addon_name}\")).{}",
                port.func
            ),
        )
    } else {
        (
            format!("require(\"{addon_rel}\").{}", port.func),
            format!(
                "require(require(\"node:path\").resolve(process.env.FNR_LIB_DIR, \"{addon_name}\")).{}",
                port.func
            ),
        )
    };
    let assign_lines = if port.instance_method {
        let field_args = port
            .self_fields
            .iter()
            .map(|f| format!("this.{f}"))
            .collect::<Vec<_>>();
        let call_args = if field_args.is_empty() {
            "..._args".to_string()
        } else {
            format!("{}, ..._args", field_args.join(", "))
        };
        let fn_sig = if is_ts_file(&port.file) {
            "function (this: any, ..._args: any[])"
        } else {
            "function (..._args)"
        };
        vec![
            format!(
                "      const _fnrFn = process.env.FNR_LIB_DIR ? {load_override} : {load_default};"
            ),
            format!("      {target_expr} = {fn_sig} {{"),
            format!("        return _fnrFn({call_args});"),
            "      };".to_string(),
        ]
    } else {
        vec![
            "      if (process.env.FNR_LIB_DIR) {".to_string(),
            format!("        {target_expr} = {load_override};"),
            "      } else {".to_string(),
            format!("        {target_expr} = {load_default};"),
            "      }".to_string(),
        ]
    };
    let mut lines = vec![
        "// Uses the verified Rust port. FNR_DISABLE=1 (or FAST_ANYTHING_DISABLE=1) runs the JavaScript above instead.".to_string(),
        format!("// Undo with:  fnr disable {}", port.id),
    ];
    if esm {
        lines.push(format!(
            "import {{ createRequire as {req_alias} }} from \"node:module\";"
        ));
        lines.push(format!(
            "import {{ resolve as {res_alias} }} from \"node:path\";"
        ));
    }
    lines.extend([
        "{".to_string(),
        "  const _fnrDis = String(process.env.FNR_DISABLE || process.env.FAST_ANYTHING_DISABLE || \"\").trim().toLowerCase();".to_string(),
        "  if (!_fnrDis || _fnrDis === \"0\" || _fnrDis === \"false\") {".to_string(),
        "    try {".to_string(),
    ]);
    lines.extend(assign_lines);
    lines.extend([
        "    } catch (_err) {".to_string(),
        "      if (process.env.FNR_STRICT === \"1\" || process.env.FNR_STRICT === \"true\") {".to_string(),
        "        const _fnrMsg = _err instanceof Error ? _err.message : String(_err);".to_string(),
        format!(
            "        throw new Error(\"fnr: FNR_STRICT=1 and Rust port {} ({addon_name}) could not be loaded: \" + _fnrMsg);",
            port.id
        ),
        "      }".to_string(),
        "    }".to_string(),
        "  }".to_string(),
        "}".to_string(),
    ]);
    lines.join("\n")
}

pub fn enable(ctx: &Ctx, port: &Port) -> Result<()> {
    let file = ctx.root.join(&port.file);
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    let from = file.parent().context("module has no folder")?;
    let mut rel = forward_slashes(&relative(
        from,
        &ctx.lib_dir().join(format!("{}.node", port.ext)),
    ));
    if !rel.starts_with("../") {
        rel = format!("./{rel}");
    }
    // Deno treats every file as an ES module except .cjs/.cts
    let deno = ctx
        .state
        .runtime
        .as_deref()
        .is_some_and(|exe| runtime_of(exe) == Runtime::Deno);
    let cjs = matches!(
        file.extension().and_then(|e| e.to_str()),
        Some("cjs" | "cts")
    );
    let block = shim(port, &rel, is_esm(&ctx.root, &file) || (deno && !cjs));
    fs::write(&file, with_shim(&text, port, &block))
        .with_context(|| format!("writing {}", port.file))
}

pub fn disable(ctx: &Ctx, port: &Port) -> Result<()> {
    let file = ctx.root.join(&port.file);
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    fs::write(&file, without_shim(&text, port)).with_context(|| format!("writing {}", port.file))
}

/// Byte offset of the keyword in the top-level `[export] <keyword> <name>` declaration, if any.
/// A module can declare a name only once at the top level, so there's at most one.
fn declaration(text: &str, keyword: &str, name: &str) -> Option<usize> {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let mut rest = line;
        if let Some(r) = rest
            .strip_prefix("export")
            .filter(|r| r.starts_with([' ', '\t']))
        {
            rest = r.trim_start();
        }
        let at = offset + line.len() - rest.len();
        if let Some(r) = rest
            .strip_prefix(keyword)
            .filter(|r| r.starts_with([' ', '\t']))
            && r.trim_start()
                .strip_prefix(name)
                .is_some_and(|r| !r.starts_with(ident))
        {
            return Some(at);
        }
        offset += line.len();
    }
    None
}

/// The text with the declaration of name switched from one keyword to another (const <-> let).
fn redeclare(text: &str, from: &str, to: &str, name: &str) -> Option<String> {
    let at = declaration(text, from, name)?;
    Some(format!("{}{to}{}", &text[..at], &text[at + from.len()..]))
}

/// A const can't be reassigned, so a function declared with const is declared with let while
/// fnr swaps it; this note in the block tells disable to put the const back.
fn let_note(name: &str) -> String {
    format!("// fnr declared {name} with let instead of const above; disable puts the const back.")
}

fn expr_note(name: &str) -> String {
    format!(
        "// fnr declared {name} with let function expression above; disable puts function back."
    )
}

/// Switch `[export] function <name>` to `[export] let <name> = function <name>` when `<name>`
/// is not referenced before its declaration, so strict TypeScript accepts reassigning it.
fn redeclare_fn_as_let(text: &str, name: &str) -> Option<String> {
    let at = declaration(text, "function", name)?;
    if contains_ident(&text[..at], name) {
        return None;
    }
    Some(format!(
        "{}let {name} = function{}",
        &text[..at],
        &text[at + "function".len()..]
    ))
}

fn restore_fn_from_let(text: &str, name: &str) -> Option<String> {
    let at = declaration(text, "let", name)?;
    let prefix = format!("let {name} = function");
    if text[at..].starts_with(&prefix) {
        Some(format!(
            "{}function{}",
            &text[..at],
            &text[at + prefix.len()..]
        ))
    } else {
        None
    }
}

fn annotate_ts_ignore(block: &str, target_expr: &str) -> String {
    let needle = format!("{target_expr} =");
    block
        .lines()
        .map(|line| {
            if line.trim_start().starts_with(&needle) {
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                format!("{indent}// @ts-ignore — reassign hoisted function declaration\n{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn with_shim(text: &str, port: &Port, block: &str) -> String {
    let note = let_note(&port.func);
    let fnote = expr_note(&port.func);
    let legacy_note = format!(
        "// fast-anything declared {} with let instead of const above; disable puts the const back.",
        port.func
    );
    let mut notes: Vec<String> = Vec::new();
    let (text, block_owned) = if port.owner.is_none() {
        if let Some(t) = redeclare(text, "const", "let", &port.func) {
            notes.push(note);
            (t, block.to_string())
        } else if text.contains(&note) || text.contains(&legacy_note) {
            notes.push(note);
            (text.to_string(), block.to_string())
        } else if let Some(t) = redeclare_fn_as_let(text, &port.func) {
            notes.push(fnote);
            (t, block.to_string())
        } else if text.contains(&fnote) {
            notes.push(fnote);
            (text.to_string(), block.to_string())
        } else if is_ts_file(&port.file) && declaration(text, "function", &port.func).is_some() {
            (text.to_string(), annotate_ts_ignore(block, &port.func))
        } else {
            (text.to_string(), block.to_string())
        }
    } else {
        (text.to_string(), block.to_string())
    };
    let block = if notes.is_empty() {
        block_owned
    } else {
        format!("{block_owned}\n{}", notes.join("\n"))
    };
    add_marked(&text, &port.id, &block, port.module == "__main__")
}

fn without_shim(text: &str, port: &Port) -> String {
    let swapped = text.contains(&let_note(&port.func));
    let swapped_fn = text.contains(&expr_note(&port.func));
    let text = backend::strip_marked(text, "//", &port.id);
    if swapped {
        redeclare(&text, "let", "const", &port.func).unwrap_or(text)
    } else if swapped_fn {
        restore_fn_from_let(&text, &port.func).unwrap_or(text)
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bun_and_deno_get_their_own_command_lines() {
        let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(Runtime::of("/usr/local/bin/bun"), Some(Runtime::Bun));
        assert_eq!(Runtime::of("C:\\deno\\deno.exe"), Some(Runtime::Deno));
        assert!(is_node("bun") && is_node("deno") && !is_node("python"));
        assert_eq!(
            Runtime::Node.with(&["--require", "r.cjs"], &args("app.js x")),
            args("--require r.cjs app.js x")
        );
        assert_eq!(
            Runtime::Bun.with(&["--preload", "r.cjs"], &args("run app.ts")),
            args("--preload r.cjs app.ts")
        );
        assert_eq!(
            Runtime::Deno.with(&["--cpu-prof"], &args("run -A app.ts x")),
            args("run --cpu-prof -A app.ts x")
        );
        assert_eq!(
            Runtime::Deno.with(&["--cpu-prof"], &args("app.ts")),
            args("run --cpu-prof app.ts")
        );
        assert_eq!(
            Runtime::Deno.script(Path::new("runner.mjs")),
            args("run -A runner.mjs")
        );
        assert_eq!(
            entry(&args("run -A --allow-ffi app.ts data.txt")).map(String::as_str),
            Some("app.ts")
        );
        assert_eq!(
            entry(&args("run app.ts")).map(String::as_str),
            Some("app.ts")
        );
    }

    #[test]
    fn worker_threads_are_recorded() {
        if !Command::new("node")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            eprintln!("no node on PATH, skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("fa-node-workers-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("recorder.cjs"), RECORDER).unwrap();
        fs::write(
            dir.join("lib.cjs"),
            "function sq(x) {\n  return x * x;\n}\nsq = globalThis.__fastAnythingWrap ? globalThis.__fastAnythingWrap(sq) : sq;\nmodule.exports = { sq };\n",
        )
        .unwrap();
        fs::write(
            dir.join("app.cjs"),
            "const { Worker, isMainThread, parentPort } = require(\"node:worker_threads\");\n\
             const { sq } = require(\"./lib.cjs\");\n\
             if (isMainThread) {\n\
             \x20 sq(1);\n\
             \x20 for (let i = 0; i < 2; i++) new Worker(__filename).on(\"message\", (m) => console.log(m));\n\
             } else {\n\
             \x20 let t = 0;\n\
             \x20 for (let i = 0; i < 5; i++) t += sq(i);\n\
             \x20 parentPort.postMessage(t);\n\
             }\n",
        )
        .unwrap();
        let out = Command::new("node")
            .args(["--require", "./recorder.cjs", "app.cjs"])
            .current_dir(&dir)
            .env("FAST_ANYTHING_RECORD_OUT", dir.join("rec.jsonl"))
            .env("FAST_ANYTHING_RECORD_META", dir.join("rec.meta.json"))
            .env("FAST_ANYTHING_RECORD_LIMIT", "500")
            .output()
            .unwrap();
        let meta = fs::read_to_string(dir.join("rec.meta.json")).unwrap_or_default();
        let lines = fs::read_to_string(dir.join("rec.jsonl"))
            .unwrap_or_default()
            .lines()
            .count();
        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".t")
            })
            .count();
        let _ = fs::remove_dir_all(&dir);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let meta: Value = serde_json::from_str(&meta).unwrap();
        assert_eq!(
            (meta["calls"].clone(), lines),
            (json!(11), 11),
            "1 call in the main thread, 5 in each worker"
        );
        assert_eq!(leftovers, 0, "worker files are absorbed and removed");
    }

    #[test]
    fn maps_typescript() {
        assert_eq!(rust_type("number").as_deref(), Some("f64"));
        assert_eq!(rust_type("string[]").as_deref(), Some("Vec<String>"));
        assert_eq!(rust_type("Array<number>").as_deref(), Some("Vec<f64>"));
        assert_eq!(rust_type("readonly number[]").as_deref(), Some("Vec<f64>"));
        assert_eq!(
            rust_type("number | undefined").as_deref(),
            Some("Option<f64>")
        );
        assert_eq!(
            rust_type("string | null").as_deref(),
            Some("Option<String>")
        );
        assert_eq!(rust_type("Float64Array").as_deref(), Some("Float64Array"));
        assert_eq!(
            rust_type("Uint8ClampedArray").as_deref(),
            Some("Uint8ClampedArray")
        );
        assert_eq!(rust_type("BigInt64Array"), None);
        assert_eq!(rust_type("Buffer").as_deref(), Some("Buffer"));
        assert_eq!(
            rust_type("Set<string>").as_deref(),
            Some("std::collections::HashSet<String>")
        );
        assert_eq!(rust_type("Set<number>"), None);
        assert_eq!(rust_type("Map<string, number>"), None);
        assert_eq!(
            rust_type("Record<string, number>").as_deref(),
            Some("std::collections::HashMap<String, f64>")
        );
        assert_eq!(rust_type("string | number"), None);
        assert_eq!(
            rust_type("[number, number][]").as_deref(),
            Some("Vec<Vec<f64>>")
        );
        assert_eq!(rust_type("[number, string]"), None);
        assert_eq!(rust_type("Point"), None);
    }

    #[test]
    fn finds_the_script() {
        let args: Vec<String> = ["--require", "x.cjs", "--no-warnings", "app.mjs", "data.txt"]
            .map(String::from)
            .to_vec();
        assert_eq!(entry(&args).map(String::as_str), Some("app.mjs"));
        assert!(
            is_node("node")
                && is_node("/usr/bin/node")
                && is_node("C:\\nodejs\\node.exe")
                && !is_node("python")
        );
        assert_eq!(snake("editDistance"), "edit_distance");
        assert_eq!(snake("type"), "r#type");
    }

    #[test]
    fn entry_script_blocks_go_before_the_main_block() {
        let app =
            "function f(x) {\n  return x;\n}\n\nif (import.meta.main) {\n  console.log(f(1));\n}\n";
        let on = add_marked(app, "app.mjs:f", "f = g;", true);
        let (block, main) = (
            on.find("f = g;").unwrap(),
            on.find("if (import.meta.main)").unwrap(),
        );
        assert!(block < main, "{on}");
        assert_eq!(backend::strip_marked(&on, "//", "app.mjs:f"), app);
        let cjs = "function f(x) {\n  return x;\n}\nif (require.main === module) main();\n";
        let on = add_marked(cjs, "app.js:f", "f = g;", true);
        assert!(
            on.find("f = g;").unwrap() < on.find("if (require.main").unwrap(),
            "{on}"
        );
        assert!(
            add_marked(app, "m.f", "f = g;", false)
                .trim_end()
                .ends_with("// <<< fnr: m.f <<<")
        );
    }

    #[test]
    fn const_functions_switch_to_let_and_back() {
        let port = Port {
            id: "lib.pairs".into(),
            file: "lib.ts".into(),
            module: "lib".into(),
            func: "pairs".into(),
            ext: "fa_lib_pairs".into(),
            crate_dir: ".fast/ports/lib.pairs".into(),
            recorded: 1,
            calls_seen: 1,
            verify: None,
            enabled: false,
            lang: backend::Lang::Node,
            owner: None,
            instance_method: false,
            self_fields: vec![],
            line: 1,
            sources: vec![],
            build_profile: None,
        };
        for original in [
            "export const pairs = (names: string[]): number => {\n  return 0;\n};\n",
            "const pairs = function (names) {\n  return 0;\n};\nmodule.exports = { pairs };\n",
            "const pairsCount = 1;\nexport  const\tpairs = (n) => n;\r\n",
        ] {
            let on = with_shim(original, &port, "pairs = rust.pairs;");
            assert!(
                declaration(&on, "let", "pairs").is_some()
                    && declaration(&on, "const", "pairs").is_none(),
                "{on}"
            );
            assert_eq!(
                on.contains("const pairsCount"),
                original.contains("const pairsCount"),
                "{on}"
            );
            assert_eq!(
                with_shim(&on, &port, "pairs = rust.pairs;"),
                on,
                "enabling twice changes nothing"
            );
            assert_eq!(without_shim(&on, &port), original);
        }
        // a function that was declared with let stays let
        let original = "let pairs = (n) => n;\n";
        let on = with_shim(original, &port, "pairs = rust.pairs;");
        assert!(!on.contains("puts the const back"));
        assert_eq!(without_shim(&on, &port), original);
    }

    #[test]
    fn static_methods_go_through_their_class() {
        let port = Port {
            id: "lib.Text.pairs".into(),
            file: "lib.mjs".into(),
            module: "lib".into(),
            func: "pairs".into(),
            ext: "fa_lib_text_pairs".into(),
            crate_dir: ".fast/ports/lib.Text.pairs".into(),
            recorded: 1,
            calls_seen: 1,
            verify: None,
            enabled: false,
            lang: backend::Lang::Node,
            owner: Some("Text".into()),
            instance_method: false,
            self_fields: vec![],
            line: 2,
            sources: vec![],
            build_profile: None,
        };
        let block = shim(&port, "./x.node", true);
        assert!(
            block.contains(
                "import { createRequire as _fnr_req_fa_lib_text_pairs } from \"node:module\";"
            ),
            "{block}"
        );
        assert!(
            block.contains("Text.pairs = _fnr_req_fa_lib_text_pairs(import.meta.url)"),
            "{block}"
        );
        assert!(block.contains("(\"./x.node\").pairs;"), "{block}");
        // a same-named const elsewhere is left alone
        let original =
            "const pairs = 1;\nexport class Text {\n  static pairs(n) {\n    return n;\n  }\n}\n";
        let on = with_shim(original, &port, &block);
        assert!(on.starts_with("const pairs = 1;"), "{on}");
        assert_eq!(without_shim(&on, &port), original);
    }

    #[test]
    fn shim_loads_the_addon() {
        let port = Port {
            id: "fuzzy.levenshtein".into(),
            file: "fuzzy.mjs".into(),
            module: "fuzzy".into(),
            func: "levenshtein".into(),
            ext: "fa_fuzzy_levenshtein".into(),
            crate_dir: ".fast/ports/fuzzy.levenshtein".into(),
            recorded: 1,
            calls_seen: 1,
            verify: None,
            enabled: false,
            lang: backend::Lang::Node,
            owner: None,
            instance_method: false,
            self_fields: vec![],
            line: 1,
            sources: vec![],
            build_profile: None,
        };
        let esm = shim(&port, "./.fast/lib/fa_fuzzy_levenshtein.node", true);
        assert!(!esm.contains("getBuiltinModule"), "{esm}");
        assert!(esm.contains(
            "import { createRequire as _fnr_req_fa_fuzzy_levenshtein } from \"node:module\";"
        ));
        assert!(esm.contains(
            "_fnr_req_fa_fuzzy_levenshtein(import.meta.url)(\"./.fast/lib/fa_fuzzy_levenshtein.node\").levenshtein"
        ));
        assert!(
            shim(&port, "./x.node", false)
                .contains("levenshtein = require(\"./x.node\").levenshtein;")
        );
        let original = "export function levenshtein(a, b) {\n  return 0;\n}\n";
        let on = backend::append_marked(original, "//", &port.id, &esm);
        assert_eq!(backend::strip_marked(&on, "//", &port.id), original);
        assert!(esm.contains("_fnr_res_fa_fuzzy_levenshtein(process.env.FNR_LIB_DIR,"));
        let cjs = shim(&port, "./x.node", false);
        assert!(cjs.contains("require(\"node:path\").resolve(process.env.FNR_LIB_DIR,"));
        assert!(cjs.contains("_fnrDis === \"0\" || _fnrDis === \"false\""));
        assert!(!cjs.contains("(v) =>"));
        assert!(cjs.contains("_err instanceof Error ? _err.message : String(_err)"));
    }

    #[test]
    fn strict_ts_function_and_interface_codegen() {
        let mut records = std::collections::BTreeMap::new();
        records.insert(
            "User".to_string(),
            vec![
                crate::hotspot::Param {
                    name: "name".into(),
                    annotation: Some("string".into()),
                    default: None,
                    kwonly: false,
                },
                crate::hotspot::Param {
                    name: "age".into(),
                    annotation: Some("number | undefined".into()),
                    default: None,
                    kwonly: false,
                },
            ],
        );
        let hs = Hotspot {
            id: "fuzzy.rankUsers".into(),
            file: "fuzzy.ts".into(),
            line: 2,
            name: "rankUsers".into(),
            params: vec![
                crate::hotspot::Param {
                    name: "users".into(),
                    annotation: Some("User[]".into()),
                    default: None,
                    kwonly: false,
                },
                crate::hotspot::Param {
                    name: "minLen".into(),
                    annotation: Some("number | undefined".into()),
                    default: Some("3".into()),
                    kwonly: false,
                },
            ],
            returns: Some("number".into()),
            records,
            ..Default::default()
        };
        let port = Port {
            id: "fuzzy.rankUsers".into(),
            file: "fuzzy.ts".into(),
            module: "fuzzy".into(),
            func: "rankUsers".into(),
            ext: "fa_fuzzy_rankusers".into(),
            crate_dir: ".fast/ports/fuzzy.rankUsers".into(),
            lang: backend::Lang::Node,
            ..Default::default()
        };
        let rs = lib_rs(&hs, &port);
        assert!(rs.contains("#[napi(object)]"), "{rs}");
        assert!(rs.contains("pub struct User"), "{rs}");
        assert!(rs.contains("pub age: Option<f64>"), "{rs}");
        assert!(
            rs.contains("pub fn rank_users(users: Vec<User>, min_len: Option<f64>) -> Result<f64>"),
            "{rs}"
        );

        let ts_src = "export interface User { name: string; age?: number; }\nexport function rankUsers(users: User[], minLen = 3): number {\n  return users.length + minLen;\n}\n";
        let block = shim(&port, "./.fast/lib/fa_fuzzy_rankusers.node", true);
        let on = with_shim(ts_src, &port, &block);
        assert!(
            on.contains("export let rankUsers = function rankUsers"),
            "{on}"
        );
        assert!(!on.contains("(v) =>"), "{on}");
        assert!(on.contains("_err instanceof Error"), "{on}");
        assert_eq!(with_shim(&on, &port, &block), on);
        assert_eq!(without_shim(&on, &port), ts_src);
    }

    #[test]
    fn multiple_esm_shims_use_unique_import_aliases() {
        let p1 = Port {
            id: "fuzzy.one".into(),
            file: "fuzzy.mjs".into(),
            module: "fuzzy".into(),
            func: "one".into(),
            ext: "fa_fuzzy_one".into(),
            crate_dir: ".fast/ports/fuzzy.one".into(),
            recorded: 1,
            calls_seen: 1,
            verify: None,
            enabled: false,
            lang: backend::Lang::Node,
            owner: None,
            instance_method: false,
            self_fields: vec![],
            line: 1,
            sources: vec![],
            build_profile: None,
        };
        let p2 = Port {
            id: "fuzzy.two".into(),
            func: "two".into(),
            ext: "fa_fuzzy_two".into(),
            crate_dir: ".fast/ports/fuzzy.two".into(),
            ..p1.clone()
        };
        let combined = format!(
            "{}\n{}",
            shim(&p1, "./a.node", true),
            shim(&p2, "./b.node", true)
        );
        assert!(combined.contains("_fnr_req_fa_fuzzy_one"));
        assert!(combined.contains("_fnr_req_fa_fuzzy_two"));
        assert!(combined.contains("_fnr_res_fa_fuzzy_one"));
        assert!(combined.contains("_fnr_res_fa_fuzzy_two"));
    }

    #[test]
    fn unwraps_package_manager_and_ts_runners() {
        let dir = std::env::temp_dir().join(format!("fnr-node-unwrap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("package.json"),
            r#"{
              "scripts": {
                "start": "node server.js --port 3000",
                "bench": "tsx bench.ts",
                "dev": "vite --port 5173",
                "complex": "node a.js && node b.js",
                "envrun": "NODE_ENV=production node app.js"
              }
            }"#,
        )
        .unwrap();

        let cmd = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(
            unwrap_node_runner(&cmd("npm run start"), &dir)
                .unwrap()
                .unwrap(),
            cmd("node server.js --port 3000")
        );
        assert_eq!(
            unwrap_node_runner(&cmd("pnpm bench -- --fast"), &dir)
                .unwrap()
                .unwrap()
                .first()
                .map(String::as_str),
            Some("node")
        );
        assert!(
            unwrap_node_runner(&cmd("yarn dev"), &dir)
                .unwrap_err()
                .to_string()
                .contains("dev bundler")
        );
        assert!(
            unwrap_node_runner(&cmd("npm run complex"), &dir)
                .unwrap_err()
                .to_string()
                .contains("compound shell command")
        );
        assert!(
            unwrap_node_runner(&cmd("npm run envrun"), &dir)
                .unwrap_err()
                .to_string()
                .contains("NODE_ENV=production")
        );
        assert_eq!(
            unwrap_node_runner(&cmd("npx -y ts-node app.ts"), &dir)
                .unwrap()
                .unwrap()
                .last()
                .map(String::as_str),
            Some("app.ts")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn timed_profile_stops_long_running_node_script() {
        if !Command::new("node")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            eprintln!("no node on PATH, skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("fnr-node-dur-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("server.mjs"),
            r#"export function spin(n) {
  let s = 0;
  for (let i = 0; i < n; i++) s = (s + i * 3) | 0;
  return s;
}
const timer = setInterval(() => { spin(5000000); }, 5);
setTimeout(() => { clearInterval(timer); }, 30000);
"#,
        )
        .unwrap();
        let ctx = Ctx::open(&dir).unwrap();
        let start = std::time::Instant::now();
        let prof = profile_with_options(
            &ctx,
            "node",
            &["server.mjs".to_string()],
            crate::backend::ProfileOptions {
                duration_secs: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            start.elapsed().as_secs_f64() < 10.0,
            "timed profile should stop early, elapsed={:?}",
            start.elapsed()
        );
        assert!(
            prof.functions.iter().any(|f| f.name == "spin"),
            "expected `spin` in timed profile, got: {:?}",
            prof.functions.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        #[cfg(unix)]
        {
            let link =
                std::env::temp_dir().join(format!("fnr-node-dur-link-{}", std::process::id()));
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(&dir, &link).unwrap();
            let link_ctx = Ctx::open(&link).unwrap();
            let link_prof = profile_with_options(
                &link_ctx,
                "node",
                &["server.mjs".to_string()],
                crate::backend::ProfileOptions {
                    duration_secs: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(
                link_prof.functions.iter().any(|f| f.name == "spin"),
                "expected `spin` through symlinked root, got: {:?}",
                link_prof
                    .functions
                    .iter()
                    .map(|f| &f.name)
                    .collect::<Vec<_>>()
            );
            let _ = fs::remove_file(&link);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
