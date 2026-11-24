//! C and C++ backend: apps built with gcc or clang, e.g.  clang -O2 main.c fuzzy.c  (app arguments
//! after a second --). fast-anything builds the binary itself into .fast/bin. Profiled with a
//! one-line timer per function for one run, read with a small declaration parser, and bridged with
//! a block appended to the source file that loads the port with LoadLibrary or dlopen.

use crate::backend::{self, Lang, Profile, Recording};
use crate::hotspot::{Hotspot, Param};
use crate::native::{self, Flavor, Patches};
use crate::python::DISABLE_ENV;
use crate::state::{Ctx, Port, Verify};
use crate::util::{self, combined, output, tail};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROF_H: &str = include_str!("cfamily/prof.h");
const PROF_C: &str = include_str!("cfamily/prof.c");
const CLOCK_H: &str = include_str!("cfamily/clock.h");
const REC_H: &str = include_str!("cfamily/rec.h");
const REC_C: &str = include_str!("cfamily/rec.c");
const REC_HPP: &str = include_str!("cfamily/rec.hpp");
const JSON_HPP: &str = include_str!("cfamily/json.hpp");
const BIN_HPP: &str = include_str!("cfamily/bin.hpp");

/// Intel oneAPI: icx (C, clang-based), icpx and dpcpp (C++). icx-cl takes MSVC-style flags, so it isn't listed.
const C_COMPILERS: &[&str] = &["gcc", "clang", "cc", "icx"];
const CPP_COMPILERS: &[&str] = &["g++", "clang++", "c++", "icpx", "dpcpp"];
const C_EXT: &[&str] = &["c"];
const CPP_EXT: &[&str] = &["cc", "cpp", "cxx", "c++", "C"];
/// Compiler flags whose value is the next argument.
const VALUE_FLAGS: &[&str] = &[
    "-o", "-I", "-D", "-U", "-include", "-isystem", "-L", "-l", "-x", "-MF", "-MT", "-target",
];

fn base_name(program: &str) -> String {
    // gcc-14, clang-19, x86_64-linux-gnu-g++ ...
    let stem = util::program_stem(program);
    stem.rsplit('-')
        .find(|p| !p.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .unwrap_or(&stem)
        .to_string()
}

pub fn detect(program: &str) -> Option<Lang> {
    let b = base_name(program);
    if CPP_COMPILERS.contains(&b.as_str()) {
        Some(Lang::Cpp)
    } else if C_COMPILERS.contains(&b.as_str()) {
        Some(Lang::C)
    } else {
        None
    }
}

/// A C compiler given C++ sources still builds C++.
pub fn refine(lang: Lang, args: &[String]) -> Lang {
    if lang == Lang::C && Build::parse(args).is_ok_and(|b| b.sources.iter().any(|s| is_cpp_file(s)))
    {
        Lang::Cpp
    } else {
        lang
    }
}

fn is_cpp_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|e| CPP_EXT.contains(&e.to_string_lossy().as_ref()))
}

pub fn resolve(program: &str, root: &Path) -> Result<String> {
    let prog = if program.contains(['/', '\\']) {
        root.join(program)
    } else {
        PathBuf::from(program)
    };
    let out = output(
        Command::new(&prog).arg("--version").current_dir(root),
        program,
    )?;
    ensure!(
        out.status.success(),
        "{program} failed:\n{}",
        tail(&combined(&out), 5)
    );
    Ok(prog.to_string_lossy().into_owned())
}

#[derive(serde::Deserialize)]
struct CdbEntry {
    directory: String,
    file: String,
    #[serde(default)]
    arguments: Option<Vec<String>>,
    #[serde(default)]
    command: Option<String>,
}

fn split_shell(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut esc = false;
    let mut has_token = false;
    for ch in s.chars() {
        if esc {
            cur.push(ch);
            esc = false;
            has_token = true;
            continue;
        }
        if ch == '\\' && !in_single {
            esc = true;
            has_token = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            has_token = true;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            has_token = true;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if has_token {
                out.push(std::mem::take(&mut cur));
                has_token = false;
            }
            continue;
        }
        cur.push(ch);
        has_token = true;
    }
    if has_token {
        out.push(cur);
    }
    out
}

fn resolve_cdb_path(dir: &Path, raw: &str) -> String {
    let p = Path::new(raw);
    if p.is_absolute() {
        raw.to_string()
    } else {
        dir.join(p).to_string_lossy().into_owned()
    }
}

/// Converts a CMake/Meson/Ninja `compile_commands.json` into a normalized compiler command line
/// suitable for `fnr scan -- <cmd>` / `fnr go -- <cmd>`.
pub fn command_from_cdb(cdb_path: &Path, app_args: &[String]) -> Result<Vec<String>> {
    let text =
        fs::read_to_string(cdb_path).with_context(|| format!("reading {}", cdb_path.display()))?;
    let entries: Vec<CdbEntry> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", cdb_path.display()))?;
    ensure!(
        !entries.is_empty(),
        "{} has no compilation entries",
        cdb_path.display()
    );

    let mut compiler: Option<String> = None;
    let mut flags: Vec<String> = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    let mut seen_flags = std::collections::HashSet::new();
    let mut seen_sources = std::collections::HashSet::new();

    let mut push_flag = |f: String| {
        if seen_flags.insert(f.clone()) {
            flags.push(f);
        }
    };

    for entry in &entries {
        let argv: Vec<String> = if let Some(a) = &entry.arguments {
            a.clone()
        } else if let Some(cmd) = &entry.command {
            split_shell(cmd)
        } else {
            continue;
        };
        if argv.is_empty() {
            continue;
        }
        if compiler.is_none() {
            compiler = Some(argv[0].clone());
        }
        let dir = Path::new(&entry.directory);
        let src_resolved = resolve_cdb_path(dir, &entry.file);
        let src_ext = Path::new(&src_resolved)
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        if (C_EXT.contains(&src_ext.as_str()) || CPP_EXT.contains(&src_ext.as_str()))
            && seen_sources.insert(src_resolved.clone())
        {
            sources.push(src_resolved);
        }

        let mut i = 1;
        while i < argv.len() {
            let a = &argv[i];
            if a == "-c"
                || a == "-M"
                || a == "-MM"
                || a == "-MD"
                || a == "-MMD"
                || a == "-MG"
                || a == "-MP"
            {
                i += 1;
                continue;
            }
            if matches!(a.as_str(), "-o" | "-MF" | "-MT" | "-MQ") {
                i += 2;
                continue;
            }
            if a == "-I" || a == "-isystem" {
                if i + 1 < argv.len() {
                    push_flag(a.clone());
                    push_flag(resolve_cdb_path(dir, &argv[i + 1]));
                    i += 2;
                    continue;
                }
            } else if let Some(inc) = a.strip_prefix("-I")
                && !inc.is_empty()
            {
                push_flag(format!("-I{}", resolve_cdb_path(dir, inc)));
                i += 1;
                continue;
            }
            if a == &entry.file || resolve_cdb_path(dir, a) == resolve_cdb_path(dir, &entry.file) {
                i += 1;
                continue;
            }
            push_flag(a.clone());
            i += 1;
        }
    }

    ensure!(
        !sources.is_empty(),
        "no C/C++ source files found in {}",
        cdb_path.display()
    );
    let mut cmd = vec![compiler.unwrap_or_else(|| "cc".to_string())];
    cmd.extend(flags);
    cmd.extend(sources);
    if !app_args.is_empty() {
        cmd.push("--".to_string());
        cmd.extend(app_args.iter().cloned());
    }
    Ok(cmd)
}

/// compiler [flags] sources... [-o out] [-- app args]
struct Build {
    flags: Vec<String>,
    sources: Vec<String>,
    args: Vec<String>,
}

impl Build {
    fn parse(args: &[String]) -> Result<Build> {
        let (compile, app) = match args.iter().position(|a| a == "--") {
            Some(i) => (&args[..i], args[i + 1..].to_vec()),
            None => (args, Vec::new()),
        };
        let (mut flags, mut sources) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i < compile.len() {
            let a = &compile[i];
            if a == "-o" {
                i += 2;
                continue;
            }
            let ext = Path::new(a)
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !a.starts_with('-')
                && (C_EXT.contains(&ext.as_str()) || CPP_EXT.contains(&ext.as_str()))
            {
                sources.push(a.clone());
            } else {
                flags.push(a.clone());
                if VALUE_FLAGS.contains(&a.as_str()) && i + 1 < compile.len() {
                    flags.push(compile[i + 1].clone());
                    i += 1;
                }
            }
            i += 1;
        }
        ensure!(
            !sources.is_empty(),
            "give the sources to compile:  fnr scan -- clang -O2 main.c fuzzy.c  (app arguments after a second --)"
        );
        Ok(Build {
            flags,
            sources,
            args: app,
        })
    }
}

fn binary(ctx: &Ctx) -> PathBuf {
    ctx.fast
        .join("bin")
        .join(if cfg!(windows) { "app.exe" } else { "app" })
}

/// Copies the runtime files into .fast/cfa and returns that folder.
fn runtime_dir(ctx: &Ctx) -> Result<PathBuf> {
    let dir = ctx.fast.join("cfa");
    fs::create_dir_all(&dir)?;
    for (name, text) in [
        ("fa_prof.h", PROF_H.to_string()),
        ("fa_clock.h", CLOCK_H.to_string()),
        ("fa_rec.h", REC_H.to_string()),
        ("fa_rec.hpp", REC_HPP.replace("/*JSON*/", JSON_HPP)),
    ] {
        fs::write(dir.join(name), text)?;
    }
    Ok(dir)
}

fn compile(
    ctx: &Ctx,
    cc: &str,
    b: &Build,
    extra: &[PathBuf],
    include: Option<&Path>,
) -> Result<PathBuf> {
    let bin = binary(ctx);
    fs::create_dir_all(bin.parent().context("bin")?)?;
    let mut cmd = Command::new(cc);
    cmd.args(&b.flags)
        .args(&b.sources)
        .args(extra)
        .current_dir(&ctx.root)
        .arg("-o")
        .arg(&bin);
    if let Some(h) = include {
        cmd.arg("-include").arg(h);
    }
    let out = output(&mut cmd, cc)?;
    ensure!(
        out.status.success(),
        "building the app failed:\n{}",
        errors_first(&combined(&out))
    );
    Ok(bin)
}

/// Compiler output with the error lines (and a few after each) first: warnings can bury them.
fn errors_first(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    let mut keep = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.contains("error") && !l.contains("warning") {
            keep.extend(lines[i..(i + 4).min(lines.len())].iter().copied());
        }
    }
    if keep.is_empty() {
        tail(log, 30)
    } else {
        keep.into_iter().take(40).collect::<Vec<_>>().join("\n")
    }
}

fn run(
    ctx: &Ctx,
    bin: &Path,
    b: &Build,
    env: &[(&str, &Path)],
    derive: bool,
) -> Result<std::process::Output> {
    let mut cmd = Command::new(bin);
    cmd.args(&b.args)
        .current_dir(&ctx.root)
        .env(DISABLE_ENV, "1")
        .stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    if !derive {
        cmd.env("FAST_ANYTHING_RECORD_DERIVE", "0");
    }
    output(&mut cmd, "the app")
}

pub fn bench_command(ctx: &Ctx, cc: &str) -> Result<(String, Vec<String>)> {
    let b = Build::parse(&ctx.state.command[1..])?;
    let bin = compile(ctx, cc, &b, &[], None)?;
    Ok((bin.to_string_lossy().into_owned(), b.args))
}

// ------------------------------------------------------------------ reading the source

/// The source with comments, string and character literals and preprocessor lines blanked out
/// (same length, newlines kept), so braces and parentheses can be counted.
fn blank(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let mut line_start = true;
    let wipe = |out: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    while i < b.len() {
        let c = b[i];
        if line_start && c == b'#' {
            let start = i;
            while i < b.len() && !(b[i] == b'\n' && b[i - 1] != b'\\') {
                i += 1;
            }
            wipe(&mut out, start, i);
            continue;
        }
        if c == b'\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if !c.is_ascii_whitespace() {
            line_start = false;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            wipe(&mut out, start, i);
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            wipe(&mut out, start, i);
        } else if c == b'"' || c == b'\'' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != c && b[i] != b'\n' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
            wipe(&mut out, start + 1, i.saturating_sub(1).max(start + 1));
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

const SPECIFIERS: &[&str] = &[
    "static",
    "inline",
    "extern",
    "constexpr",
    "consteval",
    "virtual",
    "explicit",
    "friend",
    "__inline",
    "__forceinline",
    "register",
];
const KEYWORDS: &[&str] = &[
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "return",
    "sizeof",
    "else",
    "do",
    "case",
    "new",
    "delete",
    "throw",
    "alignof",
    "decltype",
    "static_assert",
    "defined",
];
const IO: &[&str] = &[
    "printf", "fprintf", "puts", "fputs", "putchar", "fputc", "fwrite", "fread", "fopen", "fclose",
    "fgets", "fgetc", "getchar", "scanf", "fscanf", "perror", "system", "exit", "abort", "getline",
    "cout", "cerr", "cin", "clog", "ofstream", "ifstream", "fstream", "sleep", "usleep", "Sleep",
    "remove", "rename", "getenv", "open", "read", "write", "close", "send", "recv", "socket",
    "thread", "async",
];
const NONDET: &[&str] = &[
    "rand",
    "srand",
    "random",
    "time",
    "clock",
    "random_device",
    "system_clock",
    "steady_clock",
    "high_resolution_clock",
    "mt19937",
    "default_random_engine",
];

#[derive(Debug, Clone)]
struct CFunc {
    name: String,
    namespaces: Vec<String>,
    class: Option<String>,
    is_static: bool,
    template: bool,
    body_open: usize,
    body_close: usize,
    params: Vec<Param>,
    ret: String,
}

#[derive(Debug)]
enum Scope {
    Namespace(String),
    Transparent,
    Class(String),
    Body(usize),
    Other,
}

fn match_back(s: &[u8], close: usize, open: u8, shut: u8) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = close as isize;
    while i >= 0 {
        let c = s[i as usize];
        if c == shut {
            depth += 1;
        } else if c == open {
            depth -= 1;
            if depth == 0 {
                return Some(i as usize);
            }
        }
        i -= 1;
    }
    None
}

fn split_top(s: &str, sep: u8) -> Vec<&str> {
    let b = s.as_bytes();
    let (mut depth, mut start, mut out) = (0i32, 0, Vec::new());
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' | b'<' | b'[' | b'{' => depth += 1,
            b')' | b'>' | b']' | b'}' => depth -= 1,
            _ if c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn squash(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" *", "*")
        .replace("* ", "*")
        .replace(" &", "&")
        .replace("& ", "&")
        .replace("< ", "<")
        .replace(" >", ">")
        .replace(", ", ",")
}

fn parse_params(list: &str) -> Vec<Param> {
    let list = list.trim();
    if list.is_empty() || list == "void" {
        return vec![];
    }
    split_top(list, b',')
        .into_iter()
        .map(|p| {
            let mut parts = split_top(p, b'=').into_iter();
            let decl = parts.next().unwrap_or_default().trim();
            let default = parts.next().map(|d| d.trim().to_string());
            let (decl, array) = match decl
                .strip_suffix(']')
                .and_then(|d| d.rfind('[').map(|i| &d[..i]))
            {
                Some(d) => (d.trim_end(), true),
                None => (decl, false),
            };
            let name_start = decl
                .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
                .map(|i| i + 1)
                .unwrap_or(0);
            let (ty, name) = (decl[..name_start].trim(), &decl[name_start..]);
            let (ty, name) = if ty.is_empty() || ty.ends_with("::") {
                (decl, "")
            } else {
                (ty, name)
            };
            let ty = if array {
                format!("{}*", ty.trim())
            } else {
                ty.to_string()
            };
            Param {
                name: name.to_string(),
                annotation: Some(squash(&ty)),
                default,
                kwonly: false,
            }
        })
        .collect()
}

fn parse(src: &str) -> Vec<CFunc> {
    let text = blank(src);
    let b = text.as_bytes();
    let mut stack: Vec<Scope> = Vec::new();
    let mut stmt = 0usize;
    let mut found: Vec<CFunc> = Vec::new();
    let decl_scope = |stack: &Vec<Scope>| {
        stack.iter().all(|s| {
            matches!(
                s,
                Scope::Namespace(_) | Scope::Transparent | Scope::Class(_)
            )
        })
    };
    for i in 0..b.len() {
        match b[i] {
            b'{' => {
                if !decl_scope(&stack) {
                    stack.push(Scope::Other);
                    continue;
                }
                let head = text[stmt..i].trim();
                let w: Vec<&str> = words(head).collect();
                let scope = if w.first() == Some(&"namespace") {
                    Scope::Namespace(w.get(1).map(|s| s.to_string()).unwrap_or_default())
                } else if w == ["extern"] {
                    Scope::Transparent
                } else if let Some(k) = w
                    .iter()
                    .position(|x| ["class", "struct", "union"].contains(x))
                    .filter(|_| !head.contains('(') && !head.contains('='))
                {
                    if w.get(k).is_some_and(|x| *x == "union") {
                        Scope::Other
                    } else {
                        Scope::Class(w.get(k + 1).map(|s| s.to_string()).unwrap_or_default())
                    }
                } else if let Some(f) = function_head(head, i, &stack) {
                    found.push(f);
                    Scope::Body(found.len() - 1)
                } else {
                    Scope::Other
                };
                stack.push(scope);
                stmt = i + 1;
            }
            b'}' => {
                if let Some(Scope::Body(k)) = stack.pop() {
                    found[k].body_close = i;
                }
                if decl_scope(&stack) {
                    stmt = i + 1;
                }
            }
            b';' if decl_scope(&stack) => stmt = i + 1,
            b':' if decl_scope(&stack)
                && b.get(i + 1) != Some(&b':')
                && i > 0
                && b[i - 1] != b':' =>
            {
                let head = text[stmt..i].trim();
                if ["public", "private", "protected"].contains(&head) {
                    stmt = i + 1;
                }
            }
            _ => {}
        }
    }
    found.retain(|f| f.body_close > f.body_open);
    found
}

fn function_head(head: &str, brace: usize, stack: &[Scope]) -> Option<CFunc> {
    let hb = head.as_bytes();
    let close = head.rfind(')')?;
    let after = &head[close + 1..];
    if !words(after).all(|w| ["const", "noexcept", "override", "final", "mutable"].contains(&w))
        || after.contains("->")
        || after.contains(':')
    {
        return None;
    }
    let open = match_back(hb, close, b'(', b')')?;
    let before = head[..open].trim_end();
    let name_start = before
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '~'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let name = &before[name_start..];
    let prefix = before[..name_start].trim();
    if name.is_empty()
        || prefix.is_empty()
        || KEYWORDS.contains(&name)
        || name.contains("operator")
        || name.contains('~')
    {
        return None;
    }
    let template = prefix.starts_with("template");
    let is_static = words(prefix).any(|w| w == "static");
    let mut ret = prefix.to_string();
    if template && let Some(gt) = ret.find('>') {
        ret = ret[gt + 1..].to_string();
    }
    let ret = squash(&words_except(&ret, SPECIFIERS));
    let namespaces = stack
        .iter()
        .filter_map(|s| {
            if let Scope::Namespace(n) = s {
                Some(n.clone())
            } else {
                None
            }
        })
        .collect();
    let class = stack.iter().rev().find_map(|s| {
        if let Scope::Class(n) = s {
            Some(n.clone())
        } else {
            None
        }
    });
    Some(CFunc {
        name: name.to_string(),
        namespaces,
        class,
        is_static,
        template,
        body_open: brace,
        body_close: 0,
        params: parse_params(&head[open + 1..close]),
        ret,
    })
}

/// Removes whole words (specifiers, attributes) from a declaration, keeping everything else.
fn words_except(s: &str, drop: &[&str]) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !drop.contains(&word.as_str()) {
            out.push_str(word);
        }
        word.clear();
    };
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    let out = out.trim().to_string();
    // [[nodiscard]] and __attribute__((...))
    let mut cleaned = out.clone();
    while let Some(a) = cleaned.find("[[") {
        let Some(z) = cleaned[a..].find("]]") else {
            break;
        };
        cleaned.replace_range(a..a + z + 2, "");
    }
    while let Some(a) = cleaned.find("__attribute__") {
        let rest = &cleaned[a..];
        let Some(open) = rest.find('(') else { break };
        let Some(close) = match_forward(rest.as_bytes(), open) else {
            break;
        };
        cleaned.replace_range(a..a + close + 1, "");
    }
    cleaned.trim().to_string()
}

fn match_forward(s: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, &c) in s.iter().enumerate().skip(open) {
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

// ------------------------------------------------------------------ types

#[derive(Debug, Clone, Copy, PartialEq)]
enum Scalar {
    Int { signed: bool, rust: &'static str },
    Float(&'static str),
    Bool,
}

fn scalar(t: &str) -> Option<Scalar> {
    let t = squash(t).replace("const ", "").replace("std::", "");
    let t = t.trim();
    let int = |signed, rust| Some(Scalar::Int { signed, rust });
    match t {
        "char" | "signed char" | "int8_t" => int(true, "i8"),
        "unsigned char" | "uint8_t" => int(false, "u8"),
        "short" | "short int" | "signed short" | "int16_t" => int(true, "i16"),
        "unsigned short" | "unsigned short int" | "uint16_t" => int(false, "u16"),
        "int" | "signed" | "signed int" | "int32_t" => int(true, "i32"),
        "unsigned" | "unsigned int" | "uint32_t" => int(false, "u32"),
        "long" | "long int" | "long long" | "long long int" | "int64_t" | "ssize_t"
        | "ptrdiff_t" | "intptr_t" => int(true, "i64"),
        "unsigned long"
        | "unsigned long int"
        | "unsigned long long"
        | "unsigned long long int"
        | "uint64_t"
        | "size_t"
        | "uintptr_t" => int(false, "u64"),
        "float" => Some(Scalar::Float("f32")),
        "double" | "long double" => Some(Scalar::Float("f64")),
        "bool" | "_Bool" => Some(Scalar::Bool),
        _ => None,
    }
}

/// What a C parameter is to the recorder and the bridge.
#[derive(Debug, Clone, PartialEq)]
enum CKind {
    Scalar(Scalar),
    Str,
    /// const T* data whose length is the parameter at this index
    Array(Scalar, usize),
    Len,
}

fn c_kinds(params: &[Param]) -> std::result::Result<Vec<CKind>, String> {
    let mut kinds: Vec<Option<CKind>> = vec![None; params.len()];
    for (i, p) in params.iter().enumerate() {
        let t = squash(p.annotation.as_deref().unwrap_or(""));
        if kinds[i].is_some() {
            continue;
        }
        if let Some(s) = scalar(&t) {
            kinds[i] = Some(CKind::Scalar(s));
        } else if t == "const char*" || t == "char const*" {
            kinds[i] = Some(CKind::Str);
        } else if let Some(inner) = t
            .strip_suffix('*')
            .filter(|_| t.starts_with("const ") || t.contains(" const*"))
        {
            let inner = inner.replace("const", "");
            let s = scalar(&inner).ok_or_else(|| format!("takes a {t}"))?;
            let len = params
                .get(i + 1)
                .and_then(|n| scalar(n.annotation.as_deref().unwrap_or("")));
            ensure_len(len, &t)?;
            kinds[i] = Some(CKind::Array(s, i + 1));
            kinds[i + 1] = Some(CKind::Len);
        } else if t.ends_with('*') {
            return Err(format!("takes a pointer it may write through ({t})"));
        } else {
            return Err(format!("takes a {t}"));
        }
    }
    Ok(kinds.into_iter().map(|k| k.unwrap_or(CKind::Len)).collect())
}

fn ensure_len(len: Option<Scalar>, t: &str) -> std::result::Result<(), String> {
    match len {
        Some(Scalar::Int { .. }) => Ok(()),
        _ => Err(format!("takes a {t} without a length right after it")),
    }
}

pub fn rust_type_cpp(t: &str) -> Option<String> {
    cpp_rust_type(t)
}

/// C++ types fast-n-rust can carry across: scalars, std::string, vector, array, map/unordered_map
/// with string keys and pair, by value or const reference.
fn cpp_rust_type(t: &str) -> Option<String> {
    let t = squash(t);
    let t = t
        .trim()
        .trim_start_matches("const ")
        .trim_end_matches('&')
        .trim();
    let t = t.strip_suffix(" const").unwrap_or(t).trim();
    if let Some(s) = scalar(t) {
        return Some(scalar_rust(s).into());
    }
    let t = t.strip_prefix("std::").unwrap_or(t);
    if t == "string" || t == "const char*" {
        return Some("String".into());
    }
    let (raw, inner) = t.strip_suffix('>')?.split_once('<')?;
    let args = split_top(inner, b',');
    Some(match (raw, args.as_slice()) {
        ("vector", [x, ..]) => format!("Vec<{}>", cpp_rust_type(x)?),
        ("array", [x, n]) => format!(
            "[{}; {}]",
            cpp_rust_type(x)?,
            n.trim().parse::<usize>().ok()?
        ),
        ("map", [k, v, ..]) if cpp_rust_type(k)? == "String" => {
            format!("std::collections::BTreeMap<String, {}>", cpp_rust_type(v)?)
        }
        ("unordered_map", [k, v, ..]) if cpp_rust_type(k)? == "String" => {
            format!("std::collections::HashMap<String, {}>", cpp_rust_type(v)?)
        }
        ("pair", [a, b]) => format!("({}, {})", cpp_rust_type(a)?, cpp_rust_type(b)?),
        _ => return None,
    })
}

fn scalar_rust(s: Scalar) -> &'static str {
    match s {
        Scalar::Int { rust, .. } | Scalar::Float(rust) => rust,
        Scalar::Bool => "bool",
    }
}

pub fn rust_type_c(t: &str) -> Option<String> {
    let t = squash(t);
    if let Some(s) = scalar(&t) {
        return Some(scalar_rust(s).into());
    }
    if t == "const char*" {
        return Some("String".into());
    }
    let inner = t.strip_suffix('*')?.replace("const", "");
    Some(format!("Vec<{}>", scalar_rust(scalar(&inner)?)))
}

fn blockers(
    f: &CFunc,
    body: &str,
    cpp: bool,
    funcs: &[CFunc],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    if f.class.is_some() && !f.is_static {
        blockers.push("instance method (static methods and free functions only for now)".into());
    }
    if f.name.contains("::") {
        blockers.push("method defined outside its class".into());
    }
    if f.template {
        blockers.push("template".into());
    }
    if f.name == "main" {
        blockers.push("the entry point".into());
    }
    if f.params.iter().any(|p| p.name.is_empty()) {
        blockers.push("has unnamed parameters".into());
    }
    if cpp {
        for p in &f.params {
            let t = p.annotation.clone().unwrap_or_default();
            if (t.ends_with('&') && !t.starts_with("const")) || t.ends_with("&&") {
                blockers.push(format!("takes a non-const reference ({t})"));
            } else if cpp_rust_type(&t).is_none() {
                blockers.push(format!("takes a {t}"));
            }
        }
        if f.ret == "void" {
            blockers.push("returns nothing (void)".into());
        } else if f.ret.ends_with('&') || f.ret.ends_with('*') || cpp_rust_type(&f.ret).is_none() {
            blockers.push(format!("returns a {}", f.ret));
        }
    } else {
        if let Err(e) = c_kinds(&f.params) {
            blockers.push(e);
        }
        if f.ret == "void" {
            blockers.push("returns nothing (void)".into());
        } else if scalar(&f.ret).is_none() {
            blockers.push(format!(
                "returns a {} (C ports return numbers or bools for now)",
                f.ret
            ));
        }
    }
    let b = body.as_bytes();
    let (mut io, mut nondet, mut helpers) = (Vec::new(), Vec::new(), Vec::new());
    let mut statics = false;
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let w = &body[start..i];
            let member = start >= 1
                && (b[start - 1] == b'.' || (start >= 2 && &body[start - 2..start] == "->"));
            let mut j = i;
            while j < b.len() && b[j] == b' ' {
                j += 1;
            }
            let call = b.get(j) == Some(&b'(');
            if w == "static" {
                statics = true;
            }
            if !member
                && IO.contains(&w)
                && (call
                    || [
                        "cout", "cerr", "cin", "clog", "ofstream", "ifstream", "fstream", "thread",
                    ]
                    .contains(&w))
            {
                io.push(w.to_string());
            }
            if !member && NONDET.contains(&w) {
                nondet.push(w.to_string());
            }
            if !member
                && call
                && w != f.name
                && funcs.iter().any(|g| g.name == w && g.class == f.class)
            {
                helpers.push(w.to_string());
            }
        } else {
            i += 1;
        }
    }
    io.dedup();
    nondet.dedup();
    helpers.sort();
    helpers.dedup();
    if !io.is_empty() {
        blockers.push(format!(
            "does I/O: {}",
            io.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !nondet.is_empty() {
        blockers.push(format!(
            "not deterministic: {}",
            nondet
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if statics {
        blockers.push("keeps static variables".into());
    }
    if !helpers.is_empty() {
        warnings.push(format!(
            "calls {}, they get ported together with it",
            helpers
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    (blockers, warnings, helpers)
}

struct Described {
    path: PathBuf,
    f: CFunc,
    hs: Hotspot,
}

fn line_of(src: &str, offset: usize) -> u32 {
    src[..offset.min(src.len())]
        .bytes()
        .filter(|&c| c == b'\n')
        .count() as u32
        + 1
}

fn describe(ctx: &Ctx, b: &Build, lang: Lang) -> Result<Vec<Described>> {
    let mut out = Vec::new();
    for s in &b.sources {
        let path = util::absolute(&ctx.root.join(s));
        let src = fs::read_to_string(&path).with_context(|| format!("reading {s}"))?;
        let cpp = lang == Lang::Cpp || is_cpp_file(s);
        let funcs = parse(&src);
        let text = blank(&src);
        let file = util::forward_slashes(path.strip_prefix(&ctx.root).unwrap_or(&path));
        let module = file
            .rsplit_once('.')
            .map(|(m, _)| m)
            .unwrap_or(&file)
            .replace('/', ".");
        for f in &funcs {
            let body = &text[f.body_open + 1..f.body_close];
            let (blockers, warnings, calls) = blockers(f, body, cpp, &funcs);
            // the declaration starts at the line of its return type
            let decl_start = src[..f.body_open]
                .rfind(
                    &f.ret
                        .split_whitespace()
                        .next()
                        .unwrap_or(&f.name)
                        .to_string(),
                )
                .unwrap_or(f.body_open);
            let line = line_of(&src, decl_start.min(f.body_open));
            let mut qual: Vec<String> = f
                .namespaces
                .iter()
                .filter(|n| !n.is_empty())
                .cloned()
                .collect();
            qual.extend(f.class.clone());
            qual.push(f.name.clone());
            let hs = Hotspot {
                file: file.clone(),
                line,
                end_line: line_of(&src, f.body_close),
                name: f.name.clone(),
                qualname: qual.join("."),
                module: Some(module.clone()),
                kind: "function".into(),
                portability: if blockers.is_empty() { 0.9 } else { 0.0 },
                blockers,
                warnings,
                params: f.params.clone(),
                returns: Some(f.ret.clone()),
                source: src[src[..f.body_open]
                    .rfind(['\n', ';', '}'])
                    .map(|i| i + 1)
                    .unwrap_or(0)..=f.body_close]
                    .trim()
                    .replace('\r', ""),
                calls,
                owner: f.class.clone(),
                ..Hotspot::default()
            };
            out.push(Described {
                path: path.clone(),
                f: f.clone(),
                hs,
            });
        }
    }
    Ok(out)
}

fn insert_at(text: &str, mut at: Vec<(usize, String)>) -> String {
    at.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut out = text.to_string();
    for (offset, piece) in at {
        out.insert_str(offset, &piece);
    }
    out
}

/// The build with one runtime source added, written in the language the compiler builds.
fn runtime_source(dir: &Path, name: &str, text: &str, lang: Lang) -> Result<PathBuf> {
    let path = dir.join(format!(
        "{name}.{}",
        if lang == Lang::Cpp { "cpp" } else { "c" }
    ));
    fs::write(&path, text)?;
    Ok(path)
}

pub fn profile(ctx: &Ctx, cc: &str, args: &[String], lang: Lang) -> Result<Profile> {
    let b = Build::parse(args)?;
    let found = describe(ctx, &b, lang)?;
    let dir = runtime_dir(ctx)?;
    let prof_c = runtime_source(&dir, "fa_prof", PROF_C, lang)?;
    let mut by_file: BTreeMap<&Path, Vec<(usize, String)>> = BTreeMap::new();
    for (i, d) in found.iter().enumerate() {
        by_file
            .entry(&d.path)
            .or_default()
            .push((d.f.body_open + 1, format!(" FA_PROF({i});")));
    }
    let mut patches = Patches::new(ctx)?;
    for (path, at) in by_file {
        let text = fs::read_to_string(path)?;
        patches.write(path, &insert_at(&text, at))?;
    }
    let bin = compile(ctx, cc, &b, &[prof_c], Some(&dir.join("fa_prof.h")));
    patches.restore()?;
    let bin = bin?;
    let out = ctx.fast.join("profile.json");
    let _ = fs::remove_file(&out);
    let ran = run(ctx, &bin, &b, &[("FAST_ANYTHING_PROF_OUT", &out)], true)?;
    ensure!(
        ran.status.success(),
        "the app failed ({}):\n{}",
        ran.status,
        tail(&combined(&ran), 30)
    );
    let result: Value = serde_json::from_str(
        &fs::read_to_string(&out)
            .context("the profiler wrote nothing: does main return normally?")?,
    )?;
    let wall = result["wall"].as_f64().unwrap_or(0.0);
    let stats = result["stats"].as_object().cloned().unwrap_or_default();
    let stat = |i: usize| stats.get(&i.to_string()).and_then(Value::as_array).cloned();
    let mut functions = Vec::new();
    for (i, d) in found.iter().enumerate() {
        let helper = found
            .iter()
            .enumerate()
            .any(|(j, g)| stat(j).is_some() && g.path == d.path && g.hs.calls.contains(&d.f.name));
        let mut hs = d.hs.clone();
        match stat(i) {
            Some(s) => {
                hs.ncalls = s[0].as_u64().unwrap_or(0);
                hs.tottime = s[1].as_f64().unwrap_or(0.0);
                hs.cumtime = s[2].as_f64().unwrap_or(0.0);
                functions.push(hs);
            }
            None if helper => functions.push(hs),
            None => {}
        }
    }
    Ok(Profile {
        wall,
        base: ".".into(),
        functions,
    })
}

// ------------------------------------------------------------------ recording

fn arg_names(hs: &Hotspot) -> Vec<String> {
    hs.params.iter().map(|p| p.name.clone()).collect()
}

fn c_put(
    kind: &CKind,
    name: &str,
    params: &[Param],
    sub: Option<(usize, &str)>,
    idx: usize,
) -> String {
    let value = |i: usize, n: &str| match sub {
        Some((k, v)) if k == i => v.to_string(),
        _ => n.to_string(),
    };
    let v = value(idx, name);
    match kind {
        CKind::Scalar(Scalar::Int { signed: true, .. }) | CKind::Len => {
            format!("fa_rec_i64((long long) ({v}));")
        }
        CKind::Scalar(Scalar::Int { signed: false, .. }) => {
            format!("fa_rec_u64((unsigned long long) ({v}));")
        }
        CKind::Scalar(Scalar::Float(_)) => format!("fa_rec_f64((double) ({v}));"),
        CKind::Scalar(Scalar::Bool) => format!("fa_rec_bool((int) ({v}));"),
        CKind::Str => format!("fa_rec_str({v});"),
        CKind::Array(s, len) => {
            let n = value(*len, &params[*len].name);
            match s {
                Scalar::Float(_) => {
                    format!("fa_rec_arr_f64({v}, (size_t) ({n}), (int) sizeof(*({v})));")
                }
                Scalar::Int { signed, .. } => {
                    format!(
                        "fa_rec_arr_i64({v}, (size_t) ({n}), (int) sizeof(*({v})), {});",
                        i32::from(*signed)
                    )
                }
                Scalar::Bool => {
                    format!("fa_rec_arr_i64({v}, (size_t) ({n}), (int) sizeof(*({v})), 0);")
                }
            }
        }
    }
}

fn c_suffix(ret: &str) -> &'static str {
    match scalar(ret) {
        Some(Scalar::Int { signed: false, .. }) => "u64",
        Some(Scalar::Int { .. }) => "i64",
        Some(Scalar::Float(_)) => "f64",
        _ => "bool",
    }
}

fn c_cast(ret: &str) -> &'static str {
    match c_suffix(ret) {
        "u64" => "(unsigned long long)",
        "i64" => "(long long)",
        "f64" => "(double)",
        _ => "(int)",
    }
}

fn c_record_prologue(hs: &Hotspot) -> Result<String> {
    let kinds = c_kinds(&hs.params).map_err(anyhow::Error::msg)?;
    let ret = hs.returns.clone().unwrap_or_default();
    let names = arg_names(hs);
    let puts = |sub: Option<(usize, &str)>| -> String {
        kinds
            .iter()
            .enumerate()
            .filter(|(_, k)| **k != CKind::Len)
            .map(|(i, k)| c_put(k, &names[i], &hs.params, sub, i))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let call = |sub: &[(usize, String)]| -> String {
        let args: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                sub.iter()
                    .find(|(k, _)| *k == i)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| n.clone())
            })
            .collect();
        format!("{}({})", hs.name, args.join(", "))
    };
    let (sfx, cast) = (c_suffix(&ret), c_cast(&ret));
    let mut derive = String::new();
    for (i, k) in kinds.iter().enumerate() {
        let (len_expr, sub_val, len_sub, free) = match k {
            CKind::Str => (
                format!("fa_rec_chars({})", names[i]),
                "fa_s".to_string(),
                None,
                "fa_rec_free(fa_s);",
            ),
            CKind::Array(_, len) => (
                format!("(size_t) ({})", names[*len]),
                format!("{} + fa_off", names[i]),
                Some(*len),
                "",
            ),
            _ => continue,
        };
        let mut subs = vec![(i, sub_val.clone())];
        if let Some(l) = len_sub {
            let lt = hs.params[l]
                .annotation
                .clone()
                .unwrap_or_else(|| "int".into());
            subs.push((l, format!("({lt}) fa_n")));
        }
        let prep = if matches!(k, CKind::Str) {
            format!("char *fa_s = fa_rec_substr({}, fa_off, fa_n); ", names[i])
        } else {
            String::new()
        };
        // fa_rec_begin + puts with the substitutions (puts takes one substitution: arrays put data + len themselves)
        let put_sub = if matches!(k, CKind::Str) {
            "fa_s".to_string()
        } else {
            sub_val.clone()
        };
        let mut put_line = String::new();
        for (j, kk) in kinds.iter().enumerate() {
            if *kk == CKind::Len {
                continue;
            }
            let p = match kk {
                CKind::Array(s, len) if j == i => {
                    let n = "fa_n";
                    match s {
                        Scalar::Float(_) => {
                            format!("fa_rec_arr_f64({put_sub}, (size_t) {n}, (int) sizeof(*({put_sub})));")
                        }
                        Scalar::Int { signed, .. } => format!(
                            "fa_rec_arr_i64({put_sub}, (size_t) {n}, (int) sizeof(*({put_sub})), {});",
                            i32::from(*signed)
                        ),
                        Scalar::Bool => {
                            format!("fa_rec_arr_i64({put_sub}, (size_t) {n}, (int) sizeof(*({put_sub})), 0);")
                        }
                    }
                    .replace("{len}", &len.to_string())
                }
                _ if j == i => c_put(kk, &names[j], &hs.params, Some((j, &put_sub)), j),
                _ => c_put(kk, &names[j], &hs.params, None, j),
            };
            put_line.push_str(&p);
            put_line.push(' ');
        }
        derive.push_str(&format!(
            "    for (int fa_k = 0; fa_k < 10 && fa_rec_derive_ok(); fa_k++) {{\n        size_t fa_len = {len_expr}, fa_n = (size_t) fa_rec_slices[fa_k];\n        if (fa_len < 2 || fa_n >= fa_len) break;\n        for (int fa_rep = 0; fa_rep < (fa_n < 2 ? 1 : 3); fa_rep++) {{\n            size_t fa_off = fa_rec_rand(fa_len - fa_n + 1);\n            {prep}fa_rec_begin(); {put_line}\n            fa_rec_derived_{sfx}({cast} {call});\n            {free}\n        }}\n    }}\n",
            call = call(&subs),
        ));
    }
    Ok(format!(
        "if (!fa_rec_inside) {{\n    fa_rec_inside = 1;\n    fa_rec_begin(); {puts}\n    long long fa_t0 = fa_rec_now();\n    {ret} fa_r = {call};\n    fa_rec_done_{sfx}({cast} fa_r, fa_rec_now() - fa_t0);\n{derive}    fa_rec_inside = 0;\n    return fa_r;\n}}",
        puts = puts(None),
        call = call(&[]),
    ))
}

fn cpp_record_prologue(hs: &Hotspot) -> String {
    let names = arg_names(hs);
    format!(
        "if (!fa_rec::st().inside) return fa_rec::call([](const auto &...fa_a) {{ return {}(fa_a...); }}{}{});",
        hs.name,
        if names.is_empty() { "" } else { ", " },
        names.join(", ")
    )
}

pub fn record(
    ctx: &Ctx,
    cc: &str,
    args: &[String],
    hs: &Hotspot,
    out: &Path,
    lang: Lang,
) -> Result<Recording> {
    let b = Build::parse(args)?;
    let found = describe(ctx, &b, lang)?;
    let d = found
        .iter()
        .filter(|d| d.hs.file == hs.file && d.hs.qualname == hs.qualname)
        .min_by_key(|d| (d.hs.line as i64 - hs.line as i64).abs())
        .with_context(|| format!("{} isn't in {} any more: scan again", hs.qualname, hs.file))?;
    let cpp = lang == Lang::Cpp || is_cpp_file(&hs.file);
    let dir = runtime_dir(ctx)?;
    let (prologue, header, extra) = if cpp {
        (cpp_record_prologue(hs), dir.join("fa_rec.hpp"), vec![])
    } else {
        (
            c_record_prologue(hs)?,
            dir.join("fa_rec.h"),
            vec![runtime_source(&dir, "fa_rec", REC_C, lang)?],
        )
    };
    let text = fs::read_to_string(&d.path)?;
    let meta = out.with_extension("meta.json");
    let mut patches = Patches::new(ctx)?;
    patches.write(
        &d.path,
        &insert_at(
            &text,
            vec![(
                d.f.body_open + 1,
                format!(" {}", prologue.replace('\n', " ")),
            )],
        ),
    )?;
    let bin = compile(ctx, cc, &b, &extra, Some(&header));
    patches.restore()?;
    let bin = bin?;
    let env = [
        ("FAST_ANYTHING_RECORD_OUT", out),
        ("FAST_ANYTHING_RECORD_META", meta.as_path()),
    ];
    let _ = fs::remove_file(&meta);
    let mut ran = run(ctx, &bin, &b, &env, true)?;
    if !ran.status.success() {
        // A shrunk input crashed the original (C has no exceptions): record without them.
        crate::ui::warn("the app crashed on a shrunk input; recording real calls only");
        ran = run(ctx, &bin, &b, &env, false)?;
    }
    ensure!(
        ran.status.success(),
        "the app failed while recording ({}):\n{}",
        ran.status,
        tail(&combined(&ran), 30)
    );
    serde_json::from_str(
        &fs::read_to_string(&meta).context("nothing was recorded: does main return normally?")?,
    )
    .context("reading the recording summary")
}

// ------------------------------------------------------------------ the crate

pub const FLAVOR_C: Flavor = Flavor {
    lang: Lang::C,
    fence: "c",
    rust_type: rust_type_c,
    default_json: |_| None,
    rules: &[
        "C `int` is `i32`, `long` is `i64` here; unsigned types wrap. Use `wrapping_*` where the original can overflow.",
        "A `const T *` followed by a length arrives as a Vec, and the length parameter still arrives on its own.",
        "`const char *` arrives as a UTF-8 String: `strlen` and `s[i]` are bytes, use `as_bytes()` for the same behavior.",
        "Floats: compilers may fuse `a * b + c` into one FMA instruction (clang does on ARM), which rounds differently. If recorded results differ in the last digits, the original was built that way: use `f64::mul_add` there, or build the app with `-ffp-contract=off`.",
        "Returns are numbers or bools. Undefined behavior in the original (overflow of signed ints, out-of-bounds reads) isn't a spec: match the defined cases.",
    ],
};

pub const FLAVOR_CPP: Flavor = Flavor {
    lang: Lang::Cpp,
    fence: "cpp",
    rust_type: cpp_rust_type,
    default_json: |_| None,
    rules: &[
        "`int` is `i32`, `long long`/`int64_t` is `i64`, `size_t` is `u64`; unsigned types wrap. Use `wrapping_*` where the original can overflow.",
        "`std::string` arrives as a UTF-8 String: `size()` and `s[i]` are bytes, use `as_bytes()` for the same behavior.",
        "`std::vector` is Vec, `std::array<T, N>` is `[T; N]`, `std::map<std::string, T>` is BTreeMap (same key order), `std::pair` is a tuple.",
        "Return `Err(message)` where the original throws.",
    ],
};

pub fn scaffold(ctx: &Ctx, hs: &Hotspot, helpers: &[Hotspot], port: &Port) -> Result<String> {
    native::scaffold(
        ctx,
        hs,
        helpers,
        port,
        if port.lang == Lang::Cpp {
            &FLAVOR_CPP
        } else {
            &FLAVOR_C
        },
    )
}

pub fn verify(ctx: &Ctx, port: &Port) -> Result<Verify> {
    native::verify(ctx, port, false)
}

// ------------------------------------------------------------------ the bridge

fn suffix(port: &Port) -> String {
    util::slug(&port.id)
}

fn c_string_literal(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn param_list(hs: &Hotspot) -> String {
    hs.params
        .iter()
        .map(|p| format!("{} {}", p.annotation.as_deref().unwrap_or("int"), p.name))
        .collect::<Vec<_>>()
        .join(", ")
}

const LOADER: &str = "#ifdef _WIN32\n#ifndef WIN32_LEAN_AND_MEAN\n#define WIN32_LEAN_AND_MEAN\n#endif\n#ifndef NOMINMAX\n#define NOMINMAX\n#endif\n#include <windows.h>\n#define FA_OPEN(p) ((void *) LoadLibraryA(p))\n#define FA_SYM(l, n) ((void *) GetProcAddress((HMODULE) (l), n))\n#define FA_STATE_T volatile LONG\n#define FA_LOAD_ACQ(p) InterlockedCompareExchange((volatile LONG *)(p), 0, 0)\n#define FA_CAS(p, expected, desired) (InterlockedCompareExchange((volatile LONG *)(p), (desired), (expected)) == (expected))\n#define FA_STORE_REL(p, v) InterlockedExchange((volatile LONG *)(p), (v))\n#define FA_YIELD() SwitchToThread()\n#else\n#include <dlfcn.h>\n#ifdef RTLD_NODELETE\n#define FA_RTLD_NODELETE RTLD_NODELETE\n#else\n#define FA_RTLD_NODELETE 0\n#endif\n#define FA_OPEN(p) dlopen(p, RTLD_NOW | RTLD_LOCAL | FA_RTLD_NODELETE)\n#define FA_SYM(l, n) dlsym(l, n)\n#define FA_STATE_T volatile int\n#define FA_LOAD_ACQ(p) __atomic_load_n((p), __ATOMIC_ACQUIRE)\n#define FA_CAS(p, expected, desired) __sync_bool_compare_and_swap((p), (expected), (desired))\n#define FA_STORE_REL(p, v) __atomic_store_n((p), (v), __ATOMIC_RELEASE)\n#define FA_YIELD() ((void)0)\n#endif";

fn lib_open_fn_c(s: &str, lib_name: &str, file_rel: &str) -> String {
    let name_lit = c_string_literal(lib_name);
    let root_rel = c_string_literal(&format!(".fast/lib/{lib_name}"));
    let file_rel_lit = c_string_literal(file_rel);
    let parent_rel = c_string_literal(&format!("../.fast/lib/{lib_name}"));
    format!(
        r#"static void *fa_open_{s}(void) {{
    const char *dir = getenv("FNR_LIB_DIR");
    if (dir && dir[0]) {{
        const char *name = {name_lit};
        size_t dlen = strlen(dir), nlen = strlen(name);
        if (dlen + 1 + nlen < 4096) {{
            char buf[4096];
            memcpy(buf, dir, dlen);
            buf[dlen] = '/';
            memcpy(buf + dlen + 1, name, nlen + 1);
            void *h = FA_OPEN(buf);
            if (h) return h;
        }}
    }}
    {{
        const char *cands[] = {{ {root_rel}, {file_rel_lit}, {parent_rel} }};
        size_t i;
        for (i = 0; i < sizeof(cands) / sizeof(cands[0]); i++) {{
            void *h = FA_OPEN(cands[i]);
            if (h) return h;
        }}
    }}
    return NULL;
}}"#
    )
}

fn lib_open_fn_cpp(lib_name: &str, file_rel: &str) -> String {
    let name_lit = c_string_literal(lib_name);
    let root_rel = c_string_literal(&format!(".fast/lib/{lib_name}"));
    let file_rel_lit = c_string_literal(file_rel);
    let parent_rel = c_string_literal(&format!("../.fast/lib/{lib_name}"));
    format!(
        r#"static void *open_lib() {{
    const char *dir = std::getenv("FNR_LIB_DIR");
    if (dir && dir[0]) {{
        const char *name = {name_lit};
        std::size_t dlen = std::strlen(dir), nlen = std::strlen(name);
        if (dlen + 1 + nlen < 4096) {{
            char buf[4096];
            std::memcpy(buf, dir, dlen);
            buf[dlen] = '/';
            std::memcpy(buf + dlen + 1, name, nlen + 1);
            void *h = FA_OPEN(buf);
            if (h) return h;
        }}
    }}
    const char *cands[] = {{ {root_rel}, {file_rel_lit}, {parent_rel} }};
    for (const char *cand : cands) {{
        void *h = FA_OPEN(cand);
        if (h) return h;
    }}
    return nullptr;
}}"#
    )
}

fn replace_slug_suffix(src: &str, s: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len() + s.len() * 8);
    let mut i = 0;
    while i < bytes.len() {
        if i + 2 <= bytes.len()
            && &bytes[i..i + 2] == b"_S"
            && !bytes
                .get(i + 2)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            out.push('_');
            out.push_str(s);
            i += 2;
        } else {
            let ch = src[i..].chars().next().expect("valid utf8");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn c_bridge(hs: &Hotspot, s: &str, lib_name: &str, file_rel: &str) -> Result<String> {
    let kinds = c_kinds(&hs.params).map_err(anyhow::Error::msg)?;
    let ret = hs.returns.clone().unwrap_or_default();
    let lib_open = lib_open_fn_c(s, lib_name, file_rel);
    let lib_name_lit = c_string_literal(lib_name);
    if let Some(sig) = native::DirectSig::from_hotspot(hs, &FLAVOR_C) {
        let abi_ret = sig.ret.c_ty();
        let mut ptr_params = Vec::new();
        let mut guards = String::new();
        let mut call_args = Vec::new();
        let mut can_direct = true;
        for (i, (dp, ck)) in sig.params.iter().zip(kinds.iter()).enumerate() {
            let n = &hs.params[i].name;
            match (dp, ck) {
                (native::DirectParam::Scalar { ty, .. }, _) => {
                    ptr_params.push(ty.c_ty().to_string());
                    if *ty == native::CAbiScalar::Bool {
                        call_args.push(format!("(uint8_t) (({n}) ? 1 : 0)"));
                    } else {
                        call_args.push(format!("({}) ({n})", ty.c_ty()));
                    }
                }
                (native::DirectParam::Str { .. }, _) => {
                    ptr_params.push("const uint8_t *".to_string());
                    ptr_params.push("size_t".to_string());
                    guards.push_str(&format!("    if (!({n})) return 0;\n"));
                    call_args.push(format!("(const uint8_t *) ({n})"));
                    call_args.push(format!("strlen({n})"));
                }
                (native::DirectParam::Slice { elem, .. }, CKind::Array(_, len_idx)) => {
                    let l = &hs.params[*len_idx].name;
                    ptr_params.push(format!("const {} *", elem.c_ty()));
                    ptr_params.push("size_t".to_string());
                    guards.push_str(&format!(
                        "    if (({l}) < 0 || (!({n}) && ({l}) > 0)) return 0;\n"
                    ));
                    call_args.push(format!("(const {} *) ({n})", elem.c_ty()));
                    call_args.push(format!("(size_t) ({l})"));
                }
                _ => {
                    can_direct = false;
                    break;
                }
            }
        }
        if can_direct {
            ptr_params.push("int32_t *".to_string());
            call_args.push("&fa_d_ok".to_string());
            let cast_ret = if sig.ret == native::CAbiScalar::Bool {
                format!("({ret}) (fa_res != 0)")
            } else {
                format!("({ret}) fa_res")
            };
            let body = format!(
                r#"/* Calls the verified Rust port of {id} via direct binary C ABI. FNR_DISABLE=1 (or FAST_ANYTHING_DISABLE=1) runs the C version instead. */
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
{loader}
{lib_open}
static {abi_ret} (*fa_direct_S)({ptr_params});
int fa_on_S(void) {{
    static FA_STATE_T state = 0;
    int s = (int) FA_LOAD_ACQ(&state);
    if (s == 3) return 1;
    if (s == 2) return 0;
    if (FA_CAS(&state, 0, 1)) {{
        int next = 2;
        if (!(getenv("FNR_DISABLE") || getenv("FAST_ANYTHING_DISABLE"))) {{
            void *lib = fa_open_S();
            if (lib) {{
                *(void **) &fa_direct_S = FA_SYM(lib, "fa_direct");
                if (fa_direct_S != NULL) next = 3;
            }}
            if (next != 3) {{
                const char *strict = getenv("FNR_STRICT");
                if (strict && (strcmp(strict, "1") == 0 || strcmp(strict, "true") == 0)) {{
                    fprintf(stderr, "fnr: FNR_STRICT=1 and Rust port %s (%s) could not be loaded\n", "{id}", {lib_name_lit});
                    abort();
                }}
            }}
        }}
        FA_STORE_REL(&state, next);
        return next == 3;
    }}
    while ((s = (int) FA_LOAD_ACQ(&state)) == 1) {{
        FA_YIELD();
    }}
    return s == 3;
}}
/* Sets *fa_ok to 0 (and the C version runs) if the port can't answer. */
{ret} fa_fast_S(int *fa_ok, {params}) {{
    int32_t fa_d_ok = 0;
    {abi_ret} fa_res;
    *fa_ok = 0;
{guards}    fa_res = fa_direct_S({call_args});
    if (fa_d_ok) {{
        *fa_ok = 1;
        return {cast_ret};
    }}
    return 0;
}}"#,
                id = hs.id,
                loader = LOADER,
                ptr_params = ptr_params.join(", "),
                params = param_list(hs),
                call_args = call_args.join(", "),
            );
            return Ok(replace_slug_suffix(&body, s));
        }
    }

    let mut puts = String::new();
    let mut first = true;
    for (i, k) in kinds.iter().enumerate() {
        if *k == CKind::Len && i > 0 && matches!(kinds[i - 1], CKind::Array(..)) && false {
            continue;
        }
        let n = &hs.params[i].name;
        if !first {
            puts.push_str("    fa_put_S(&b, \",\");\n");
        }
        first = false;
        puts.push_str(&match k {
            CKind::Scalar(Scalar::Int { signed: true, .. }) | CKind::Len => format!("    fa_int_S(&b, (long long) ({n}));\n"),
            CKind::Scalar(Scalar::Int { signed: false, .. }) => format!("    fa_uint_S(&b, (unsigned long long) ({n}));\n"),
            CKind::Scalar(Scalar::Float(_)) => format!("    if (!fa_dbl_S(&b, (double) ({n}))) goto fallback;\n"),
            CKind::Scalar(Scalar::Bool) => format!("    fa_put_S(&b, ({n}) ? \"true\" : \"false\");\n"),
            CKind::Str => format!("    if (!fa_str_S(&b, {n})) goto fallback;\n"),
            CKind::Array(sc, len) => {
                let l = &hs.params[*len].name;
                let item = match sc {
                    Scalar::Float(_) => format!("if (!fa_dbl_S(&b, (double) {n}[fa_i])) goto fallback;"),
                    Scalar::Int { signed: true, .. } => format!("fa_int_S(&b, (long long) {n}[fa_i]);"),
                    Scalar::Int { signed: false, .. } => format!("fa_uint_S(&b, (unsigned long long) {n}[fa_i]);"),
                    Scalar::Bool => format!("fa_put_S(&b, {n}[fa_i] ? \"true\" : \"false\");"),
                };
                format!("    fa_put_S(&b, \"[\");\n    for (size_t fa_i = 0; fa_i < (size_t) ({l}); fa_i++) {{ if (fa_i) fa_put_S(&b, \",\"); {item} }}\n    fa_put_S(&b, \"]\");\n")
            }
        });
    }
    let parse = match scalar(&ret) {
        Some(Scalar::Bool) => "strncmp(ok, \"true\", 4) == 0".to_string(),
        Some(Scalar::Float(_)) => format!("({ret}) strtod(ok, NULL)"),
        Some(Scalar::Int { signed: false, .. }) => format!("({ret}) strtoull(ok, NULL, 10)"),
        _ => format!("({ret}) strtoll(ok, NULL, 10)"),
    };
    let body = format!(
        r#"/* Calls the verified Rust port of {id}. FNR_DISABLE=1 (or FAST_ANYTHING_DISABLE=1) runs the C version instead. */
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
{loader}
{lib_open}
typedef struct {{ char *p; size_t len, cap; }} fa_buf_S;
static unsigned char *(*fa_call_S)(const unsigned char *, size_t, size_t *);
static void (*fa_free_S)(unsigned char *, size_t);
int fa_on_S(void) {{
    static FA_STATE_T state = 0;
    int s = (int) FA_LOAD_ACQ(&state);
    if (s == 3) return 1;
    if (s == 2) return 0;
    if (FA_CAS(&state, 0, 1)) {{
        int next = 2;
        if (!(getenv("FNR_DISABLE") || getenv("FAST_ANYTHING_DISABLE"))) {{
            void *lib = fa_open_S();
            if (lib) {{
                *(void **) &fa_call_S = FA_SYM(lib, "fa_call");
                *(void **) &fa_free_S = FA_SYM(lib, "fa_free");
                if (fa_call_S && fa_free_S) next = 3;
            }}
            if (next != 3) {{
                const char *strict = getenv("FNR_STRICT");
                if (strict && (strcmp(strict, "1") == 0 || strcmp(strict, "true") == 0)) {{
                    fprintf(stderr, "fnr: FNR_STRICT=1 and Rust port %s (%s) could not be loaded\n", "{id}", {lib_name_lit});
                    abort();
                }}
            }}
        }}
        FA_STORE_REL(&state, next);
        return next == 3;
    }}
    while ((s = (int) FA_LOAD_ACQ(&state)) == 1) {{
        FA_YIELD();
    }}
    return s == 3;
}}
static void fa_put_S(fa_buf_S *b, const char *s) {{
    size_t n = strlen(s);
    if (b->len + n + 1 > b->cap) {{ b->cap = (b->len + n + 1) * 2; b->p = (char *) realloc(b->p, b->cap); }}
    memcpy(b->p + b->len, s, n + 1);
    b->len += n;
}}
static void fa_int_S(fa_buf_S *b, long long v) {{ char t[32]; snprintf(t, sizeof t, "%lld", v); fa_put_S(b, t); }}
static void fa_uint_S(fa_buf_S *b, unsigned long long v) {{ char t[32]; snprintf(t, sizeof t, "%llu", v); fa_put_S(b, t); }}
static int fa_dbl_S(fa_buf_S *b, double v) {{
    char t[40];
    if (!isfinite(v)) return 0;
    snprintf(t, sizeof t, "%.17g", v);
    fa_put_S(b, t);
    return 1;
}}
static int fa_str_S(fa_buf_S *b, const char *s) {{
    char t[8] = {{0}};
    if (!s) {{ fa_put_S(b, "null"); return 1; }}
    fa_put_S(b, "\"");
    for (; *s; s++) {{
        unsigned char c = (unsigned char) *s;
        if (c == '"' || c == '\\') {{ t[0] = '\\'; t[1] = (char) c; t[2] = 0; }}
        else if (c < 0x20) snprintf(t, sizeof t, "\\u%04x", c);
        else {{ t[0] = (char) c; t[1] = 0; }}
        fa_put_S(b, t);
    }}
    fa_put_S(b, "\"");
    return 1;
}}
/* Sets *fa_ok to 0 (and the C version runs) if the port can't answer. */
{ret} fa_fast_S(int *fa_ok, {params}) {{
    fa_buf_S b = {{0}};
    size_t n = 0;
    unsigned char *out;
    char *text;
    const char *ok;
    {ret} r = 0;
    *fa_ok = 0;
    fa_put_S(&b, "[");
{puts}    fa_put_S(&b, "]");
    out = fa_call_S((const unsigned char *) b.p, b.len, &n);
    text = (char *) malloc(n + 1);
    memcpy(text, out, n);
    text[n] = 0;
    fa_free_S(out, n);
    ok = strstr(text, "\"ok\":");
    if (ok) {{
        ok += 5;
        r = {parse};
        *fa_ok = 1;
    }}
    free(text);
fallback:
    free(b.p);
    return r;
}}"#,
        id = hs.id,
        loader = LOADER,
        params = param_list(hs),
    );
    Ok(replace_slug_suffix(&body, s))
}

fn cpp_bridge(
    hs: &Hotspot,
    s: &str,
    lib_name: &str,
    file_rel: &str,
    namespaces: &[String],
) -> String {
    let ret = hs.returns.clone().unwrap_or_default();
    let lib_open = lib_open_fn_cpp(lib_name, file_rel);
    let lib_name_lit = c_string_literal(lib_name);
    let open: String = namespaces
        .iter()
        .map(|n| {
            if n.is_empty() {
                "namespace {\n".to_string()
            } else {
                format!("namespace {n} {{\n")
            }
        })
        .collect();
    let close: String = namespaces.iter().map(|_| "}\n").collect();
    let has_vec_bool = hs.params.iter().any(|p| {
        p.annotation
            .as_deref()
            .is_some_and(|a| squash(a).contains("vector<bool>"))
    });
    if !has_vec_bool && let Some(sig) = native::DirectSig::from_hotspot(hs, &FLAVOR_CPP) {
        let abi_ret = sig.ret.c_ty();
        let mut ptr_params = Vec::new();
        let mut guards = String::new();
        let mut call_args = Vec::new();
        for (i, dp) in sig.params.iter().enumerate() {
            let n = &hs.params[i].name;
            let ann = hs.params[i].annotation.as_deref().unwrap_or("");
            match dp {
                native::DirectParam::Scalar { ty, .. } => {
                    ptr_params.push(ty.c_ty().to_string());
                    if *ty == native::CAbiScalar::Bool {
                        call_args.push(format!("static_cast<uint8_t>(({n}) ? 1 : 0)"));
                    } else {
                        call_args.push(format!("static_cast<{}>({n})", ty.c_ty()));
                    }
                }
                native::DirectParam::Str { .. } => {
                    ptr_params.push("const uint8_t *".to_string());
                    ptr_params.push("size_t".to_string());
                    if squash(ann).contains("char*") {
                        guards.push_str(&format!("    if (!({n})) return false;\n"));
                        call_args.push(format!("reinterpret_cast<const uint8_t *>({n})"));
                        call_args.push(format!("std::strlen({n})"));
                    } else {
                        call_args.push(format!("reinterpret_cast<const uint8_t *>({n}.data())"));
                        call_args.push(format!("{n}.size()"));
                    }
                }
                native::DirectParam::Slice { elem, .. } => {
                    ptr_params.push(format!("const {} *", elem.c_ty()));
                    ptr_params.push("size_t".to_string());
                    call_args.push(format!(
                        "reinterpret_cast<const {} *>({n}.data())",
                        elem.c_ty()
                    ));
                    call_args.push(format!("{n}.size()"));
                }
            }
        }
        ptr_params.push("int32_t *".to_string());
        call_args.push("&fa_d_ok".to_string());
        let assign_out = if sig.ret == native::CAbiScalar::Bool {
            format!("fa_out = static_cast<{ret}>(fa_res != 0);")
        } else {
            format!("fa_out = static_cast<{ret}>(fa_res);")
        };
        let body = format!(
            r#"// Calls the verified Rust port of {id} via direct binary C ABI. FNR_DISABLE=1 (or FAST_ANYTHING_DISABLE=1) runs the C++ version instead.
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>
{loader}
namespace fa_bridge_S {{
{lib_open}
struct Lib {{
    {abi_ret} (*direct)({ptr_params}) = nullptr;
    bool on = false;
    Lib() {{
        if (std::getenv("FNR_DISABLE") || std::getenv("FAST_ANYTHING_DISABLE")) return;
        void *lib = open_lib();
        if (lib) {{
            *(void **) &direct = FA_SYM(lib, "fa_direct");
            on = direct != nullptr;
        }}
        if (!on) {{
            const char *strict = std::getenv("FNR_STRICT");
            if (strict && (std::strcmp(strict, "1") == 0 || std::strcmp(strict, "true") == 0)) {{
                std::fprintf(stderr, "fnr: FNR_STRICT=1 and Rust port %s (%s) could not be loaded\n", "{id}", {lib_name_lit});
                std::abort();
            }}
        }}
    }}
}};
inline Lib &lib() {{ static Lib l; return l; }}
}} // namespace fa_bridge_S
{open}bool fa_on_S() {{ return fa_bridge_S::lib().on; }}
bool fa_fast_S({ret} &fa_out, {params}) {{
    int32_t fa_d_ok = 0;
{guards}    {abi_ret} fa_res = fa_bridge_S::lib().direct({call_args});
    if (!fa_d_ok) return false;
    {assign_out}
    return true;
}}
{close}"#,
            id = hs.id,
            loader = LOADER,
            ptr_params = ptr_params.join(", "),
            params = param_list(hs),
            call_args = call_args.join(", "),
        );
        return replace_slug_suffix(&body, s);
    }
    if native::BinSig::from_hotspot(hs, &FLAVOR_CPP) {
        let body = format!(
            r#"// Calls the verified Rust port of {id} via compact binary wire FFI (fa_bin). FNR_DISABLE=1 (or FAST_ANYTHING_DISABLE=1) runs the C++ version instead.
#include <algorithm>
#include <array>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <stdexcept>
#include <string>
#include <type_traits>
#include <unordered_map>
#include <utility>
#include <vector>
{loader}
namespace fa_bridge_S {{
{lib_open}
{bin_hpp}
struct Lib {{
    unsigned char *(*bin)(const unsigned char *, size_t, size_t *) = nullptr;
    void (*free)(unsigned char *, size_t) = nullptr;
    bool on = false;
    Lib() {{
        if (std::getenv("FNR_DISABLE") || std::getenv("FAST_ANYTHING_DISABLE")) return;
        void *lib = open_lib();
        if (lib) {{
            *(void **) &bin = FA_SYM(lib, "fa_bin");
            *(void **) &free = FA_SYM(lib, "fa_free");
            on = bin && free;
        }}
        if (!on) {{
            const char *strict = std::getenv("FNR_STRICT");
            if (strict && (std::strcmp(strict, "1") == 0 || std::strcmp(strict, "true") == 0)) {{
                std::fprintf(stderr, "fnr: FNR_STRICT=1 and Rust port %s (%s) could not be loaded\n", "{id}", {lib_name_lit});
                std::abort();
            }}
        }}
    }}
}};
inline Lib &lib() {{ static Lib l; return l; }}
}} // namespace fa_bridge_S
{open}bool fa_on_S() {{ return fa_bridge_S::lib().on; }}
bool fa_fast_S({ret} &fa_out, {params}) {{
    std::vector<uint8_t> in;
    in.reserve(256);
    if (!fa_bridge_S::bin_enc_args(in, {args})) return false;
    size_t n = 0;
    unsigned char *out = fa_bridge_S::lib().bin(in.data(), in.size(), &n);
    if (!out || n == 0) {{
        if (out) fa_bridge_S::lib().free(out, n);
        return false;
    }}
    if (out[0] == 0) {{
        uint32_t mlen = 0;
        if (n >= 5) std::memcpy(&mlen, out + 1, 4);
        std::string msg = (n >= (size_t) (5 + mlen))
            ? std::string((const char *) (out + 5), mlen)
            : std::string("binary call failed");
        fa_bridge_S::lib().free(out, n);
        throw std::runtime_error("fnr: " + msg);
    }}
    const uint8_t *p = out + 1;
    const uint8_t *e = out + n;
    bool ok = (out[0] == 1) && fa_bridge_S::bin_dec(p, e, fa_out);
    fa_bridge_S::lib().free(out, n);
    return ok;
}}
{close}"#,
            id = hs.id,
            loader = LOADER,
            bin_hpp = BIN_HPP,
            params = param_list(hs),
            args = arg_names(hs).join(", "),
        );
        return replace_slug_suffix(&body, s);
    }
    let body = format!(
        r#"// Calls the verified Rust port of {id}. FNR_DISABLE=1 (or FAST_ANYTHING_DISABLE=1) runs the C++ version instead.
#include <algorithm>
#include <array>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <stdexcept>
#include <string>
#include <type_traits>
#include <unordered_map>
#include <utility>
#include <vector>
{loader}
namespace fa_bridge_S {{
{lib_open}
{json}
struct Lib {{
    unsigned char *(*call)(const unsigned char *, size_t, size_t *) = nullptr;
    void (*free)(unsigned char *, size_t) = nullptr;
    bool on = false;
    Lib() {{
        if (std::getenv("FNR_DISABLE") || std::getenv("FAST_ANYTHING_DISABLE")) return;
        void *lib = open_lib();
        if (lib) {{
            *(void **) &call = FA_SYM(lib, "fa_call");
            *(void **) &free = FA_SYM(lib, "fa_free");
            on = call && free;
        }}
        if (!on) {{
            const char *strict = std::getenv("FNR_STRICT");
            if (strict && (std::strcmp(strict, "1") == 0 || std::strcmp(strict, "true") == 0)) {{
                std::fprintf(stderr, "fnr: FNR_STRICT=1 and Rust port %s (%s) could not be loaded\n", "{id}", {lib_name_lit});
                std::abort();
            }}
        }}
    }}
}};
inline Lib &lib() {{ static Lib l; return l; }}
}} // namespace fa_bridge_S
{open}bool fa_on_S() {{ return fa_bridge_S::lib().on; }}
bool fa_fast_S({ret} &fa_out, {params}) {{
    std::string in = "[";
    if (!fa_bridge_S::enc_all(in, {args})) return false;
    in += "]";
    size_t n = 0;
    unsigned char *out = fa_bridge_S::lib().call((const unsigned char *) in.data(), in.size(), &n);
    std::string text((const char *) out, n);
    fa_bridge_S::lib().free(out, n);
    const char *p = text.c_str();
    fa_bridge_S::V reply = fa_bridge_S::parse(p);
    if (const fa_bridge_S::V *err = reply.get("error")) throw std::runtime_error("fnr: " + err->s);
    const fa_bridge_S::V *ok = reply.get("ok");
    if (!ok) return false;
    fa_bridge_S::from(*ok, fa_out);
    return true;
}}
{close}"#,
        id = hs.id,
        loader = LOADER,
        json = JSON_HPP,
        params = param_list(hs),
        args = arg_names(hs).join(", "),
    );
    replace_slug_suffix(&body, s)
}

fn find_body(name: &str, hint: u32) -> impl Fn(&str) -> Option<usize> {
    let call = format!("{name}(");
    let spaced = format!("{name} (");
    move |text: &str| {
        native::body_line(text, hint, |l| {
            (l.contains(&call) || l.contains(&spaced)) && !l.trim_end().ends_with(';')
        })
    }
}

pub fn enable(ctx: &Ctx, port: &Port) -> Result<()> {
    let hs = ctx
        .state
        .hotspots
        .iter()
        .find(|h| h.id == port.id)
        .with_context(|| format!("{} isn't in the last scan", port.id))?;
    let file = ctx.root.join(&port.file);
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    let cpp = port.lang == Lang::Cpp || is_cpp_file(&port.file);
    let s = suffix(port);
    let (_, lib_name) = port.lang.artifact(&port.ext);
    let from = file.parent().context("source file has no folder")?;
    let file_rel = util::forward_slashes(&util::relative(from, &ctx.lib_dir().join(&lib_name)));
    let ret = hs.returns.clone().unwrap_or_default();
    let args = arg_names(hs).join(", ");
    let (prologue, bridge) = if cpp {
        let namespaces = parse(&text)
            .into_iter()
            .find(|f| f.name == hs.name && f.class == hs.owner)
            .map(|f| f.namespaces)
            .unwrap_or_default();
        (
            format!(
                "bool fa_on_{s}();\nbool fa_fast_{s}({ret} &, {params});\nif (fa_on_{s}()) {{\n    {ret} fa_r{{}};\n    if (fa_fast_{s}(fa_r, {args})) return fa_r;\n}}",
                params = param_list(hs)
            ),
            cpp_bridge(hs, &s, &lib_name, &file_rel, &namespaces),
        )
    } else {
        (
            format!(
                "int fa_on_{s}(void);\n{ret} fa_fast_{s}(int *, {params});\nif (fa_on_{s}()) {{\n    int fa_ok;\n    {ret} fa_r = fa_fast_{s}(&fa_ok, {args});\n    if (fa_ok) return fa_r;\n}}",
                params = param_list(hs)
            ),
            c_bridge(hs, &s, &lib_name, &file_rel)?,
        )
    };
    let patched = native::insert_in_body(
        &text,
        "//",
        &port.id,
        &prologue,
        find_body(&port.func, port.line.max(1)),
    )?;
    let patched = backend::append_marked(&patched, "//", &format!("{} bridge", port.id), &bridge);
    fs::write(&file, patched).with_context(|| format!("writing {}", port.file))
}

pub fn disable(ctx: &Ctx, port: &Port) -> Result<()> {
    let file = ctx.root.join(&port.file);
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", port.file))?;
    let text = native::strip_in_body(&text, "//", &port.id);
    let text = backend::strip_marked(&text, "//", &format!("{} bridge", port.id));
    fs::write(&file, text).with_context(|| format!("writing {}", port.file))
}

fn strip_cpp_ref_const(t: &str) -> (String, bool) {
    let sq = squash(t);
    if sq == "constchar*" || sq == "char*" {
        return ("std::string".to_string(), true);
    }
    let mut s = t.trim().to_string();
    while let Some(rest) = s.strip_suffix('&') {
        s = rest.trim().to_string();
    }
    if let Some(rest) = s.strip_prefix("const ") {
        s = rest.trim().to_string();
    }
    (s, false)
}

fn c_oracle_driver(hs: &Hotspot) -> Result<String> {
    let kinds = c_kinds(&hs.params).map_err(anyhow::Error::msg)?;
    let ret = hs.returns.as_deref().unwrap_or("int");
    let ret_sc = scalar(ret).context("non-scalar C return type")?;
    let mut decls = String::new();
    let mut frees = String::new();
    let mut call_args = Vec::new();
    let mut parse_cases = String::new();
    let mut arr_len_pairs: Vec<(usize, usize)> = Vec::new();
    for (i, k) in kinds.iter().enumerate() {
        match k {
            CKind::Scalar(Scalar::Bool) => {
                decls.push_str(&format!("        int a{i} = 0;\n"));
                call_args.push(format!("a{i}"));
                parse_cases.push_str(&format!(
                    "        fa_o_ws(&p); if (strncmp(p, \"true\", 4) == 0) {{ a{i} = 1; p += 4; }} else if (strncmp(p, \"false\", 5) == 0) {{ a{i} = 0; p += 5; }} else {{ a{i} = (int) strtoll(p, &endp, 10); p = endp; }}\n"
                ));
            }
            CKind::Scalar(Scalar::Int { signed: true, .. }) => {
                let cty = hs.params[i].annotation.as_deref().unwrap_or("long long");
                decls.push_str(&format!("        {cty} a{i} = 0;\n"));
                call_args.push(format!("a{i}"));
                parse_cases.push_str(&format!(
                    "        fa_o_ws(&p); a{i} = ({cty}) strtoll(p, &endp, 10); p = endp;\n"
                ));
            }
            CKind::Scalar(Scalar::Int { signed: false, .. }) => {
                let cty = hs.params[i]
                    .annotation
                    .as_deref()
                    .unwrap_or("unsigned long long");
                decls.push_str(&format!("        {cty} a{i} = 0;\n"));
                call_args.push(format!("a{i}"));
                parse_cases.push_str(&format!(
                    "        fa_o_ws(&p); a{i} = ({cty}) strtoull(p, &endp, 10); p = endp;\n"
                ));
            }
            CKind::Scalar(Scalar::Float(_)) => {
                let cty = hs.params[i].annotation.as_deref().unwrap_or("double");
                decls.push_str(&format!("        {cty} a{i} = 0;\n"));
                call_args.push(format!("a{i}"));
                parse_cases.push_str(&format!(
                    "        fa_o_ws(&p); a{i} = ({cty}) strtod(p, &endp); p = endp;\n"
                ));
            }
            CKind::Str => {
                decls.push_str(&format!("        char *a{i} = NULL;\n"));
                frees.push_str(&format!("        free(a{i});\n"));
                call_args.push(format!("a{i}"));
                parse_cases.push_str(&format!("        fa_o_ws(&p); a{i} = fa_o_str(&p);\n"));
            }
            CKind::Len => {
                let cty = hs.params[i].annotation.as_deref().unwrap_or("size_t");
                decls.push_str(&format!("        {cty} a{i} = 0;\n"));
                call_args.push(format!("a{i}"));
                parse_cases.push_str(&format!(
                    "        fa_o_ws(&p); {{ long long lv = strtoll(p, &endp, 10); p = endp; if (fa_arr_seen_{i}) a{i} = ({cty}) fa_arr_len_{i}; else a{i} = ({cty}) lv; }}\n"
                ));
            }
            CKind::Array(sc, len_idx) => {
                arr_len_pairs.push((i, *len_idx));
                let elem_c = match sc {
                    Scalar::Float("f32") => "float",
                    Scalar::Float(_) => "double",
                    Scalar::Int { signed: true, .. } => "int",
                    Scalar::Int { signed: false, .. } => "unsigned int",
                    Scalar::Bool => "uint8_t",
                };
                decls.push_str(&format!(
                    "        {elem_c} *a{i} = NULL; size_t fa_arr_len_{len_idx} = 0; int fa_arr_seen_{len_idx} = 0;\n"
                ));
                frees.push_str(&format!("        free(a{i});\n"));
                call_args.push(format!("a{i}"));
                let parse_item = match sc {
                    Scalar::Float(_) => format!("({elem_c}) strtod(p, &endp); p = endp"),
                    Scalar::Int { signed: false, .. } => {
                        format!("({elem_c}) strtoull(p, &endp, 10); p = endp")
                    }
                    _ => format!("({elem_c}) strtoll(p, &endp, 10); p = endp"),
                };
                parse_cases.push_str(&format!(
                    "        fa_o_ws(&p); if (*p == '[') {{ p++; size_t cap = 16; a{i} = ({elem_c}*) malloc(cap * sizeof({elem_c})); fa_arr_len_{len_idx} = 0; fa_o_ws(&p); while (*p && *p != ']') {{ if (fa_arr_len_{len_idx} == cap) {{ cap *= 2; a{i} = ({elem_c}*) realloc(a{i}, cap * sizeof({elem_c})); }} a{i}[fa_arr_len_{len_idx}++] = {parse_item}; fa_o_ws(&p); if (*p == ',') {{ p++; fa_o_ws(&p); }} }} if (*p == ']') p++; fa_arr_seen_{len_idx} = 1; }} else if (strncmp(p, \"null\", 4) == 0) {{ p += 4; }}\n"
                ));
            }
        }
        if i + 1 < kinds.len() {
            parse_cases.push_str("        fa_o_ws(&p); if (*p == ',') p++;\n");
        }
    }
    let print_res = match ret_sc {
        Scalar::Bool => format!(
            "        int r = (int) ({}({}));\n        fprintf(fout, \"{{\\\"ok\\\":%s}}\\n\", r ? \"true\" : \"false\");",
            hs.name,
            call_args.join(", ")
        ),
        Scalar::Float(_) => format!(
            "        double r = (double) ({}({}));\n        fprintf(fout, \"{{\\\"ok\\\":%.17g}}\\n\", r);",
            hs.name,
            call_args.join(", ")
        ),
        Scalar::Int { signed: false, .. } => format!(
            "        unsigned long long r = (unsigned long long) ({}({}));\n        fprintf(fout, \"{{\\\"ok\\\":%llu}}\\n\", r);",
            hs.name,
            call_args.join(", ")
        ),
        Scalar::Int { signed: true, .. } => format!(
            "        long long r = (long long) ({}({}));\n        fprintf(fout, \"{{\\\"ok\\\":%lld}}\\n\", r);",
            hs.name,
            call_args.join(", ")
        ),
    };
    let _ = arr_len_pairs;
    Ok(format!(
        r#"
#undef main
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
static void fa_o_ws(const char **p) {{ while (**p==' '||**p=='\t'||**p=='\r'||**p=='\n') (*p)++; }}
static char *fa_o_str(const char **p) {{
    if (strncmp(*p, "null", 4) == 0) {{ *p += 4; return NULL; }}
    if (**p != '"') return NULL;
    (*p)++;
    size_t cap = 64, n = 0; char *s = (char*) malloc(cap);
    while (**p && **p != '"') {{
        char c = *(*p)++;
        if (c == '\\' && **p) {{
            char e = *(*p)++;
            if (e=='n') c='\n'; else if (e=='t') c='\t'; else if (e=='r') c='\r'; else c=e;
        }}
        if (n + 2 > cap) {{ cap *= 2; s = (char*) realloc(s, cap); }}
        s[n++] = c;
    }}
    if (**p == '"') (*p)++;
    s[n] = 0;
    return s;
}}
int main(int argc, char **argv) {{
    if (argc < 3) return 2;
    FILE *fin = fopen(argv[1], "rb");
    FILE *fout = fopen(argv[2], "wb");
    if (!fin || !fout) return 3;
    char *line = NULL; size_t lcap = 0;
    while (getline(&line, &lcap, fin) > 0) {{
        const char *p = strstr(line, "\"args\"");
        if (!p) continue;
        p = strchr(p, '[');
        if (!p) continue;
        p++;
        char *endp = NULL;
{decls}{parse_cases}
{print_res}
{frees}    }}
    free(line);
    fclose(fin);
    fclose(fout);
    return 0;
}}
"#
    ))
}

fn cpp_oracle_driver(hs: &Hotspot, namespaces: &[String]) -> String {
    let open: String = namespaces
        .iter()
        .map(|n| {
            if n.is_empty() {
                "namespace {\n".to_string()
            } else {
                format!("namespace {n} {{\n")
            }
        })
        .collect();
    let close: String = namespaces.iter().map(|_| "}\n").collect();
    let mut vars = String::new();
    let mut call_args = Vec::new();
    for (i, p) in hs.params.iter().enumerate() {
        let ann = p.annotation.as_deref().unwrap_or("int");
        let (val_ty, is_cstr) = strip_cpp_ref_const(ann);
        vars.push_str(&format!(
            "        {val_ty} a{i}{{}};\n\
             \x20       if (i_check({i}, args)) fa_fuzz_oracle::from(args->arr[{i}], a{i});\n"
        ));
        if is_cstr {
            call_args.push(format!("a{i}.c_str()"));
        } else {
            call_args.push(format!("a{i}"));
        }
    }
    let fn_name = &hs.name;
    let call_args_str = call_args.join(", ");
    format!(
        r#"
#undef main
#include <algorithm>
#include <array>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <map>
#include <stdexcept>
#include <string>
#include <type_traits>
#include <unordered_map>
#include <utility>
#include <vector>
namespace fa_fuzz_oracle {{
{json}
inline bool i_check(size_t idx, const V *args) {{ return args && args->kind == 4 && idx < args->arr.size(); }}
}} // namespace fa_fuzz_oracle
{open}static std::string fa_invoke_oracle_{fn_name}(const fa_fuzz_oracle::V *args) {{
    using fa_fuzz_oracle::i_check;
{vars}    auto res = {fn_name}({call_args_str});
    std::string enc_ok;
    if (!fa_fuzz_oracle::enc(enc_ok, res)) return std::string("{{\"error\":\"unencodable return\"}}");
    return std::string("{{\"ok\":") + enc_ok + "}}";
}}
{close}
int main(int argc, char **argv) {{
    if (argc < 3) return 2;
    std::ifstream fin(argv[1]);
    std::ofstream fout(argv[2], std::ios::binary);
    if (!fin || !fout) return 3;
    std::string line;
    while (std::getline(fin, line)) {{
        if (line.find_first_not_of(" \t\r\n") == std::string::npos) continue;
        const char *p = line.c_str();
        fa_fuzz_oracle::V root = fa_fuzz_oracle::parse(p);
        const fa_fuzz_oracle::V *args = root.get("args");
        if (!args && root.kind == 4) args = &root;
        try {{
            fout << fa_invoke_oracle_{fn_name}(args) << '\n';
        }} catch (const std::exception &e) {{
            std::string msg;
            fa_fuzz_oracle::enc_str(msg, e.what());
            fout << "{{\"error\":" << msg << "}}\n";
        }} catch (...) {{
            fout << "{{\"error\":\"exception\"}}\n";
        }}
    }}
    return 0;
}}
"#,
        json = JSON_HPP,
    )
}

fn build_and_run_oracle(
    ctx: &Ctx,
    port: &Port,
    cases_path: &Path,
    oracle_out: &Path,
) -> Result<()> {
    let hs = ctx
        .state
        .hotspots
        .iter()
        .find(|h| h.id == port.id)
        .with_context(|| format!("{} not in last scan", port.id))?;
    let cc = ctx.runtime()?;
    let b = Build::parse(&ctx.state.command[1..])?;
    let file_path = ctx.root.join(&port.file);
    let orig_src = fs::read_to_string(&file_path)?;
    let stripped = backend::strip_marked(
        &native::strip_in_body(&orig_src, "//", &port.id),
        "//",
        &format!("{} bridge", port.id),
    );
    let cpp = port.lang == Lang::Cpp || is_cpp_file(&port.file);
    let driver = if cpp {
        let namespaces = parse(&stripped)
            .into_iter()
            .find(|f| f.name == hs.name && f.class == hs.owner)
            .map(|f| f.namespaces)
            .unwrap_or_default();
        cpp_oracle_driver(hs, &namespaces)
    } else {
        c_oracle_driver(hs)?
    };
    let oracle_dir = ctx.fast.join("fuzz_oracle");
    fs::create_dir_all(&oracle_dir)?;
    let fname = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .context("bad port filename")?;
    let oracle_tu = oracle_dir.join(format!("oracle_{fname}"));
    fs::write(&oracle_tu, format!("{stripped}\n{driver}"))?;
    let oracle_bin = oracle_dir.join(if cfg!(windows) {
        "oracle.exe"
    } else {
        "oracle"
    });

    let target_norm = fs::canonicalize(&file_path).ok();
    let mut srcs = Vec::new();
    let mut replaced = false;
    for s in &b.sources {
        let cand = ctx.root.join(s);
        let match_port = target_norm
            .as_ref()
            .zip(fs::canonicalize(&cand).ok())
            .is_some_and(|(a, b)| a == &b)
            || s == &port.file;
        if match_port {
            srcs.push(oracle_tu.to_string_lossy().into_owned());
            replaced = true;
        } else {
            srcs.push(s.clone());
        }
    }
    if !replaced {
        srcs.push(oracle_tu.to_string_lossy().into_owned());
    }
    let mut cmd = Command::new(cc);
    cmd.args(&b.flags)
        .arg("-Dmain=__fnr_orig_main")
        .arg("-I")
        .arg(&ctx.root);
    if let Some(par) = file_path.parent() {
        cmd.arg("-I").arg(par);
    }
    cmd.args(&srcs)
        .current_dir(&ctx.root)
        .arg("-o")
        .arg(&oracle_bin);
    let comp = output(&mut cmd, cc)?;
    ensure!(
        comp.status.success(),
        "compiling oracle failed: {}",
        errors_first(&combined(&comp))
    );
    let _ = fs::remove_file(oracle_out);
    let ran = output(
        Command::new(&oracle_bin)
            .arg(cases_path)
            .arg(oracle_out)
            .current_dir(&ctx.root)
            .env(DISABLE_ENV, "1")
            .env("FNR_DISABLE", "1")
            .stdin(Stdio::null()),
        "fuzz oracle",
    )?;
    ensure!(
        ran.status.success() && oracle_out.exists(),
        "running oracle failed: {}",
        tail(&combined(&ran), 12)
    );
    Ok(())
}

/// Differential fuzzing for C/C++: evaluates synthesized cases in the original C/C++ binary
/// first and compares every output against the Rust port (falling back to crash+determinism if needed).
pub fn fuzz_cases(
    ctx: &Ctx,
    port: &Port,
    cases_path: &Path,
    out_path: &Path,
    float_tolerance: Option<f64>,
) -> Result<native::NativeFuzzReport> {
    let oracle_out = ctx.crate_dir(port).join("fuzz-oracle.jsonl");
    match build_and_run_oracle(ctx, port, cases_path, &oracle_out) {
        Ok(()) => native::fuzz_cases_against_oracle(
            ctx,
            port,
            false,
            cases_path,
            &oracle_out,
            out_path,
            float_tolerance,
            None,
        ),
        Err(e) => native::fuzz_cases(ctx, port, false, cases_path, out_path, float_tolerance).map(
            |mut rep| {
                rep.oracle_note = Some(format!("C/C++ differential oracle skipped: {e}"));
                rep
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_c_and_cpp_functions() {
        let c = "#include <stdio.h>\n/* a { comment */\nstatic int lev(const char *a, const char *b) {\n  return 0;\n}\n\nlong count(const int *xs, size_t n, int max)\n{\n  if (n) { return 1; }\n  printf(\"{\");\n  return 0;\n}\nint main(void) { return lev(\"a\", \"b\"); }\n";
        assert_eq!(detect("icpx"), Some(Lang::Cpp));
        assert_eq!(detect("C:\\oneAPI\\bin\\dpcpp.exe"), Some(Lang::Cpp));
        assert_eq!(detect("icx"), Some(Lang::C));
        assert_eq!(detect("icx-cl"), None);
        let f = parse(c);
        assert_eq!(
            f.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["lev", "count", "main"]
        );
        assert_eq!(f[0].ret, "int");
        assert!(f[0].is_static);
        assert_eq!(
            c_kinds(&f[1].params).unwrap(),
            vec![
                CKind::Array(scalar("int").unwrap(), 1),
                CKind::Len,
                CKind::Scalar(scalar("int").unwrap())
            ]
        );
        let cpp = "namespace text {\nclass Fuzzy {\npublic:\n  static std::vector<std::pair<int, int>> find(const std::vector<std::string> &names, int max = 2) {\n    return {};\n  }\n  int size() const { return 0; }\n};\n}\n";
        let f = parse(cpp);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].class.as_deref(), Some("Fuzzy"));
        assert_eq!(f[0].namespaces, ["text"]);
        assert_eq!(f[0].params[1].default.as_deref(), Some("2"));
        assert_eq!(cpp_rust_type(&f[0].ret).as_deref(), Some("Vec<(i32, i32)>"));
        assert_eq!(
            cpp_rust_type("const std::vector<std::string>&").as_deref(),
            Some("Vec<String>")
        );
        assert_eq!(detect("clang++"), Some(Lang::Cpp));
        assert_eq!(detect("/usr/bin/gcc-14"), Some(Lang::C));
        assert_eq!(detect("clang-cl"), None.or(detect("clang-cl")));
        let b = Build::parse(&[
            "-O2".into(),
            "main.c".into(),
            "-o".into(),
            "app".into(),
            "--".into(),
            "x".into(),
        ])
        .unwrap();
        assert_eq!(
            (b.flags, b.sources, b.args),
            (
                vec!["-O2".to_string()],
                vec!["main.c".to_string()],
                vec!["x".to_string()]
            )
        );
    }

    #[test]
    fn direct_c_and_cpp_bridges_skip_json_for_compatible_sigs() {
        use crate::hotspot::Param;
        let hs = Hotspot {
            id: "mandelbrot.c:mandelbrot".into(),
            file: "mandelbrot.c".into(),
            line: 1,
            end_line: 10,
            name: "mandelbrot".into(),
            qualname: "mandelbrot".into(),
            params: vec![
                Param {
                    name: "width".into(),
                    annotation: Some("int".into()),
                    default: None,
                    kwonly: false,
                },
                Param {
                    name: "height".into(),
                    annotation: Some("int".into()),
                    default: None,
                    kwonly: false,
                },
                Param {
                    name: "max_iter".into(),
                    annotation: Some("int".into()),
                    default: None,
                    kwonly: false,
                },
            ],
            returns: Some("long long".into()),
            ..Hotspot::default()
        };
        let c_src = c_bridge(&hs, "mandelbrot", "libx.so", ".fast/lib/libx.so").unwrap();
        assert!(c_src.contains("fa_direct"), "{c_src}");
        assert!(!c_src.contains("snprintf"), "{c_src}");
        assert!(c_src.contains("getenv(\"FNR_STRICT\")"), "{c_src}");
        assert!(c_src.contains("#define FA_STATE_T"), "{c_src}");
        assert!(c_src.contains("FA_STORE_REL(&state"), "{c_src}");
        assert!(!c_src.contains("FNR_mandelbrot"), "{c_src}");

        let cpp_hs = Hotspot {
            id: "math.cpp:dot".into(),
            file: "math.cpp".into(),
            line: 1,
            end_line: 5,
            name: "dot".into(),
            qualname: "dot".into(),
            params: vec![
                Param {
                    name: "a".into(),
                    annotation: Some("const std::vector<double>&".into()),
                    default: None,
                    kwonly: false,
                },
                Param {
                    name: "b".into(),
                    annotation: Some("const std::vector<double>&".into()),
                    default: None,
                    kwonly: false,
                },
            ],
            returns: Some("double".into()),
            ..Hotspot::default()
        };
        let cpp_src = cpp_bridge(&cpp_hs, "dot", "libx.so", ".fast/lib/libx.so", &[]);
        assert!(cpp_src.contains("fa_direct"), "{cpp_src}");
        assert!(!cpp_src.contains("fa_bridge_dot::parse"), "{cpp_src}");
        assert!(cpp_src.contains("std::getenv(\"FNR_STRICT\")"), "{cpp_src}");
        assert!(!cpp_src.contains("FNR_dot"), "{cpp_src}");
    }

    #[test]
    fn parses_compile_commands_json_arguments_and_command_strings() {
        let dir = std::env::temp_dir().join(format!("fnr-cdb-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let cdb = dir.join("compile_commands.json");
        fs::write(
            &cdb,
            serde_json::json!([
                {
                    "directory": dir.to_string_lossy(),
                    "file": "src/main.c",
                    "arguments": ["clang", "-O2", "-Iinclude", "-c", "src/main.c", "-o", "main.o"]
                },
                {
                    "directory": dir.to_string_lossy(),
                    "file": "src/mandelbrot.c",
                    "command": "clang -O2 -ffp-contract=off -DM=1 -c src/mandelbrot.c -o mandelbrot.o"
                }
            ])
            .to_string(),
        )
        .unwrap();
        let cmd = command_from_cdb(&cdb, &["--arg".to_string()]).unwrap();
        assert_eq!(cmd[0], "clang");
        assert!(cmd.contains(&"-O2".to_string()));
        assert!(cmd.contains(&"-ffp-contract=off".to_string()));
        assert!(cmd.iter().any(|s| s.ends_with("src/main.c")));
        assert!(cmd.iter().any(|s| s.ends_with("src/mandelbrot.c")));
        assert!(!cmd.contains(&"-c".to_string()));
        assert!(!cmd.contains(&"main.o".to_string()));
        assert_eq!(&cmd[cmd.len() - 2..], ["--", "--arg"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
