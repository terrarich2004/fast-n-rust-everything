//! The steps (scan, port, verify, enable, bench, report) shared by the CLI and the MCP server.

use crate::backend::{self, Lang};
use crate::bench::{self, BenchResult};
use crate::hotspot::{self, Hotspot};
use crate::report::{self, ReportOut};
use crate::state::{Ctx, Port, Verify};
use crate::{agent, show, ui, util};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
pub struct ScanOut {
    pub command: Vec<String>,
    pub wall: f64,
    pub hotspots: Vec<Hotspot>,
}

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    pub duration_secs: Option<u64>,
    pub load_command: Option<String>,
}

pub fn scan(root: &Path, command: Vec<String>) -> Result<ScanOut> {
    scan_with_options(root, command, ScanOptions::default())
}

pub fn scan_with_options(root: &Path, command: Vec<String>, opts: ScanOptions) -> Result<ScanOut> {
    ensure!(
        !command.is_empty(),
        "put the command that runs your app after --, e.g.  fnr scan -- python app.py"
    );
    let mut ctx = Ctx::open(root)?;
    let command = match crate::python::unwrap_env_runner(&command, |argv| {
        env_runner_output(&ctx.root, argv)
    })? {
        Some(unwrapped) => {
            ui::step(format!(
                "{} runs {}",
                ui::bold(command[..2].join(" ")),
                ui::bold(&unwrapped[0])
            ));
            unwrapped
        }
        None => match crate::python::unwrap_python_tool(&command, &ctx.root) {
            Some(unwrapped) => {
                ui::step(format!(
                    "{} runs {}",
                    ui::bold(&command[0]),
                    ui::bold(unwrapped.join(" "))
                ));
                unwrapped
            }
            None => match crate::node::unwrap_node_runner(&command, &ctx.root)? {
                Some(unwrapped) => {
                    ui::step(format!(
                        "{} runs {}",
                        ui::bold(&command[0]),
                        ui::bold(unwrapped.join(" "))
                    ));
                    unwrapped
                }
                None => command,
            },
        },
    };
    let lang = crate::cfamily::refine(Lang::detect(&command[0])?, &command[1..]);
    let lang = crate::kotlin::refine(lang, &command[0], &ctx.root);
    let exe = lang.resolve(&command[0], &ctx.root)?;
    let effective_duration = match (opts.duration_secs, opts.load_command.as_ref()) {
        (Some(d), _) => Some(d),
        (None, Some(c)) if !c.trim().is_empty() => Some(4),
        _ => None,
    };
    ui::step(format!("profiling {}", ui::bold(command.join(" "))));
    let profile = lang.profile_with_options(
        &ctx,
        &exe,
        &command[1..],
        crate::backend::ProfileOptions {
            duration_secs: effective_duration,
            load_command: opts.load_command.clone(),
        },
    )?;
    let hotspots = hotspot::rank(profile.functions, profile.wall);
    ctx.state.command = command.clone();
    ctx.state.lang = lang;
    ctx.state.runtime = Some(exe);
    ctx.state.base = profile.base;
    ctx.state.wall = profile.wall;
    ctx.state.duration_secs = effective_duration;
    ctx.state.load_command = opts.load_command.filter(|c| !c.trim().is_empty());
    ctx.state.hotspots = hotspots.clone();
    ctx.save()?;
    Ok(ScanOut {
        command,
        wall: profile.wall,
        hotspots,
    })
}

/// What an env runner (uv, poetry, ...) prints for a command run in the project.
fn env_runner_output(root: &Path, argv: &[&str]) -> Result<String> {
    let out = util::output(
        std::process::Command::new(argv[0])
            .args(&argv[1..])
            .current_dir(root),
        argv[0],
    )?;
    ensure!(
        out.status.success(),
        "{} failed:\n{}",
        argv.join(" "),
        util::tail(&util::combined(&out), 10)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn pick<'a>(hotspots: &'a [Hotspot], id: Option<&str>) -> Result<&'a Hotspot> {
    let Some(id) = id else {
        return hotspots
            .iter()
            .find(|h| h.worth_it())
            .context("no function worth porting in the last scan (see  fnr status  for why)");
    };
    if let Some(h) = hotspots.iter().find(|h| h.id == id) {
        return Ok(h);
    }
    let hits: Vec<&Hotspot> = hotspots
        .iter()
        .filter(|h| h.name == id || h.qualname == id)
        .collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => bail!("{id} wasn't in the last scan; run  fnr scan -- ...  again, or check the name"),
        many => bail!(
            "{id} is ambiguous: {}",
            many.iter()
                .map(|h| h.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[derive(Serialize)]
pub struct PortOut {
    pub id: String,
    pub crate_dir: PathBuf,
    pub lib_rs: PathBuf,
    pub task_md: PathBuf,
    pub recorded: usize,
    pub calls_seen: u64,
    pub skipped: usize,
    pub task: String,
}

pub fn port(root: &Path, id: Option<&str>, samples: usize) -> Result<PortOut> {
    let mut ctx = Ctx::open(root)?;
    let exe = ctx.runtime()?.to_string();
    let lang = ctx.state.lang;
    let hs = pick(&ctx.state.hotspots, id)?.clone();
    if !hs.portable() {
        bail!("{} can't be ported yet: {}", hs.id, hs.blockers.join("; "));
    }
    let module = hs.module.clone().unwrap_or_default();
    let crate_rel = format!(".fast/ports/{}", hs.id.replace([':', '/', '\\'], "."));
    let dir = ctx.root.join(&crate_rel);
    fs::create_dir_all(&dir)?;

    ui::step(format!(
        "running the app again to record real calls to {}",
        ui::bold(&hs.id)
    ));
    let app_args = ctx.state.command[1..].to_vec();
    let rec = lang.record(
        &ctx,
        &exe,
        &app_args,
        &hs,
        &dir.join("recordings.jsonl"),
        samples.max(1),
    )?;
    if rec.mutates {
        bail!(
            "{} changes its arguments in place (seen while recording). A Rust port can only return a value, so it would drop those changes. Have the function return the new value instead, then scan again.",
            hs.id
        );
    }
    if rec.kept == 0 {
        let types = if rec.unsupported.is_empty() {
            String::new()
        } else {
            format!(" (got: {})", rec.unsupported.join(", "))
        };
        bail!(
            "recorded no usable calls to {} out of {}. Arguments and results must be plain data: numbers, strings, bytes, lists, tuples, dicts, sets{types}",
            hs.id,
            rec.calls
        );
    }
    let enabled = ctx.state.ports.get(&hs.id).is_some_and(|p| p.enabled);
    let helpers: Vec<Hotspot> = hs
        .calls
        .iter()
        .filter_map(|name| {
            hotspot::helpers_named(&ctx.state.hotspots, &hs, name)
                .first()
                .copied()
        })
        .cloned()
        .collect();
    let text = fs::read_to_string(ctx.root.join(&hs.file)).unwrap_or_default();
    let sources: Vec<&str> = std::iter::once(&hs)
        .chain(&helpers)
        .map(|h| h.source.as_str())
        .collect();
    let port = Port {
        id: hs.id.clone(),
        file: hs.file.clone(),
        module,
        func: hs.name.clone(),
        ext: format!("fa_{}", util::slug(&hs.id)),
        crate_dir: crate_rel,
        recorded: rec.kept,
        calls_seen: rec.calls,
        verify: None,
        enabled,
        lang,
        owner: hs.owner.clone(),
        instance_method: hs.instance_method,
        self_fields: hs.self_fields.clone(),
        line: hs.line,
        sources: crate::state::verbatim(&text, &sources),
        build_profile: None,
    };
    let task = lang.scaffold(&ctx, &hs, &helpers, &port)?;
    ctx.state.ports.insert(port.id.clone(), port);
    ctx.save()?;
    Ok(PortOut {
        id: hs.id,
        lib_rs: dir
            .join("src")
            .join(if lang.c_bridge() { "port.rs" } else { "lib.rs" }),
        task_md: dir.join("TASK.md"),
        crate_dir: dir,
        recorded: rec.kept,
        calls_seen: rec.calls,
        skipped: rec.skipped,
        task,
    })
}

#[derive(Debug, Clone, Default)]
pub struct VerifyOptions {
    pub float_tolerance: Option<f64>,
    pub known_diffs: Option<PathBuf>,
    pub min_speedup: Option<f64>,
}

pub fn verify(root: &Path, id: Option<&str>) -> Result<(String, Verify)> {
    verify_with(root, id, &VerifyOptions::default())
}

pub fn verify_with(
    root: &Path,
    id: Option<&str>,
    opts: &VerifyOptions,
) -> Result<(String, Verify)> {
    let mut ctx = Ctx::open(root)?;
    let id = ctx.resolve_port(id)?;
    let port = ctx.state.ports[&id].clone();
    let source_hash = backend::source_hash(&ctx.crate_dir(&port))?;
    if ctx.stale(&port) {
        ui::warn(format!(
            "{} changed since {id} was recorded: {}",
            port.file,
            stale_hint(&id)
        ));
    }
    ui::step(format!("building {} (release)", ui::bold(&port.ext)));
    let v = match backend::build(&ctx, &port)? {
        Err(log) => Verify {
            built: false,
            build_log: Some(log),
            ..Verify::default()
        },
        Ok(_) => {
            ui::step(format!(
                "replaying {} recorded call{} against the Rust port",
                port.recorded,
                if port.recorded == 1 { "" } else { "s" }
            ));
            port.lang.verify_with(
                &ctx,
                &port,
                opts.float_tolerance,
                opts.known_diffs.as_deref(),
            )?
        }
    };
    let mut v = v;
    v.lang = port.lang;
    v.source_hash = Some(source_hash);
    if !v.ok() && port.enabled {
        port.lang.disable(&ctx, &port)?;
        if let Some(p) = ctx.state.ports.get_mut(&id) {
            p.enabled = false;
        }
        ui::warn(format!(
            "{id} was enabled; switched it back to {} until it passes again",
            port.lang.label()
        ));
    }
    if let Some(p) = ctx.state.ports.get_mut(&id) {
        p.verify = Some(v.clone());
    }
    ctx.save()?;
    Ok((id, v))
}

/// Verify every port; an enabled one that fails is switched back, as with a single verify.
pub fn verify_all(root: &Path) -> Result<()> {
    verify_all_with(root, &VerifyOptions::default())
}

pub fn verify_all_with(root: &Path, opts: &VerifyOptions) -> Result<()> {
    let ids: Vec<String> = Ctx::open(root)?.state.ports.keys().cloned().collect();
    ensure!(!ids.is_empty(), "nothing ported yet: run  fnr port");
    let mut failed = vec![];
    for id in &ids {
        let (id, v) = verify_with(root, Some(id), opts)?;
        show::verify(&id, &v);
        if !v.ok() || v.below_min_speedup(opts.min_speedup) {
            failed.push(id);
        }
    }
    ensure!(
        failed.is_empty(),
        "{} of {} ports failed verify/speedup: {}",
        failed.len(),
        ids.len(),
        failed.join(", ")
    );
    ui::ok(format!("all {} ports match the original", ids.len()));
    Ok(())
}

/// What to do about a port whose original changed after it was recorded.
pub fn stale_hint(id: &str) -> String {
    format!(
        "its recorded calls and Rust may be out of date. Run  fnr port {id}  to record again, update the Rust, then verify (or --force)"
    )
}

/// What to do about a port that's right but slower than the original.
pub fn slower_hint() -> &'static str {
    "calling into Rust costs more than this function's work, so it won't be switched on. Port its caller instead (fewer, bigger calls), or  enable --force"
}

/// Whether the crate is not exactly the one covered by the last verify. Old state without a hash
/// keeps its legacy behavior; re-porting clears verify and therefore always needs a new one.
fn crate_needs_verify(ctx: &Ctx, port: &Port) -> Result<bool> {
    let Some(verify) = &port.verify else {
        return Ok(true);
    };
    let Some(expected) = verify.source_hash.as_deref() else {
        return Ok(false);
    };
    Ok(backend::source_hash(&ctx.crate_dir(port))? != expected)
}

pub fn enable(root: &Path, id: Option<&str>, force: bool) -> Result<String> {
    enable_with_options(root, id, force, false, false)
}

/// Enable a verified port after rebuilding with the `Ship` LTO profile (and optional PGO).
pub fn enable_with(root: &Path, id: Option<&str>, force: bool, pgo: bool) -> Result<String> {
    enable_with_options(root, id, force, pgo, false)
}

/// Enable a verified port with explicit `pgo` and `native_cpu` (`-C target-cpu=native`) build flags.
pub fn enable_with_options(
    root: &Path,
    id: Option<&str>,
    force: bool,
    pgo: bool,
    native_cpu: bool,
) -> Result<String> {
    let mut ctx = Ctx::open(root)?;
    let id = ctx.resolve_port(id)?;
    let port = ctx.state.ports[&id].clone();
    let was_enabled = port.enabled;
    if ctx.stale(&port) && !force {
        bail!(
            "{} changed since {id} was recorded: {}",
            port.file,
            stale_hint(&id)
        );
    }
    let verified = port.verify.as_ref().is_some_and(Verify::ok);
    if !verified && !force {
        bail!("{id} hasn't passed verify yet: run  fnr verify {id}  (or --force)");
    }
    if !force && crate_needs_verify(&ctx, &port)? {
        bail!("{id}'s Rust changed since it passed verify: run  fnr verify {id}  (or --force)");
    }
    if let Some(m) = port
        .verify
        .as_ref()
        .filter(|v| v.slower() && !force)
        .and_then(|v| v.micro)
    {
        bail!(
            "{id} is {} slower per call in Rust: {}",
            util::times(1.0 / m.speedup),
            slower_hint()
        );
    }
    let profile_label = match (pgo, native_cpu) {
        (true, true) => "pgo+native-cpu",
        (true, false) => "pgo",
        (false, true) => "ship+native-cpu",
        (false, false) => "ship",
    };
    if !pgo {
        ui::step(format!(
            "building {} ({profile_label})",
            ui::bold(&port.ext)
        ));
        match backend::build_with_options(
            &ctx,
            &port,
            backend::BuildOptions {
                profile: backend::BuildProfile::Ship,
                native_cpu,
            },
        )? {
            Ok(_) => {}
            Err(log) => bail!("ship build failed for {id}:\n{log}"),
        }
    }
    port.lang.enable(&ctx, &port)?;
    if pgo {
        ui::step(format!(
            "building {} ({profile_label})",
            ui::bold(&port.ext)
        ));
        if let Err(e) = backend::build_pgo_with_options(&ctx, &port, native_cpu) {
            if !was_enabled {
                let _ = port.lang.disable(&ctx, &port);
            }
            return Err(e);
        }
    }
    if let Some(p) = ctx.state.ports.get_mut(&id) {
        p.enabled = true;
        p.build_profile = Some(profile_label.to_string());
    }
    ctx.save()?;
    Ok(id)
}

fn markers_for_port(port: &Port) -> Vec<(String, String)> {
    let (comment, ids): (&str, Vec<String>) = match port.lang {
        Lang::Python | Lang::Ruby | Lang::R => ("#", vec![port.id.clone()]),
        Lang::Php | Lang::C | Lang::Cpp => {
            ("//", vec![port.id.clone(), format!("{} bridge", port.id)])
        }
        _ => ("//", vec![port.id.clone()]),
    };
    ids.iter().map(|id| backend::markers(comment, id)).collect()
}

pub fn disable(root: &Path, id: Option<&str>) -> Result<String> {
    let mut ctx = Ctx::open(root)?;
    let id = ctx.resolve_port(id)?;
    let port = &ctx.state.ports[&id];
    let file = ctx.root.join(&port.file);
    let before = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    for (begin, end) in markers_for_port(port) {
        let legacy = backend::legacy_markers(
            if begin.starts_with('#') { "#" } else { "//" },
            begin
                .split(": ")
                .nth(1)
                .and_then(|s| s.strip_suffix(" >>>"))
                .unwrap_or(&id),
        );
        ensure!(
            before.contains(&begin) == before.contains(&end)
                && before.contains(&legacy.0) == before.contains(&legacy.1),
            "the fnr block for {id} is incomplete; restore its begin/end markers before disabling"
        );
    }
    port.lang.disable(&ctx, port)?;
    let after = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    for (begin, end) in markers_for_port(port) {
        ensure!(
            !after.contains(&begin) && !after.contains(&end),
            "the fnr block for {id} wasn't removed"
        );
    }
    if let Some(p) = ctx.state.ports.get_mut(&id) {
        p.enabled = false;
    }
    ctx.save()?;
    Ok(id)
}

/// Switch every enabled port back to the original code; returns their ids.
pub fn disable_all(root: &Path) -> Result<Vec<String>> {
    let ids: Vec<String> = Ctx::open(root)?
        .state
        .ports
        .values()
        .filter(|p| p.enabled)
        .map(|p| p.id.clone())
        .collect();
    for id in &ids {
        disable(root, Some(id))?;
    }
    Ok(ids)
}

pub fn build_all(root: &Path) -> Result<()> {
    let ctx = Ctx::open(root)?;
    ensure!(!ctx.state.ports.is_empty(), "nothing ported yet");
    let mut failed = 0;
    for port in ctx.state.ports.values() {
        if port.enabled && crate_needs_verify(&ctx, port)? {
            failed += 1;
            ui::error(format!(
                "{} is switched on and its Rust hasn't passed verify in its current state; run  fnr verify {}",
                port.id, port.id
            ));
            continue;
        }
        ui::step(format!("building {}", ui::bold(&port.id)));
        match backend::build(&ctx, port)? {
            Ok(path) => ui::ok(format!("{}", path.display())),
            Err(log) => {
                failed += 1;
                ui::error(format!("{} doesn't compile:\n{log}", port.id));
            }
        }
    }
    ensure!(failed == 0, "{failed} port(s) didn't build");
    Ok(())
}

/// Compile every `.fast/ports/*` crate using the `Ship` LTO profile directly into `.fast/lib/`,
/// without requiring `.fast/state.json` or `recordings.jsonl` (for Docker/CI deploy builds).
pub fn build_deploy(root: &Path, native_cpu: bool) -> Result<Vec<PathBuf>> {
    let ctx = Ctx::open(root)?;
    let mut targets: Vec<Port> = Vec::new();
    let mut seen_dirs = std::collections::HashSet::<PathBuf>::new();

    for port in ctx.state.ports.values() {
        let dir = ctx.crate_dir(port);
        if dir.join("Cargo.toml").is_file() {
            let key = fs::canonicalize(&dir).unwrap_or(dir);
            seen_dirs.insert(key);
            targets.push(port.clone());
        }
    }

    let ports_dir = ctx.fast.join("ports");
    if ports_dir.is_dir() {
        let mut entries: Vec<PathBuf> = fs::read_dir(&ports_dir)?
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| p.is_dir() && p.join("Cargo.toml").is_file())
            .collect();
        entries.sort();
        for dir in entries {
            let key = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            if seen_dirs.insert(key) {
                targets.push(backend::infer_port_from_manifest(&ctx.root, &dir)?);
            }
        }
    }

    ensure!(
        !targets.is_empty(),
        "no .fast/ports/*/Cargo.toml crates found to build"
    );

    let mut built = Vec::new();
    let mut failed = 0usize;
    let opts = backend::BuildOptions {
        profile: backend::BuildProfile::Ship,
        native_cpu,
    };
    for port in &targets {
        ui::step(format!("deploy-building {} (ship)", ui::bold(&port.ext)));
        match backend::build_with_options(&ctx, port, opts)? {
            Ok(path) => {
                ui::ok(format!("{}", path.display()));
                built.push(path);
            }
            Err(log) => {
                failed += 1;
                ui::error(format!("{} doesn't compile:\n{log}", port.id));
            }
        }
    }
    ensure!(failed == 0, "{failed} port(s) didn't build");
    Ok(built)
}

pub fn bench(root: &Path, runs: usize) -> Result<BenchResult> {
    let mut ctx = Ctx::open(root)?;
    for p in ctx
        .state
        .ports
        .values()
        .filter(|p| p.enabled && ctx.stale(p))
    {
        ui::warn(format!(
            "{} changed since {} was recorded: {}",
            p.file,
            p.id,
            stale_hint(&p.id)
        ));
    }
    let result = bench::run(&ctx, runs.max(1))?;
    ctx.state.bench = Some(result.clone());
    ctx.save()?;
    Ok(result)
}

pub fn report(root: &Path) -> Result<ReportOut> {
    report::write(&Ctx::open(root)?)
}

/// What the agent gets told after a failed attempt, or a right one that's slower than the original.
pub fn feedback(v: &Verify) -> String {
    feedback_with(v, &VerifyOptions::default())
}

/// What the agent gets told after a failed attempt, or a right one that is slower / below `--min-speedup`.
pub fn feedback_with(v: &Verify, opts: &VerifyOptions) -> String {
    let target_file = if v.lang.c_bridge() {
        "src/port.rs"
    } else {
        "src/lib.rs"
    };
    if v.ok()
        && let Some(m) = v.micro.filter(|_| v.slower())
    {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let ffi_note = match (m.direct_abi, m.ffi_ns) {
            (Some(false), Some(ffi)) if ffi > 0.4 * m.rust_ns => format!(
                "\nFFI/JSON marshaling (~{}/call) dominates runtime. Port the outer caller to reduce crossings or escalate with `fnr escalate --batch`.",
                util::nanos(ffi)
            ),
            (Some(true), _) => "\nDirect binary C ABI is active (zero JSON overhead), so the bottleneck is inside the Rust compute kernel. Consider 4-wide ILP/SIMD (preserving IEEE-754 op order, no FMA), dynamic work-stealing (`AtomicI32`/`rayon`), and avoiding per-call allocations.".to_string(),
            _ => "\nConsider borrowing inputs instead of converting/cloning, pre-allocating with `with_capacity`, ASCII `&[u8]` fast paths, and multi-core parallelism where inputs are large.".to_string(),
        };
        return format!(
            "\n\n## Your last attempt is right but slow\n\n\
             All {} cases match, but a call takes {} in Rust against {} in the original ({} slower) on a {cores}-core host.{ffi_note}\n\nFix {target_file}.\n",
            v.total,
            util::nanos(m.rust_ns),
            util::nanos(m.original_ns),
            util::times(1.0 / m.speedup),
        );
    }
    if v.ok()
        && v.below_min_speedup(opts.min_speedup)
        && let (Some(min), Some(m)) = (opts.min_speedup, v.micro)
    {
        return format!(
            "\n\n## Your last attempt is right but below --min-speedup\n\n\
             It matched all {} calls ({} per call), which is below the required --min-speedup {min:.2}x. Avoid per-call allocations/clones and tighten inner loops in {target_file}.\n",
            v.total,
            util::times(m.speedup),
        );
    }
    let mut out = String::from("\n\n## Your last attempt failed\n\n");
    if let Some(log) = &v.build_log {
        out.push_str("It doesn't compile:\n\n```\n");
        out.push_str(log);
        out.push_str("\n```\n");
    } else {
        out.push_str(&format!(
            "{} of {} recorded calls came out different:\n\n",
            v.failed, v.total
        ));
        for f in &v.failures {
            out.push_str(&format!(
                "- `{}` should give `{}`, the port gave `{}`\n",
                f.call, f.expected, f.got
            ));
        }
    }
    out.push_str(&format!("\nFix {target_file}.\n"));
    out
}

pub fn agent_port(root: &Path, id: &str, agent_spec: &str, attempts: usize) -> Result<Verify> {
    agent_port_with(root, id, agent_spec, attempts, &VerifyOptions::default())
}

pub fn agent_port_with(
    root: &Path,
    id: &str,
    agent_spec: &str,
    attempts: usize,
    opts: &VerifyOptions,
) -> Result<Verify> {
    let attempts = attempts.max(1);
    let mut last: Option<Verify> = None;
    // A port that's right but slower goes back to the agent to speed up; if no later attempt is both
    // right and faster, its sources are put back.
    let mut right: Option<(Verify, Snapshot)> = None;
    for attempt in 1..=attempts {
        let ctx = Ctx::open(root)?;
        let port = ctx
            .state
            .ports
            .get(id)
            .with_context(|| format!("{id} isn't ported"))?
            .clone();
        let dir = ctx.crate_dir(&port);
        let mut prompt = fs::read_to_string(dir.join("TASK.md")).context("reading TASK.md")?;
        if let Some(v) = &last {
            prompt.push_str(&feedback_with(v, opts));
        }
        ui::step(format!(
            "{} is writing the Rust port (attempt {attempt}/{attempts})",
            ui::bold(agent_spec)
        ));
        agent::run(agent_spec, &dir, &prompt, &dir.join("agent.log"))?;
        let (_, v) = verify_with(root, Some(id), opts)?;
        show::verify(id, &v);
        if v.ok() && !v.slower() && !v.below_min_speedup(opts.min_speedup) {
            return Ok(v);
        }
        if v.ok()
            && right
                .as_ref()
                .is_none_or(|(best, _)| speedup(&v) > speedup(best))
        {
            right = Some((v.clone(), snapshot(&dir)?));
        }
        last = Some(v);
    }
    if let Some((best, files)) = right
        && last
            .as_ref()
            .is_none_or(|v| !v.ok() || speedup(v) < speedup(&best))
    {
        ui::step(format!(
            "putting back the right attempt of {id} ({} per call)",
            util::times(speedup(&best))
        ));
        let ctx = Ctx::open(root)?;
        let port = ctx
            .state
            .ports
            .get(id)
            .with_context(|| format!("{id} isn't ported"))?;
        restore_snapshot(&ctx.crate_dir(port), &files)?;
        let (_, v) = verify_with(root, Some(id), opts)?;
        return Ok(v);
    }
    Ok(last.unwrap_or_default())
}

fn speedup(v: &Verify) -> f64 {
    v.micro.map(|m| m.speedup).unwrap_or(0.0)
}

/// Cargo inputs for one attempt; recordings, TASK.md and agent.log are deliberately excluded.
type Snapshot = Vec<(PathBuf, Vec<u8>)>;

fn snapshot_files(dir: &Path, out: &mut Snapshot) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            snapshot_files(&path, out)?;
        } else {
            out.push((path.clone(), fs::read(&path)?));
        }
    }
    Ok(())
}

fn snapshot(dir: &Path) -> Result<Snapshot> {
    let mut out = vec![];
    for name in ["Cargo.toml", "Cargo.lock", "build.rs"] {
        let path = dir.join(name);
        if path.is_file() {
            out.push((path.clone(), fs::read(path)?));
        }
    }
    let src = dir.join("src");
    if src.is_dir() {
        snapshot_files(&src, &mut out)?;
    }
    Ok(out)
}

fn restore_snapshot(dir: &Path, files: &Snapshot) -> Result<()> {
    for name in ["Cargo.lock", "build.rs"] {
        let path = dir.join(name);
        if !files.iter().any(|(saved, _)| saved == &path) {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
            }
        }
    }
    let src = dir.join("src");
    if src.exists() {
        fs::remove_dir_all(&src)?;
    }
    for (path, bytes) in files {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
    }
    Ok(())
}

pub fn go(
    root: &Path,
    command: Vec<String>,
    agent_spec: Option<&str>,
    top: usize,
    attempts: usize,
    runs: usize,
    samples: usize,
) -> Result<()> {
    let scanned = scan(root, command)?;
    show::scan(&scanned);
    let picks: Vec<String> = scanned
        .hotspots
        .iter()
        .filter(|h| h.worth_it())
        .take(top.max(1))
        .map(|h| h.id.clone())
        .collect();
    ensure!(
        !picks.is_empty(),
        "nothing worth porting: no portable function takes {}% or more of the run",
        hotspot::WORTH_IT * 100.0
    );

    let Some(agent_spec) = agent_spec else {
        for id in &picks {
            show::port(&port(root, Some(id), samples)?, false);
        }
        ui::line("");
        ui::line(format!(
            "  Now write the Rust: hand each TASK.md to your coding agent (or pass {}), then:",
            ui::bold("--agent claude")
        ));
        for id in &picks {
            show::next(&format!("fnr verify {id} && fnr enable {id}"));
        }
        show::next("fnr bench");
        return Ok(());
    };

    let mut enabled = 0;
    for id in &picks {
        show::port(&port(root, Some(id), samples)?, false);
        let v = agent_port(root, id, agent_spec, attempts)?;
        if v.ok() && v.slower() {
            ui::warn(format!(
                "{id}: right, but slower in Rust per call; {}",
                slower_hint()
            ));
        } else if v.ok() {
            enable(root, Some(id), false)?;
            ui::ok(format!("{} now runs in Rust", ui::bold(id)));
            enabled += 1;
        } else {
            ui::warn(format!(
                "{id}: not right after {attempts} attempts; fix src/lib.rs by hand (see TASK.md), then  fnr verify {id}"
            ));
        }
    }
    ensure!(
        enabled > 0,
        "no port both passed verification and ran faster"
    );
    show::bench(&bench(root, runs)?);
    show::report(&report(root)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Micro;

    #[test]
    fn slow_but_right_gets_asked_for_speed() {
        let v = Verify {
            built: true,
            total: 10,
            passed: 10,
            micro: Some(Micro {
                original_ns: 1000.0,
                rust_ns: 4000.0,
                speedup: 0.25,
                ..Micro::default()
            }),
            ..Verify::default()
        };
        let text = feedback(&v);
        assert!(
            text.contains("right but slow")
                && text.contains("All 10 cases match")
                && text.contains("4.0× slower"),
            "{text}"
        );
        let wrong = Verify { failed: 1, ..v };
        assert!(feedback(&wrong).contains("failed"));
    }

    fn project(name: &str) -> (PathBuf, Port) {
        let root = std::env::temp_dir().join(format!("fa-pipeline-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let crate_dir = ".fast/ports/demo";
        fs::create_dir_all(root.join(crate_dir).join("src")).unwrap();
        fs::write(
            root.join(crate_dir).join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(root.join(crate_dir).join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        fs::write(root.join("app.py"), "def f():\n    pass\n").unwrap();
        let port = Port {
            id: "demo".into(),
            file: "app.py".into(),
            module: "app".into(),
            func: "f".into(),
            ext: "fa_demo".into(),
            crate_dir: crate_dir.into(),
            recorded: 1,
            calls_seen: 1,
            verify: None,
            enabled: false,
            lang: Lang::Python,
            owner: None,
            instance_method: false,
            self_fields: vec![],
            line: 1,
            sources: vec![],
            build_profile: None,
        };
        (root, port)
    }

    #[test]
    fn feedback_with_reports_below_min_speedup() {
        let v = Verify {
            built: true,
            total: 10,
            passed: 10,
            micro: Some(Micro {
                original_ns: 200.0,
                rust_ns: 100.0,
                speedup: 2.0,
                ..Micro::default()
            }),
            ..Verify::default()
        };
        let opts = VerifyOptions {
            min_speedup: Some(5.0),
            ..VerifyOptions::default()
        };
        let text = feedback_with(&v, &opts);
        assert!(text.contains("--min-speedup"), "{text}");
        assert!(text.contains("5.00x"), "{text}");
    }

    #[test]
    fn enable_requires_the_crate_that_passed_verify() {
        let (root, mut port) = project("enable-hash");
        let hash = backend::source_hash(&root.join(&port.crate_dir)).unwrap();
        fs::write(
            root.join(&port.crate_dir).join("src/lib.rs"),
            "pub fn f() { panic!() }\n",
        )
        .unwrap();
        port.verify = Some(Verify {
            built: true,
            total: 1,
            passed: 1,
            source_hash: Some(hash),
            ..Verify::default()
        });
        let mut ctx = Ctx::open(&root).unwrap();
        ctx.state.ports.insert(port.id.clone(), port);
        ctx.save().unwrap();

        let err = enable(&root, Some("demo"), false).unwrap_err();
        assert!(
            err.to_string()
                .contains("Rust changed since it passed verify"),
            "{err}"
        );
        assert!(!Ctx::open(&root).unwrap().state.ports["demo"].enabled);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn live_build_guard_covers_missing_changed_and_legacy_verify() {
        let (root, mut port) = project("build-hash");
        let ctx = Ctx::open(&root).unwrap();
        assert!(crate_needs_verify(&ctx, &port).unwrap());

        port.verify = Some(Verify::default());
        assert!(
            !crate_needs_verify(&ctx, &port).unwrap(),
            "legacy verify without a hash keeps working"
        );

        let hash = backend::source_hash(&root.join(&port.crate_dir)).unwrap();
        port.verify = Some(Verify {
            source_hash: Some(hash),
            ..Verify::default()
        });
        assert!(!crate_needs_verify(&ctx, &port).unwrap());
        fs::write(
            root.join(&port.crate_dir).join("Cargo.toml"),
            "not a valid manifest\n",
        )
        .unwrap();
        assert!(crate_needs_verify(&ctx, &port).unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn snapshot_restores_crate_inputs_without_touching_run_artifacts() {
        let dir = std::env::temp_dir().join(format!("fa-agent-snapshot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src/nested")).unwrap();
        fs::write(dir.join("Cargo.toml"), "best manifest\n").unwrap();
        fs::write(dir.join("Cargo.lock"), "best lock\n").unwrap();
        fs::write(dir.join("src/lib.rs"), "best lib\n").unwrap();
        fs::write(dir.join("src/nested/helper.rs"), "best helper\n").unwrap();
        for name in ["TASK.md", "recordings.jsonl", "agent.log"] {
            fs::write(dir.join(name), "before\n").unwrap();
        }
        let saved = snapshot(&dir).unwrap();

        fs::write(dir.join("Cargo.toml"), "later manifest\n").unwrap();
        fs::write(dir.join("Cargo.lock"), "later lock\n").unwrap();
        fs::write(dir.join("src/lib.rs"), "later lib\n").unwrap();
        fs::write(dir.join("src/extra.rs"), "later extra\n").unwrap();
        fs::write(dir.join("build.rs"), "later build script\n").unwrap();
        fs::remove_file(dir.join("src/nested/helper.rs")).unwrap();
        for name in ["TASK.md", "recordings.jsonl", "agent.log"] {
            fs::write(dir.join(name), "later evidence\n").unwrap();
        }

        restore_snapshot(&dir, &saved).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("Cargo.toml")).unwrap(),
            "best manifest\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("Cargo.lock")).unwrap(),
            "best lock\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "best lib\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("src/nested/helper.rs")).unwrap(),
            "best helper\n"
        );
        assert!(!dir.join("src/extra.rs").exists());
        assert!(!dir.join("build.rs").exists());
        for name in ["TASK.md", "recordings.jsonl", "agent.log"] {
            assert_eq!(
                fs::read_to_string(dir.join(name)).unwrap(),
                "later evidence\n"
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    fn enabled_project(name: &str, text: &str) -> (PathBuf, String) {
        let root = std::env::temp_dir().join(format!("fa-disable-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("app.py"), text).unwrap();
        let port = Port {
            id: "demo".into(),
            file: "app.py".into(),
            module: "app".into(),
            func: "f".into(),
            ext: "fa_demo".into(),
            crate_dir: ".fast/ports/demo".into(),
            recorded: 1,
            calls_seen: 1,
            verify: None,
            enabled: true,
            lang: Lang::Python,
            owner: None,
            instance_method: false,
            self_fields: vec![],
            line: 1,
            sources: vec![],
            build_profile: None,
        };
        let id = port.id.clone();
        let mut ctx = Ctx::open(&root).unwrap();
        ctx.state.ports.insert(port.id.clone(), port);
        ctx.save().unwrap();
        (root, id)
    }

    #[test]
    fn disable_rejects_damaged_blocks_without_changing_state() {
        let (begin, end) = backend::markers("#", "demo");
        for (name, text) in [
            ("missing-end", format!("{begin}\nf = rust\n")),
            ("missing-begin", format!("{end}\n")),
        ] {
            let (root, id) = enabled_project(name, &text);
            let err = disable(&root, Some(&id)).unwrap_err();
            assert!(err.to_string().contains("incomplete"), "{err}");
            assert_eq!(fs::read_to_string(root.join("app.py")).unwrap(), text);
            assert!(Ctx::open(&root).unwrap().state.ports[&id].enabled);
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn disable_removes_a_complete_block() {
        let (begin, end) = backend::markers("#", "demo");
        let original = "def f():\n    pass\n";
        let text = format!("{original}\n{begin}\nf = rust\n{end}\n");
        let (root, id) = enabled_project("complete", &text);
        disable(&root, Some(&id)).unwrap();
        assert_eq!(fs::read_to_string(root.join("app.py")).unwrap(), original);
        assert!(!Ctx::open(&root).unwrap().state.ports[&id].enabled);
        let _ = fs::remove_dir_all(root);
    }
}
