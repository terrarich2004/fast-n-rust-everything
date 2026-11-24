//! What every language backend does, and the parts they share: building the crate and
//! the marked blocks appended to source files.

use crate::hotspot::Hotspot;
use crate::state::{Ctx, Port, Verify};
use crate::util::{self, combined, output, tail};
use crate::{cfamily, csharp, golang, java, kotlin, node, php, python, r, ruby};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    Python,
    Node,
    Ruby,
    Java,
    Go,
    Php,
    C,
    Cpp,
    #[serde(rename = "csharp")]
    CSharp,
    R,
    Kotlin,
}

#[derive(Deserialize)]
pub struct Profile {
    pub wall: f64,
    pub base: String,
    pub functions: Vec<Hotspot>,
}

#[derive(Deserialize)]
pub struct Recording {
    pub calls: u64,
    pub kept: usize,
    pub skipped: usize,
    pub unsupported: Vec<String>,
    /// The function changed an argument in place during a recorded call.
    #[serde(default)]
    pub mutates: bool,
}

/// Launchers that can't be unwrapped the way uv run or poetry run are (see python::unwrap_env_runner).
fn wrapper_hint(program: &str) -> Option<&'static str> {
    Some(match crate::util::program_stem(program).as_str() {
        "tsx" | "ts-node" | "npx" => {
            "Node.js 22.6+ runs TypeScript itself, and fast-n-rust needs node to start the app:\n    fnr scan -- node app.ts"
        }
        "next" | "vite" | "webpack" | "nuxt" | "astro" => {
            "Dev bundlers wrap your app in a build server. Profile your backend script or a CPU workload directly:\n    fnr scan -- node bench.ts  (or `fnr scan --duration 10 -- tsx server.ts`)"
        }
        "fastapi" => {
            "Run FastAPI via uvicorn or python directly so fnr can attach cProfile:\n    fnr scan --duration 10 -- uvicorn app:app"
        }
        "bundle" => {
            "bundle exec sets up gems for its own process only. Scan with ruby directly, and if the app needs Bundler, put  require \"bundler/setup\"  at the top of its script:\n    fnr scan -- ruby app.rb"
        }
        _ => return None,
    })
}

impl Lang {
    pub fn detect(program: &str) -> Result<Lang> {
        if python::is_python(program) {
            Ok(Lang::Python)
        } else if node::is_node(program) {
            Ok(Lang::Node)
        } else if ruby::is_ruby(program) {
            Ok(Lang::Ruby)
        } else if java::is_java(program) {
            Ok(Lang::Java)
        } else if golang::is_go(program) {
            Ok(Lang::Go)
        } else if php::is_php(program) {
            Ok(Lang::Php)
        } else if csharp::is_csharp(program) {
            Ok(Lang::CSharp)
        } else if r::is_r(program) {
            Ok(Lang::R)
        } else if let Some(lang) = cfamily::detect(program) {
            Ok(lang)
        } else if let Some(hint) = wrapper_hint(program) {
            bail!("{hint}")
        } else {
            bail!(
                "fast-n-rust speaks Python, Node.js (JavaScript/TypeScript), Ruby, Java, Go, PHP, C, C++, C# and R. Start the command with one of them:\n    fnr scan -- python app.py\n    fnr scan -- node app.js  (or bun app.ts, deno run -A app.ts)\n    fnr scan -- ruby app.rb\n    fnr scan -- java App.java\n    fnr scan -- go run .\n    fnr scan -- php app.php\n    fnr scan -- clang -O2 main.c  (or gcc, g++, clang++, icpx)\n    fnr scan -- dotnet run -c Release\n    fnr scan -- Rscript app.R"
            )
        }
    }

    /// Ports called through the C entry point, with the port in src/port.rs.
    pub fn c_bridge(self) -> bool {
        matches!(
            self,
            Lang::Ruby
                | Lang::Java
                | Lang::Go
                | Lang::Php
                | Lang::C
                | Lang::Cpp
                | Lang::CSharp
                | Lang::Kotlin
                | Lang::R
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Lang::Python => "Python",
            Lang::Node => "JavaScript",
            Lang::Ruby => "Ruby",
            Lang::Java => "Java",
            Lang::Go => "Go",
            Lang::Php => "PHP",
            Lang::C => "C",
            Lang::Cpp => "C++",
            Lang::CSharp => "C#",
            Lang::R => "R",
            Lang::Kotlin => "Kotlin",
        }
    }

    pub fn resolve(self, program: &str, root: &Path) -> Result<String> {
        match self {
            Lang::Python => python::resolve(program, root),
            Lang::Node => node::resolve(program, root),
            Lang::Ruby => ruby::resolve(program, root),
            Lang::Java => java::resolve(program, root),
            Lang::Go => golang::resolve(program, root),
            Lang::Php => php::resolve(program, root),
            Lang::C | Lang::Cpp => cfamily::resolve(program, root),
            Lang::CSharp => csharp::resolve(program, root),
            Lang::R => r::resolve(program, root),
            Lang::Kotlin => java::resolve(program, root),
        }
    }

    pub fn profile(self, ctx: &Ctx, exe: &str, args: &[String]) -> Result<Profile> {
        match self {
            Lang::Python => python::profile(ctx, exe, args),
            Lang::Node => node::profile(ctx, exe, args),
            Lang::Ruby => ruby::profile(ctx, exe, args),
            Lang::Java => java::profile(ctx, exe, args),
            Lang::Go => golang::profile(ctx, exe, args),
            Lang::Php => php::profile(ctx, exe, args),
            Lang::C | Lang::Cpp => cfamily::profile(ctx, exe, args, self),
            Lang::CSharp => csharp::profile(ctx, exe, args),
            Lang::R => r::profile(ctx, exe, args),
            Lang::Kotlin => kotlin::profile(ctx, exe, args),
        }
    }

    pub fn record(
        self,
        ctx: &Ctx,
        exe: &str,
        args: &[String],
        hs: &Hotspot,
        out: &Path,
        limit: usize,
    ) -> Result<Recording> {
        match self {
            Lang::Python => python::record(ctx, exe, args, hs, out, limit),
            Lang::Node => node::record(ctx, exe, args, hs, out, limit),
            Lang::Ruby => ruby::record(ctx, exe, args, hs, out, limit),
            Lang::Java => java::record(ctx, exe, args, hs, out, limit),
            Lang::Go => golang::record(ctx, exe, args, hs, out, limit),
            Lang::Php => php::record(ctx, exe, args, hs, out, limit),
            Lang::C | Lang::Cpp => cfamily::record(ctx, exe, args, hs, out, self),
            Lang::CSharp => csharp::record(ctx, exe, args, hs, out, limit),
            Lang::R => r::record(ctx, exe, args, hs, out, limit),
            Lang::Kotlin => kotlin::record(ctx, exe, args, hs, out, limit),
        }
    }

    pub fn scaffold(
        self,
        ctx: &Ctx,
        hs: &Hotspot,
        helpers: &[Hotspot],
        port: &Port,
    ) -> Result<String> {
        match self {
            Lang::Python => python::scaffold(ctx, hs, helpers, port),
            Lang::Node => node::scaffold(ctx, hs, helpers, port),
            Lang::Ruby => ruby::scaffold(ctx, hs, helpers, port),
            Lang::Java => java::scaffold(ctx, hs, helpers, port),
            Lang::Go => golang::scaffold(ctx, hs, helpers, port),
            Lang::Php => php::scaffold(ctx, hs, helpers, port),
            Lang::C | Lang::Cpp => cfamily::scaffold(ctx, hs, helpers, port),
            Lang::CSharp => csharp::scaffold(ctx, hs, helpers, port),
            Lang::R => r::scaffold(ctx, hs, helpers, port),
            Lang::Kotlin => kotlin::scaffold(ctx, hs, helpers, port),
        }
    }

    pub fn verify(self, ctx: &Ctx, port: &Port) -> Result<Verify> {
        self.verify_with(ctx, port, None, None)
    }

    pub fn verify_with(
        self,
        ctx: &Ctx,
        port: &Port,
        float_tolerance: Option<f64>,
        known_diffs: Option<&Path>,
    ) -> Result<Verify> {
        match self {
            Lang::Python => python::verify_with(ctx, port, float_tolerance, known_diffs),
            Lang::Node => node::verify_with(ctx, port, float_tolerance, known_diffs),
            Lang::Go => crate::native::verify_with(ctx, port, true, float_tolerance, known_diffs),
            Lang::Ruby
            | Lang::Java
            | Lang::Php
            | Lang::C
            | Lang::Cpp
            | Lang::CSharp
            | Lang::R
            | Lang::Kotlin => {
                crate::native::verify_with(ctx, port, false, float_tolerance, known_diffs)
            }
        }
    }

    pub fn fuzz_cases(
        self,
        ctx: &Ctx,
        port: &Port,
        cases_path: &Path,
        out_path: &Path,
        float_tolerance: Option<f64>,
    ) -> Result<crate::native::NativeFuzzReport> {
        match self {
            Lang::Python => python::fuzz_cases(ctx, port, cases_path, out_path, float_tolerance),
            Lang::Node => node::fuzz_cases(ctx, port, cases_path, out_path, float_tolerance),
            Lang::C | Lang::Cpp => {
                cfamily::fuzz_cases(ctx, port, cases_path, out_path, float_tolerance)
            }
            Lang::Java => java::fuzz_cases(ctx, port, cases_path, out_path, float_tolerance),
            Lang::CSharp => csharp::fuzz_cases(ctx, port, cases_path, out_path, float_tolerance),
            Lang::Php => php::fuzz_cases(ctx, port, cases_path, out_path, float_tolerance),
            Lang::Ruby => ruby::fuzz_cases(ctx, port, cases_path, out_path, float_tolerance),
            Lang::R => r::fuzz_cases(ctx, port, cases_path, out_path, float_tolerance),
            Lang::Go => {
                let mut rep = crate::native::fuzz_cases(
                    ctx,
                    port,
                    true,
                    cases_path,
                    out_path,
                    float_tolerance,
                )?;
                rep.oracle_note = Some(
                    "Go differential fuzz oracle not enabled; ran crash+determinism check".into(),
                );
                Ok(rep)
            }
            Lang::Kotlin => {
                let mut rep = crate::native::fuzz_cases(
                    ctx,
                    port,
                    false,
                    cases_path,
                    out_path,
                    float_tolerance,
                )?;
                rep.oracle_note = Some(
                    "Kotlin differential fuzz oracle not enabled; ran crash+determinism check"
                        .into(),
                );
                Ok(rep)
            }
        }
    }

    pub fn enable(self, ctx: &Ctx, port: &Port) -> Result<()> {
        match self {
            Lang::Python => python::enable(ctx, port),
            Lang::Node => node::enable(ctx, port),
            Lang::Ruby => ruby::enable(ctx, port),
            Lang::Java => java::enable(ctx, port),
            Lang::Go => golang::enable(ctx, port),
            Lang::Php => php::enable(ctx, port),
            Lang::C | Lang::Cpp => cfamily::enable(ctx, port),
            Lang::CSharp => csharp::enable(ctx, port),
            Lang::R => r::enable(ctx, port),
            Lang::Kotlin => kotlin::enable(ctx, port),
        }
    }

    pub fn disable(self, ctx: &Ctx, port: &Port) -> Result<()> {
        match self {
            Lang::Python => python::disable(ctx, port),
            Lang::Node => node::disable(ctx, port),
            Lang::Ruby => ruby::disable(ctx, port),
            Lang::Java => java::disable(ctx, port),
            Lang::Go => golang::disable(ctx, port),
            Lang::Php => php::disable(ctx, port),
            Lang::C | Lang::Cpp => cfamily::disable(ctx, port),
            Lang::CSharp => csharp::disable(ctx, port),
            Lang::R => r::disable(ctx, port),
            Lang::Kotlin => kotlin::disable(ctx, port),
        }
    }

    /// What bench runs: (program, args). Go apps are built once, so the compiler isn't timed.
    pub fn bench_command(self, ctx: &Ctx) -> Result<(String, Vec<String>)> {
        let exe = ctx.runtime()?;
        match self {
            Lang::Go => golang::bench_command(ctx, exe),
            Lang::Java | Lang::Kotlin => java::bench_command(ctx, exe),
            Lang::C | Lang::Cpp => cfamily::bench_command(ctx, exe),
            Lang::CSharp => csharp::bench_command(ctx, exe),
            _ => Ok((exe.to_string(), ctx.state.command[1..].to_vec())),
        }
    }

    /// (file cargo builds, name it gets in .fast/lib)
    pub fn artifact(self, ext: &str) -> (String, String) {
        let built = if cfg!(windows) {
            format!("{ext}.dll")
        } else if cfg!(target_os = "macos") {
            format!("lib{ext}.dylib")
        } else {
            format!("lib{ext}.so")
        };
        let dest = match self {
            Lang::Python if cfg!(windows) => format!("{ext}.pyd"),
            Lang::Python => format!("{ext}.abi3.so"),
            Lang::Node => format!("{ext}.node"),
            // loaded through a C FFI (Fiddle, FFM, ...): the platform's own library name
            Lang::Ruby
            | Lang::Java
            | Lang::Go
            | Lang::Php
            | Lang::C
            | Lang::Cpp
            | Lang::CSharp
            | Lang::Kotlin
            | Lang::R => built.clone(),
        };
        (built, dest)
    }
}

/// Settings shared by every generated crate: macOS extension linking (host-specific `-C target-cpu=native`
/// is opt-in via `--native-cpu` so default/packaged builds stay portable across machines).
pub const CARGO_CONFIG: &str = "# Extensions on macOS resolve the host's symbols at load time.\n\
[target.aarch64-apple-darwin]\nrustflags = [\"-C\", \"link-arg=-undefined\", \"-C\", \"link-arg=dynamic_lookup\"]\n\n\
[target.x86_64-apple-darwin]\nrustflags = [\"-C\", \"link-arg=-undefined\", \"-C\", \"link-arg=dynamic_lookup\"]\n";

/// Two-phase build profile: fast `--release` (`lto = false`) during agent verify loops,
/// and `--profile ship` (`lto = "thin"`, `codegen-units = 1`) when enabling or packaging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BuildProfile {
    #[default]
    Fast,
    Ship,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BuildOptions {
    pub profile: BuildProfile,
    pub native_cpu: bool,
}

/// Fast release profile for verification iterations + `ship` profile with LTO for final enable/bench/package.
pub const RELEASE_PROFILE: &str = "[profile.release]\nopt-level = 3\nlto = false\n\n\
[profile.ship]\ninherits = \"release\"\nlto = \"thin\"\ncodegen-units = 1\n\n\
# A crate of its own, even inside another Cargo workspace.\n[workspace]\n";

fn compiler_errors(log: &str) -> String {
    let noise = [
        "Compiling",
        "Finished",
        "Updating",
        "Locking",
        "Downloading",
        "Downloaded",
        "Blocking",
        "Adding",
        "Checking",
    ];
    let kept: Vec<&str> = log
        .lines()
        .filter(|l| !noise.iter().any(|n| l.trim_start().starts_with(n)))
        .collect();
    tail(&kept.join("\n"), 60)
}

/// Target directory used when compiling hotspot ports (`FNR_CACHE_DIR` / `CARGO_TARGET_DIR`, or `.fast/target`).
pub fn target_dir(ctx: &Ctx) -> PathBuf {
    for var in ["FNR_CACHE_DIR", "CARGO_TARGET_DIR"] {
        if let Some(dir) = std::env::var_os(var).filter(|d| !d.is_empty()) {
            let dir = PathBuf::from(dir);
            return std::path::absolute(&dir).unwrap_or(dir);
        }
    }
    ctx.fast.join("target")
}

/// Release-build a port into .fast/lib using the fast verify profile.
pub fn build(ctx: &Ctx, port: &Port) -> Result<std::result::Result<PathBuf, String>> {
    build_with_options(ctx, port, BuildOptions::default())
}

/// Build a port into .fast/lib using either `Fast` (`--release`, no LTO) or `Ship` (`lto = "thin"`, `codegen-units = 1`).
pub fn build_with_profile(
    ctx: &Ctx,
    port: &Port,
    profile: BuildProfile,
) -> Result<std::result::Result<PathBuf, String>> {
    build_with_options(
        ctx,
        port,
        BuildOptions {
            profile,
            native_cpu: false,
        },
    )
}

pub fn build_with_options(
    ctx: &Ctx,
    port: &Port,
    opts: BuildOptions,
) -> Result<std::result::Result<PathBuf, String>> {
    build_inner(ctx, port, opts, &[])
}

fn encode_cargo_rustflags(
    inherited_encoded: Option<&str>,
    inherited_raw: Option<&str>,
    native_cpu: bool,
    is_macos: bool,
    extra: &[String],
) -> Option<String> {
    let mut flags: Vec<String> = if let Some(enc) = inherited_encoded
        && !enc.is_empty()
    {
        enc.split('\x1f').map(str::to_string).collect()
    } else if let Some(raw) = inherited_raw
        && !raw.trim().is_empty()
    {
        raw.split_whitespace().map(str::to_string).collect()
    } else {
        Vec::new()
    };

    let had_inherited = !flags.is_empty();
    if native_cpu {
        flags.push("-C".into());
        flags.push("target-cpu=native".into());
    }
    if is_macos && (!extra.is_empty() || native_cpu || had_inherited) {
        let has_dynamic_lookup = flags.iter().any(|f| f.contains("dynamic_lookup"));
        if !has_dynamic_lookup {
            flags.push("-C".into());
            flags.push("link-arg=-undefined".into());
            flags.push("-C".into());
            flags.push("link-arg=dynamic_lookup".into());
        }
    }
    flags.extend(extra.iter().cloned());
    if flags.is_empty() {
        None
    } else {
        Some(flags.join("\x1f"))
    }
}

fn build_inner(
    ctx: &Ctx,
    port: &Port,
    opts: BuildOptions,
    extra_rustflags: &[String],
) -> Result<std::result::Result<PathBuf, String>> {
    let dir = ctx.crate_dir(port);
    ensure!(
        dir.join("Cargo.toml").exists(),
        "{} has no crate yet: run  fnr port {}",
        port.id,
        port.id
    );
    let pyo3_python = if port.lang == Lang::Python {
        if let Some(rt) = ctx.state.runtime.as_deref() {
            Some(rt.to_string())
        } else if let Ok(env_py) = std::env::var("PYO3_PYTHON")
            && !env_py.trim().is_empty()
        {
            Some(env_py)
        } else {
            Some(
                crate::python::resolve("python3", &ctx.root)
                    .or_else(|_| crate::python::resolve("python", &ctx.root))?,
            )
        }
    } else {
        None
    };
    let target = target_dir(ctx);
    let has_ship = fs::read_to_string(dir.join("Cargo.toml"))
        .map(|t| t.contains("[profile.ship]"))
        .unwrap_or(false);
    let mut cmd = Command::new("cargo");
    let target_sub = match (opts.profile, has_ship) {
        (BuildProfile::Ship, true) => {
            cmd.args(["build", "--profile", "ship", "--message-format=short"]);
            "ship"
        }
        (BuildProfile::Ship, false) => {
            cmd.args(["build", "--release", "--message-format=short"])
                .env("CARGO_PROFILE_RELEASE_LTO", "thin")
                .env("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "1");
            "release"
        }
        (BuildProfile::Fast, _) => {
            cmd.args(["build", "--release", "--message-format=short"]);
            "release"
        }
    };
    cmd.current_dir(&dir).env("CARGO_TARGET_DIR", &target);
    if let Some(ref exe) = pyo3_python {
        cmd.env("PYO3_PYTHON", exe);
    }
    if !extra_rustflags.is_empty() {
        cmd.env("CARGO_INCREMENTAL", "0");
    }
    let inherited_encoded = std::env::var("CARGO_ENCODED_RUSTFLAGS").ok();
    let inherited_raw = std::env::var("RUSTFLAGS").ok();
    if let Some(encoded) = encode_cargo_rustflags(
        inherited_encoded.as_deref(),
        inherited_raw.as_deref(),
        opts.native_cpu,
        cfg!(target_os = "macos"),
        extra_rustflags,
    ) {
        cmd.env("CARGO_ENCODED_RUSTFLAGS", encoded);
        cmd.env_remove("RUSTFLAGS");
    }
    let out = output(&mut cmd, "cargo (install Rust from https://rustup.rs)")?;
    if !out.status.success() {
        return Ok(Err(compiler_errors(&String::from_utf8_lossy(&out.stderr))));
    }
    let (built, name) = port.lang.artifact(&port.ext);
    let lib_dir = ctx.lib_dir();
    fs::create_dir_all(&lib_dir)?;
    let dest = lib_dir.join(name);
    fs::copy(target.join(target_sub).join(&built), &dest)
        .with_context(|| format!("copying {built} into .fast/lib (is the app still running?)"))?;
    Ok(Ok(dest))
}

fn parse_llvm_major(text: &str) -> Option<u32> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("LLVM version") {
            let num = rest
                .trim_start_matches([':', ' '])
                .split(['.', '-', ' '])
                .next()?;
            if let Ok(v) = num.parse::<u32>() {
                return Some(v);
            }
        }
    }
    None
}

fn find_llvm_profdata() -> Result<PathBuf> {
    let rustc_vv = Command::new("rustc")
        .args(["-vV"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let rustc_llvm_major = rustc_vv.as_deref().and_then(parse_llvm_major);
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let host = rustc_vv.as_deref().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("host: ").map(|h| h.trim().to_string()))
    });
    if let (Some(sys), Some(host)) = (sysroot, host) {
        let bin = PathBuf::from(sys)
            .join("lib")
            .join("rustlib")
            .join(host)
            .join("bin")
            .join(if cfg!(windows) {
                "llvm-profdata.exe"
            } else {
                "llvm-profdata"
            });
        if bin.is_file() {
            return Ok(bin);
        }
    }
    if let Ok(out) = Command::new("llvm-profdata").arg("--version").output()
        && out.status.success()
    {
        let ver_text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if let (Some(want), Some(have)) = (rustc_llvm_major, parse_llvm_major(&ver_text))
            && want != have
        {
            bail!(
                "llvm-profdata on PATH is LLVM {have}, but rustc uses LLVM {want}; install matching tools with `rustup component add llvm-tools`"
            );
        }
        return Ok(PathBuf::from("llvm-profdata"));
    }
    bail!("llvm-profdata not found; install it with `rustup component add llvm-tools`");
}

/// Infer a minimal `Port` descriptor from a `.fast/ports/<dir>/Cargo.toml` for clean deploy builds
/// when `.fast/state.json` is absent (e.g. Docker builds or fresh CI checkouts).
pub fn infer_port_from_manifest(root: &Path, crate_dir: &Path) -> Result<Port> {
    let manifest = crate_dir.join("Cargo.toml");
    let text =
        fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
    let mut section = String::new();
    let mut pkg_name: Option<String> = None;
    let mut lib_name: Option<String> = None;
    let mut has_pyo3 = false;
    let mut has_napi = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.to_string();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            let val = v.trim().trim_matches('"');
            if key == "name" {
                if section == "[package]" {
                    pkg_name = Some(val.to_string());
                } else if section == "[lib]" {
                    lib_name = Some(val.to_string());
                }
            } else if section == "[dependencies]" {
                if key == "pyo3" {
                    has_pyo3 = true;
                } else if key == "napi" || key == "napi-derive" {
                    has_napi = true;
                }
            }
        }
    }
    let dir_name = crate_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("port");
    let ext = lib_name
        .or(pkg_name)
        .unwrap_or_else(|| format!("fa_{}", util::slug(dir_name)));
    let lang = if has_pyo3 {
        Lang::Python
    } else if has_napi {
        Lang::Node
    } else {
        Lang::C
    };
    let rel_dir = util::forward_slashes(&util::relative(root, crate_dir));
    Ok(Port {
        id: dir_name.to_string(),
        ext,
        crate_dir: rel_dir,
        lang,
        ..Port::default()
    })
}

/// Profile-Guided Optimization build for an already-enabled port:
/// instrumented Ship build -> workload training run -> `llvm-profdata merge` -> PGO Ship build.
pub fn build_pgo(ctx: &Ctx, port: &Port) -> Result<PathBuf> {
    build_pgo_with_options(ctx, port, false)
}

pub fn build_pgo_with_options(ctx: &Ctx, port: &Port, native_cpu: bool) -> Result<PathBuf> {
    let profdata_bin = find_llvm_profdata()?;
    let pgo_dir = ctx.fast.join("pgo").join(&port.ext);
    if pgo_dir.exists() {
        let _ = fs::remove_dir_all(&pgo_dir);
    }
    fs::create_dir_all(&pgo_dir)?;

    let opts = BuildOptions {
        profile: BuildProfile::Ship,
        native_cpu,
    };
    let gen_flags = [
        "-C".to_string(),
        format!("profile-generate={}", pgo_dir.display()),
    ];
    match build_inner(ctx, port, opts, &gen_flags)? {
        Ok(_) => {}
        Err(log) => bail!("PGO instrumented build failed:\n{log}"),
    }

    let (exe, args) = ctx.state.lang.bench_command(ctx)?;
    let prof_pattern = pgo_dir.join("default_%m_%p.profraw");
    let mut run_cmd = Command::new(&exe);
    run_cmd
        .args(&args)
        .current_dir(&ctx.root)
        .env("LLVM_PROFILE_FILE", &prof_pattern)
        .env_remove("FNR_DISABLE")
        .env_remove("FAST_ANYTHING_DISABLE")
        .stdin(std::process::Stdio::null());
    let run_out = output(&mut run_cmd, "running workload for PGO")?;
    ensure!(
        run_out.status.success(),
        "PGO training run failed ({}):\n{}",
        run_out.status,
        tail(&combined(&run_out), 20)
    );

    let profraws: Vec<PathBuf> = fs::read_dir(&pgo_dir)?
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "profraw"))
        .collect();
    ensure!(
        !profraws.is_empty(),
        "PGO training run produced no .profraw files in {} (was the port actually invoked?)",
        pgo_dir.display()
    );

    let merged = pgo_dir.join("merged.profdata");
    let mut merge_cmd = Command::new(&profdata_bin);
    merge_cmd
        .arg("merge")
        .arg("-o")
        .arg(&merged)
        .args(&profraws);
    let merge_out = output(&mut merge_cmd, "llvm-profdata")?;
    ensure!(
        merge_out.status.success(),
        "llvm-profdata merge failed:\n{}",
        tail(&combined(&merge_out), 20)
    );

    let use_flags = [
        "-C".to_string(),
        format!("profile-use={}", merged.display()),
    ];
    let dest = match build_inner(ctx, port, opts, &use_flags)? {
        Ok(p) => p,
        Err(log) => bail!("PGO optimized build failed:\n{log}"),
    };
    Ok(dest)
}

/// FNV-1a over the crate's Cargo.toml and everything under src/, so build can tell whether the
/// sources are still the ones verify checked. Stable across Rust versions, unlike DefaultHasher.
pub fn source_hash(dir: &Path) -> Result<String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                walk(&path, out)?
            } else {
                out.push(path)
            }
        }
        Ok(())
    }
    let mut files = vec![dir.join("Cargo.toml")];
    if dir.join("src").is_dir() {
        walk(&dir.join("src"), &mut files)?;
    }
    files.sort();
    let mut h: u64 = 0xcbf29ce484222325;
    for file in &files {
        let rel = file
            .strip_prefix(dir)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        for b in rel.bytes().chain([0]).chain(bytes).chain([0]) {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
    }
    Ok(format!("{h:016x}"))
}

// ------------------------------------------------------------------ marked blocks

pub fn markers(comment: &str, id: &str) -> (String, String) {
    (
        format!("{comment} >>> fnr: {id} >>>"),
        format!("{comment} <<< fnr: {id} <<<"),
    )
}

/// Legacy `fast-anything` marker lines, kept so existing projects disable/re-enable cleanly.
pub fn legacy_markers(comment: &str, id: &str) -> (String, String) {
    (
        format!("{comment} >>> fast-anything: {id} >>>"),
        format!("{comment} <<< fast-anything: {id} <<<"),
    )
}

fn newline(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

fn strip_pair(text: &str, begin: &str, end: &str) -> Option<String> {
    let start = text.find(begin)?;
    let len = text[start..].find(end)?;
    let nl = newline(text);
    let rest = &text[start + len + end.len()..];
    let rest = rest.strip_prefix(nl).unwrap_or(rest);
    if rest.is_empty() {
        // appended at the end, after one blank line
        let before = &text[..start];
        Some(before.strip_suffix(nl).unwrap_or(before).to_string())
    } else {
        // inserted before other code, followed by one blank line
        Some(format!(
            "{}{}",
            &text[..start],
            rest.strip_prefix(nl).unwrap_or(rest)
        ))
    }
}

/// Remove the block for id (either `fnr` or legacy `fast-anything`); the rest of the file comes back byte for byte.
pub fn strip_marked(text: &str, comment: &str, id: &str) -> String {
    let (begin, end) = markers(comment, id);
    let text = strip_pair(text, &begin, &end).unwrap_or_else(|| text.to_string());
    let (l_begin, l_end) = legacy_markers(comment, id);
    strip_pair(&text, &l_begin, &l_end).unwrap_or(text)
}

fn block(text: &str, comment: &str, id: &str, body: &str) -> String {
    let (begin, end) = markers(comment, id);
    format!("{begin}\n{body}\n{end}").replace('\n', newline(text))
}

/// Append (or replace) the block for id at the end of the file, after one blank line.
pub fn append_marked(text: &str, comment: &str, id: &str, body: &str) -> String {
    let text = strip_marked(text, comment, id);
    let nl = newline(&text);
    let gap = if text.ends_with('\n') {
        nl.to_string()
    } else {
        format!("{nl}{nl}")
    };
    format!("{text}{gap}{}{nl}", block(&text, comment, id, body))
}

/// Insert (or replace) the block for id right before the line at byte offset at.
pub fn insert_marked(
    text: &str,
    comment: &str,
    id: &str,
    body: &str,
    at: impl Fn(&str) -> Option<usize>,
) -> String {
    let text = strip_marked(text, comment, id);
    let Some(at) = at(&text) else {
        return append_marked(&text, comment, id, body);
    };
    let nl = newline(&text);
    format!(
        "{}{}{nl}{nl}{}",
        &text[..at],
        block(&text, comment, id, body),
        &text[at..]
    )
}

/// Byte offset of the first top-level line that starts with one of the prefixes.
pub fn line_starting(text: &str, prefixes: &[&str]) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let squashed: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        let top_level = !line.starts_with([' ', '\t']);
        if top_level && prefixes.iter().any(|p| squashed.starts_with(p)) {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_hash_follows_the_sources() {
        let dir = std::env::temp_dir().join(format!("fa-hash-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(dir.join("src/lib.rs"), "fn f() {}\n").unwrap();
        let first = source_hash(&dir).unwrap();
        assert_eq!(source_hash(&dir).unwrap(), first);
        fs::write(dir.join("TASK.md"), "not a source").unwrap();
        assert_eq!(
            source_hash(&dir).unwrap(),
            first,
            "only Cargo.toml and src/ count"
        );
        fs::write(dir.join("src/lib.rs"), "fn f() { }\n").unwrap();
        assert_ne!(source_hash(&dir).unwrap(), first);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn launchers_get_a_hint() {
        assert!(
            Lang::detect("npx")
                .unwrap_err()
                .to_string()
                .contains("node app.ts")
        );
        assert!(
            Lang::detect("tsx")
                .unwrap_err()
                .to_string()
                .contains("node app.ts")
        );
        assert!(
            Lang::detect("bundle")
                .unwrap_err()
                .to_string()
                .contains("bundler/setup")
        );
        assert!(
            Lang::detect("vite")
                .unwrap_err()
                .to_string()
                .contains("Dev bundlers")
        );
        assert!(
            Lang::detect("fastapi")
                .unwrap_err()
                .to_string()
                .contains("uvicorn")
        );
        assert!(
            Lang::detect("cobol")
                .unwrap_err()
                .to_string()
                .starts_with("fast-n-rust speaks")
        );
    }

    #[test]
    fn blocks_come_out_byte_for_byte() {
        let guard =
            |t: &str| line_starting(t, &["if__name__==\"__main__\":", "if__name__=='__main__':"]);
        for original in [
            "a = 1\n",
            "a = 1\r\n",
            "a = 1\n\n\nif __name__ == \"__main__\":\n    main()\n",
            "x",
        ] {
            let appended = append_marked(original, "#", "m.f", "f = g");
            assert!(appended.contains("# >>> fnr: m.f >>>"));
            assert_eq!(
                strip_marked(&appended, "#", "m.f"),
                original.to_string() + if original == "x" { "\n" } else { "" }
            );
            let legacy = appended.replace("fnr:", "fast-anything:");
            assert_eq!(
                strip_marked(&legacy, "#", "m.f"),
                original.to_string() + if original == "x" { "\n" } else { "" }
            );
            let inserted = insert_marked(original, "#", "m.f", "f = g", guard);
            assert_eq!(
                insert_marked(&inserted, "#", "m.f", "f = g", guard),
                inserted,
                "idempotent"
            );
            let back = strip_marked(&inserted, "#", "m.f");
            assert_eq!(back.trim_end(), original.trim_end());
            if original.contains("__main__") {
                assert_eq!(back, original);
                assert!(inserted.find(">>>").unwrap() < inserted.find("if __name__").unwrap());
            }
        }
        let two = append_marked(&append_marked("a\n", "#", "one", "1"), "#", "two", "2");
        assert_eq!(
            strip_marked(&strip_marked(&two, "#", "one"), "#", "two"),
            "a\n"
        );
    }

    #[test]
    fn encodes_rustflags_preserving_paths_with_spaces() {
        let extra = vec![
            "-C".to_string(),
            "profile-generate=/tmp/my project/.fast/pgo".to_string(),
        ];
        let encoded = encode_cargo_rustflags(None, Some("-C opt-level=3"), true, false, &extra)
            .expect("encoded flags");
        let parts: Vec<&str> = encoded.split('\x1f').collect();
        assert_eq!(
            parts,
            vec![
                "-C",
                "opt-level=3",
                "-C",
                "target-cpu=native",
                "-C",
                "profile-generate=/tmp/my project/.fast/pgo",
            ]
        );
        assert_eq!(encode_cargo_rustflags(None, None, false, false, &[]), None);
    }
}
