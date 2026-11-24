//! PowerShell: script modules (`.psm1`) and scripts (`.ps1`), their `Import-Module`, `using
//! module` and dot-sourced imports, risks, and the functions a module exports with their
//! `param()` types mapped into the TS-like type dialect.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use tree_sitter::Node;

use super::syntax::{self, named_children, text};
use crate::risks::{Risk, regex_needs_fancy};
use crate::ts::deps::CrateMatch;
use crate::ts::discover::{FileKind, Package, SourceFile, Workspace};
use crate::ts::exports::{ExportedFn, Param};
use crate::ts::scan::{FileFacts, Import};

fn language() -> tree_sitter::Language {
    tree_sitter_powershell::LANGUAGE.into()
}

const SKIP_DIRS: &[&str] = &["node_modules", "target", "bin", "obj", "out", "Output"];

// ---- discovery -------------------------------------------------------------------------------

/// The module names a `.psd1` manifest lists under `RequiredModules`, as plain names or
/// `@{ ModuleName = 'x'; ... }` entries.
fn required_modules(text: &str) -> Vec<String> {
    static ENTRY: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let entry = ENTRY.get_or_init(|| {
        regex::Regex::new(r#"(?i)ModuleName\s*=\s*['"]([^'"]+)['"]|['"]([^'"]+)['"]"#)
            .expect("valid regex")
    });
    let lower = text.to_ascii_lowercase();
    let Some(start) = lower.find("requiredmodules") else {
        return Vec::new();
    };
    let rest = &text[start..];
    let Some(open) = rest.find(['(', '\'', '"', '@']) else {
        return Vec::new();
    };
    let mut depth = 0i32;
    let mut end = rest.len();
    for (i, c) in rest[open..].char_indices() {
        match c {
            '(' | '{' => depth += 1,
            ')' | '}' => depth -= 1,
            '\n' if depth <= 0 => {
                end = open + i;
                break;
            }
            _ => {}
        }
        if depth < 0 {
            end = open + i;
            break;
        }
    }
    let body: String = rest[open..end]
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let mut names = Vec::new();
    for c in entry.captures_iter(&body) {
        if let Some(name) = c.get(1) {
            names.push(name.as_str().to_string());
        } else if let Some(name) = c.get(2)
            && !body[..c.get(0).map_or(0, |m| m.start())]
                .trim_end()
                .to_ascii_lowercase()
                .ends_with('=')
        {
            names.push(name.as_str().to_string());
        }
    }
    names.dedup();
    names
}

fn is_test(rel: &str, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tests.ps1")
        || lower.ends_with(".test.ps1")
        || rel
            .split('/')
            .any(|p| matches!(p.to_ascii_lowercase().as_str(), "test" | "tests"))
}

pub fn discover(root: &Path) -> Result<Workspace> {
    let mut packages: Vec<Package> = Vec::new();
    let mut files = Vec::new();
    let walker = crate::walk::filtered(root, |e| {
        !(e.file_type().is_some_and(|t| t.is_dir())
            && SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
    })
    .hidden(true)
    .require_git(false)
    .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        let dir = rel
            .rsplit_once('/')
            .map_or(String::new(), |(d, _)| d.to_string());
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".psd1") {
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            if !text.to_ascii_lowercase().contains("moduleversion") {
                continue;
            }
            let deps = required_modules(&text);
            let module = name[..name.len() - ".psd1".len()].to_string();
            match packages.iter_mut().find(|p| p.dir == dir) {
                Some(p) => {
                    p.deps.extend(deps);
                    p.name = module;
                }
                None => packages.push(Package {
                    name: module,
                    src_root: dir.clone(),
                    dir,
                    deps,
                    dev_deps: Vec::new(),
                    namespaces: Vec::new(),
                }),
            }
        } else if lower.ends_with(".ps1") || lower.ends_with(".psm1") {
            let kind = if is_test(&rel, &name) {
                FileKind::Test
            } else {
                FileKind::Source
            };
            files.push((rel, kind));
        }
    }
    if !packages.iter().any(|p| p.dir.is_empty()) {
        packages.push(Package {
            name: root
                .file_name()
                .map_or_else(|| "root".into(), |n| n.to_string_lossy().into_owned()),
            dir: String::new(),
            src_root: String::new(),
            deps: Vec::new(),
            dev_deps: Vec::new(),
            namespaces: Vec::new(),
        });
    }
    packages.sort_by(|a, b| a.dir.cmp(&b.dir));
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let files = files
        .into_iter()
        .map(|(path, kind)| {
            let package = packages
                .iter()
                .enumerate()
                .filter(|(_, p)| p.dir.is_empty() || path.starts_with(&format!("{}/", p.dir)))
                .max_by_key(|(_, p)| p.dir.len())
                .map_or(0, |(i, _)| i);
            SourceFile {
                path,
                package,
                kind,
            }
        })
        .collect();
    Ok(Workspace { packages, files })
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

// ---- risks and imports -----------------------------------------------------------------------

/// The command a `command` node runs, lower-cased (`import-module`), or `None` for `& $x` and
/// `. path`.
fn command_name(node: Node, source: &str) -> Option<String> {
    let name = node.child_by_field_name("command_name")?;
    (name.kind() == "command_name").then(|| text(name, source).to_ascii_lowercase())
}

/// The arguments of a command that are not `-Parameter` names, in order.
fn command_words<'s>(node: Node, source: &'s str) -> Vec<&'s str> {
    let Some(elements) = node.child_by_field_name("command_elements") else {
        return Vec::new();
    };
    let mut words = Vec::new();
    for child in named_children(elements) {
        match child.kind() {
            "command_argument_sep" | "command_parameter" | "redirection" => {}
            "generic_token" | "string_literal" | "array_literal_expression" => {
                words.push(text(child, source).trim().trim_matches(['\'', '"']));
            }
            _ => {}
        }
    }
    words
}

/// A path as written after `.` or `Import-Module`, relative to the file: `$PSScriptRoot/x.ps1`
/// and `.\x.ps1` become `./x.ps1`.
fn local_path(spec: &str) -> Option<String> {
    let spec = spec.trim().trim_matches(['\'', '"']).replace('\\', "/");
    let rest = ["$PSScriptRoot/", "${PSScriptRoot}/", "./"]
        .iter()
        .find_map(|p| {
            spec.get(..p.len())
                .filter(|head| head.eq_ignore_ascii_case(p))
                .map(|_| &spec[p.len()..])
        })
        .map(str::to_string)
        .or_else(|| spec.starts_with("../").then(|| spec.clone()))?;
    if rest.contains('$') || rest.contains('(') {
        return None;
    }
    Some(if rest.starts_with("../") {
        rest
    } else {
        format!("./{rest}")
    })
}

const COMPARISONS: &[&str] = &[
    "-eq",
    "-ne",
    "-gt",
    "-ge",
    "-lt",
    "-le",
    "-like",
    "-notlike",
    "-match",
    "-notmatch",
    "-contains",
    "-notcontains",
    "-in",
    "-notin",
    "-replace",
    "-split",
];

/// Whether a statement of a function body sends a value to the pipeline without `return`,
/// `Write-Output` or an assignment: PowerShell's implicit output.
fn implicit_output(node: Node, source: &str) -> bool {
    if node.kind() != "pipeline" {
        return false;
    }
    let mut assigns = false;
    let mut silent = false;
    syntax::walk(node, |n| match n.kind() {
        "assignment_expression" => assigns = true,
        "command" => {
            if let Some(name) = command_name(n, source)
                && (name.starts_with("write-host")
                    || name.starts_with("out-null")
                    || name.starts_with("set-")
                    || name.starts_with("remove-")
                    || name.starts_with("add-")
                    || name == "throw")
            {
                silent = true;
            }
        }
        _ => {}
    });
    !assigns && !silent && !text(node, source).trim_start().starts_with("[void]")
}

pub fn scan(source: &str) -> FileFacts {
    let mut facts = FileFacts {
        loc: syntax::loc(source),
        ..FileFacts::default()
    };
    let Some(tree) = syntax::parse(&language(), source) else {
        return facts;
    };
    let root = tree.root_node();
    facts.parse_errors = usize::from(root.has_error());
    let add = |facts: &mut FileFacts, risk: Risk, node: Node| {
        facts
            .risks
            .entry(risk)
            .or_default()
            .push(syntax::line(node))
    };
    syntax::walk(root, |node| match node.kind() {
        "command" => {
            let invoked = node
                .child(0)
                .filter(|c| c.kind() == "command_invokation_operator")
                .map(|c| text(c, source).trim().to_string());
            if invoked.as_deref() == Some(".") {
                let target = node
                    .child_by_field_name("command_name")
                    .map_or("", |n| text(n, source));
                match local_path(target) {
                    Some(spec) => facts.imports.push(Import {
                        specifier: spec,
                        type_only: false,
                        dynamic: false,
                    }),
                    None => add(&mut facts, Risk::DynamicCode, node),
                }
                return;
            }
            if invoked.as_deref() == Some("&") {
                add(&mut facts, Risk::Callbacks, node);
                return;
            }
            let Some(name) = command_name(node, source) else {
                return;
            };
            match name.as_str() {
                "import-module" | "ipmo" | "using" => {
                    let words = command_words(node, source);
                    let spec = if name == "using" {
                        match words.as_slice() {
                            [kind, spec, ..] if kind.eq_ignore_ascii_case("module") => Some(*spec),
                            _ => None,
                        }
                    } else {
                        words.first().copied()
                    };
                    if let Some(spec) = spec {
                        if spec.contains('$') && local_path(spec).is_none() {
                            add(&mut facts, Risk::DynamicCode, node);
                        } else {
                            facts.imports.push(Import {
                                specifier: local_path(spec).unwrap_or_else(|| spec.to_string()),
                                type_only: false,
                                dynamic: false,
                            });
                        }
                    }
                }
                "invoke-expression" | "iex" | "invoke-command" | "start-job" | "add-type" => {
                    add(&mut facts, Risk::DynamicCode, node)
                }
                "foreach-object" | "%" | "where-object" | "?" => {
                    facts.loops += 1;
                    add(&mut facts, Risk::Callbacks, node);
                }
                "sort-object" | "sort" | "group-object" | "group" => {
                    add(&mut facts, Risk::OrderedCollections, node)
                }
                "write-output" | "echo" => add(&mut facts, Risk::PipelineOutput, node),
                "get-date" | "get-random" | "new-guid" => {
                    add(&mut facts, Risk::ClockAndRandom, node)
                }
                "convertto-json" | "convertfrom-json" => add(&mut facts, Risk::Json, node),
                "write-error" => add(&mut facts, Risk::Exceptions, node),
                "start-threadjob" | "start-process" => add(&mut facts, Risk::Concurrency, node),
                _ => {}
            }
        }
        "comparison_operator" => {
            let op = text(node, source).to_ascii_lowercase();
            if COMPARISONS.contains(&op.as_str()) {
                add(&mut facts, Risk::CaseInsensitive, node);
            }
            if matches!(op.as_str(), "-match" | "-notmatch" | "-replace" | "-split")
                && let Some(pattern) = node.next_named_sibling()
            {
                let mut literal = None;
                syntax::walk(pattern, |n| {
                    if literal.is_none() && n.kind() == "string_literal" {
                        literal = Some(text(n, source));
                    }
                });
                match literal {
                    Some(p)
                        if regex_needs_fancy(p.trim_matches(['\'', '"']), "")
                            || p.contains("(?<") =>
                    {
                        add(&mut facts, Risk::RegexSyntax, node)
                    }
                    Some(_) => {}
                    None => add(&mut facts, Risk::DynamicRegex, node),
                }
            }
        }
        "switch_statement" => add(&mut facts, Risk::CaseInsensitive, node),
        "variable" => {
            let v = text(node, source).to_ascii_lowercase();
            if v == "$null" {
                add(&mut facts, Risk::NullReferences, node);
            } else if v.starts_with("$script:") || v.starts_with("$global:") {
                add(&mut facts, Risk::DynamicTyping, node);
            }
        }
        "hash_literal_expression" => {
            let ordered = node
                .parent()
                .and_then(|p| p.parent())
                .filter(|p| p.kind() == "cast_expression")
                .is_some_and(|p| {
                    text(p, source)
                        .to_ascii_lowercase()
                        .starts_with("[ordered]")
                });
            if ordered {
                add(&mut facts, Risk::OrderedCollections, node);
            } else {
                add(&mut facts, Risk::UnorderedIteration, node);
            }
        }
        "cast_expression" => add(&mut facts, Risk::LooseConversion, node),
        "multiplicative_expression" | "multiplicative_argument_expression"
            if named_children(node).len() > 1 && text(node, source).contains('/') =>
        {
            add(&mut facts, Risk::LooseConversion, node)
        }
        "try_statement" | "trap_statement" => add(&mut facts, Risk::Exceptions, node),
        "flow_control_statement"
            if text(node, source).to_ascii_lowercase().starts_with("throw") =>
        {
            add(&mut facts, Risk::Exceptions, node)
        }
        "script_block_expression" => add(&mut facts, Risk::Callbacks, node),
        "class_statement" => add(&mut facts, Risk::ClassInheritance, node),
        "foreach_statement" | "for_statement" | "while_statement" | "do_statement" => {
            facts.loops += 1
        }
        "function_statement" => {
            let body = node
                .named_children(&mut node.walk())
                .find(|c| c.kind() == "script_block")
                .and_then(|b| b.child_by_field_name("script_block_body"));
            if let Some(statements) = body.and_then(|b| {
                named_children(b)
                    .into_iter()
                    .find(|c| c.kind() == "statement_list")
            }) && named_children(statements)
                .into_iter()
                .any(|s| implicit_output(s, source))
            {
                add(&mut facts, Risk::PipelineOutput, node);
            }
        }
        _ => {}
    });
    facts
}

/// Modules that ship with PowerShell: no crate needed to replace an import of them.
fn builtin(module: &str) -> bool {
    let m = module.to_ascii_lowercase();
    m.starts_with("microsoft.powershell.")
        || matches!(
            m.as_str(),
            "psreadline" | "packagemanagement" | "powershellget" | "threadjob" | "cimcmdlets"
        )
}

pub fn resolve(
    from: &str,
    spec: &str,
    index: &HashMap<&str, usize>,
    _ws: &Workspace,
) -> super::Resolution {
    if spec.starts_with("./") || spec.starts_with("../") {
        let dir = from.rsplit_once('/').map_or("", |(d, _)| d);
        let joined = join(dir, spec.trim_start_matches("./"));
        let Some(path) = normalize(&joined) else {
            return super::Resolution::Unresolved;
        };
        for candidate in [
            path.clone(),
            format!("{path}.psm1"),
            format!("{path}.ps1"),
            format!("{path}/{}.psm1", path.rsplit('/').next().unwrap_or(&path)),
        ] {
            if let Some(i) = index.get(candidate.as_str()) {
                return super::Resolution::File(*i);
            }
        }
        if let Some(manifest) = path.strip_suffix(".psd1") {
            let module = manifest.rsplit('/').next().unwrap_or(manifest);
            if let Some(i) = index.get(format!("{manifest}.psm1").as_str()).or_else(|| {
                let dir = manifest.rsplit_once('/').map_or("", |(d, _)| d);
                index.get(join(dir, &format!("{module}.psm1")).as_str())
            }) {
                return super::Resolution::File(*i);
            }
        }
        return super::Resolution::Unresolved;
    }
    if builtin(spec) {
        return super::Resolution::Stdlib;
    }
    super::Resolution::External(spec.to_string())
}

fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// Rust counterparts of common PowerShell Gallery modules.
pub fn lookup(module: &str) -> Option<CrateMatch> {
    crate::crates::find("powershell", module)
}

// ---- exports ---------------------------------------------------------------------------------

/// A PowerShell type as written inside `[...]` (`string`, `int[]`, `System.Int64`), in the
/// dialect; `None` for a .NET object that does not cross as JSON.
pub fn ps_to_ts(t: &str) -> Option<String> {
    let t = t.trim();
    if let Some(inner) = t.strip_suffix("[]") {
        let inner = ps_to_ts(inner)?;
        return Some(if inner.contains(' ') {
            format!("({inner})[]")
        } else {
            format!("{inner}[]")
        });
    }
    let lower = t.to_ascii_lowercase();
    let lower = lower.strip_prefix("system.").unwrap_or(&lower);
    Some(
        match lower {
            "string" | "char" => "string",
            "int" | "int32" | "long" | "int64" | "int16" | "short" | "byte" | "sbyte"
            | "uint16" | "ushort" | "uint32" | "uint" | "uint64" | "ulong" => "int",
            "double" | "single" | "float" | "decimal" => "number",
            "bool" | "boolean" | "switch" | "management.automation.switchparameter" => "boolean",
            "array" | "object[]" | "collections.arraylist" => "unknown[]",
            "hashtable"
            | "collections.hashtable"
            | "collections.idictionary"
            | "ordered"
            | "collections.specialized.ordereddictionary"
            | "pscustomobject"
            | "management.automation.pscustomobject" => "Record<string, unknown>",
            "object" | "psobject" | "management.automation.psobject" => "unknown",
            _ => return None,
        }
        .to_string(),
    )
}

/// The type a `[...]` constraint names, without brackets (`[string[]]` gives `string[]`).
fn type_of(attribute: Node, source: &str) -> Option<String> {
    let literal = named_children(attribute)
        .into_iter()
        .find(|c| c.kind() == "type_literal")?;
    let t = text(literal, source).trim();
    Some(
        t.strip_prefix('[')
            .and_then(|r| r.strip_suffix(']'))
            .unwrap_or(t)
            .trim()
            .to_string(),
    )
}

/// The name of a `[Name(...)]` attribute, lower-cased.
fn attribute_name(attribute: Node, source: &str) -> Option<String> {
    let name = named_children(attribute)
        .into_iter()
        .find(|c| c.kind() == "attribute_name")?;
    Some(text(name, source).trim().to_ascii_lowercase())
}

fn attributes<'t>(list: Option<Node<'t>>) -> Vec<Node<'t>> {
    list.map(|l| {
        named_children(l)
            .into_iter()
            .filter(|a| a.kind() == "attribute")
            .collect()
    })
    .unwrap_or_default()
}

/// The param block of a function: `param(...)` in its body, or `function f(...)`.
fn param_block<'t>(function: Node<'t>) -> Option<Node<'t>> {
    let children = named_children(function);
    if let Some(declared) = children
        .iter()
        .find(|c| c.kind() == "function_parameter_declaration")
    {
        return Some(*declared);
    }
    for c in children {
        if matches!(c.kind(), "script_block" | "ERROR")
            && let Some(block) = named_children(c)
                .into_iter()
                .find(|n| n.kind() == "param_block")
        {
            return Some(block);
        }
    }
    None
}

/// The names `Export-ModuleMember -Function` lists at the top level of a module, or `None`
/// when it has none (every function is exported).
fn exported_names(root: Node, source: &str) -> Option<Vec<String>> {
    let mut names: Option<Vec<String>> = None;
    for statement in named_children(root)
        .into_iter()
        .filter(|n| n.kind() == "statement_list")
        .flat_map(named_children)
        .filter(|n| n.kind() != "function_statement")
    {
        let mut commands = Vec::new();
        syntax::walk(statement, |n| {
            if n.kind() == "command" {
                commands.push(n);
            }
        });
        for command in commands {
            if command_name(command, source).as_deref() != Some("export-modulemember") {
                continue;
            }
            let list = names.get_or_insert_with(Vec::new);
            let elements = command
                .child_by_field_name("command_elements")
                .map_or("", |e| text(e, source));
            list.extend(function_list(elements));
        }
    }
    names
}

/// `text` with up to `column` leading spaces taken off every line after the first, which starts
/// at that column in the source.
fn dedent(text: &str, column: usize) -> String {
    let mut lines = text.lines();
    let mut out = lines.next().unwrap_or("").to_string();
    for line in lines {
        let spaces = line.len() - line.trim_start_matches([' ', '\t']).len();
        out.push('\n');
        out.push_str(&line[spaces.min(column)..]);
    }
    out
}

/// The names after `-Function` (or in the first position) of `Export-ModuleMember`'s arguments:
/// a word that starts with `-` after a space is the next parameter, unless a comma comes before it.
fn function_list(arguments: &str) -> Vec<String> {
    let mut param = "-function".to_string();
    let mut list = Vec::new();
    let mut continues = false;
    for word in arguments.split_whitespace() {
        if word.starts_with('-') && !continues {
            param = word.to_ascii_lowercase();
            continue;
        }
        continues = word.ends_with(',');
        if param.starts_with("-f") {
            list.extend(
                word.split(',')
                    .map(|w| w.trim().trim_matches(['\'', '"', '@', '(', ')']))
                    .filter(|w| !w.is_empty())
                    .map(str::to_string),
            );
        }
        if !continues {
            param = "-cmdlet".into();
        }
    }
    list
}

fn wildcard(pattern: &str, name: &str) -> bool {
    let pattern = regex::escape(&pattern.to_ascii_lowercase())
        .replace(r"\*", ".*")
        .replace(r"\?", ".");
    regex::Regex::new(&format!("^{pattern}$"))
        .is_ok_and(|re| re.is_match(&name.to_ascii_lowercase()))
}

/// What keeps a parameter of type `t` in PowerShell, if anything.
fn parameter_blocker(name: &str, t: &str) -> String {
    let lower = t.to_ascii_lowercase();
    if lower == "scriptblock" || lower.ends_with(".scriptblock") {
        format!("takes a script block `${name}`")
    } else if lower == "ref" {
        format!("takes `${name}` by reference (`[ref]`)")
    } else {
        format!("takes a .NET object `${name}` (`[{t}]`)")
    }
}

fn function_fn(path: &str, node: Node, source: &str) -> Option<ExportedFn> {
    let name = named_children(node)
        .into_iter()
        .find(|c| c.kind() == "function_name")
        .map(|n| text(n, source).trim().to_string())?;
    let keyword = text(node, source)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut blocker = None;
    if keyword == "filter" {
        blocker = Some("a filter: it runs once per pipeline item".to_string());
    } else if keyword == "workflow" {
        blocker = Some("a workflow".to_string());
    }
    let block = param_block(node);
    let declared = block.is_some_and(|b| b.kind() == "function_parameter_declaration");
    let list = block.and_then(|b| {
        named_children(b)
            .into_iter()
            .find(|c| c.kind() == "parameter_list")
    });
    let block_attributes = block.filter(|b| b.kind() == "param_block").and_then(|b| {
        named_children(b)
            .into_iter()
            .find(|c| c.kind() == "attribute_list")
    });
    let mut returns = "unknown".to_string();
    for attribute in attributes(block_attributes) {
        if attribute_name(attribute, source).as_deref() == Some("outputtype") {
            let mut found = None;
            syntax::walk(attribute, |n| {
                if found.is_none() && n.kind() == "type_literal" {
                    let t = text(n, source).trim();
                    found = Some(t.trim_start_matches('[').trim_end_matches(']').to_string());
                }
            });
            if let Some(t) = found.and_then(|t| ps_to_ts(&t)) {
                returns = t;
            }
        }
    }
    let mut params = Vec::new();
    for p in list
        .map(named_children)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.kind() == "script_parameter")
    {
        let Some(var) = named_children(p)
            .into_iter()
            .find(|c| c.kind() == "variable")
        else {
            continue;
        };
        let pname = text(var, source)
            .trim_start_matches('$')
            .trim_start_matches('{')
            .trim_end_matches('}')
            .to_string();
        let attrs = attributes(
            named_children(p)
                .into_iter()
                .find(|c| c.kind() == "attribute_list"),
        );
        let declared_type = attrs.iter().rev().find_map(|a| type_of(*a, source));
        let mut mandatory = false;
        for a in &attrs {
            let Some(aname) = attribute_name(*a, source) else {
                continue;
            };
            let atext = text(*a, source).to_ascii_lowercase().replace(' ', "");
            if aname == "parameter" {
                if atext.contains("mandatory") && !atext.contains("mandatory=$false") {
                    mandatory = true;
                }
                if atext.contains("valuefrompipeline") {
                    blocker.get_or_insert_with(|| format!("takes pipeline input `${pname}`"));
                }
            }
        }
        let default = named_children(p)
            .into_iter()
            .find(|c| c.kind() == "script_parameter_default")
            .map(|d| {
                text(d, source)
                    .trim()
                    .trim_start_matches('=')
                    .trim()
                    .to_string()
            });
        let ts_type = match &declared_type {
            Some(t) => match ps_to_ts(t) {
                Some(ts) => ts,
                None => {
                    blocker.get_or_insert_with(|| parameter_blocker(&pname, t));
                    "unknown".into()
                }
            },
            None => "unknown".into(),
        };
        let by_reference = matches!(ts_type.as_str(), "unknown" | "Record<string, unknown>")
            || ts_type.ends_with("[]");
        let null_default = default
            .as_deref()
            .is_none_or(|d| d.eq_ignore_ascii_case("$null"));
        params.push(Param {
            name: pname,
            ts_type,
            optional: !mandatory && by_reference && null_default,
            rest: false,
            default,
            keyword_only: false,
            native_type: declared_type.map(|t| format!("[{t}]")),
        });
    }
    let mut named_blocks = false;
    let mut pipeline_input = false;
    let mut dynamic = false;
    syntax::walk(node, |n| match n.kind() {
        "named_block" => {
            let block = text(n, source).trim_start().to_ascii_lowercase();
            if !block.starts_with("end") {
                named_blocks = true;
            }
        }
        "variable" => {
            let v = text(n, source).to_ascii_lowercase();
            if v == "$input" {
                pipeline_input = true;
            }
            if v == "$pscmdlet" || v == "$args" {
                dynamic = true;
            }
        }
        _ => {}
    });
    if named_blocks {
        blocker
            .get_or_insert_with(|| "has `begin`/`process` blocks: it streams the pipeline".into());
    }
    if pipeline_input {
        blocker.get_or_insert_with(|| "reads pipeline input (`$input`)".into());
    }
    if dynamic {
        blocker.get_or_insert_with(|| "reads `$PSCmdlet` or `$args`".into());
    }
    let prototype = match block {
        Some(_) if declared => Some(format!("param({})", list.map_or("", |l| text(l, source)))),
        Some(b) => Some(dedent(text(b, source), b.start_position().column)),
        None => Some("param()".to_string()),
    };
    Some(ExportedFn {
        name,
        file: path.to_string(),
        line: syntax::line(node),
        params,
        returns,
        blocker,
        prototype,
        ..ExportedFn::default()
    })
}

pub fn exports(path: &str, source: &str) -> Vec<ExportedFn> {
    let lower = path.to_ascii_lowercase();
    if !(lower.ends_with(".psm1") || lower.ends_with(".ps1")) {
        return Vec::new();
    }
    let Some(tree) = syntax::parse(&language(), source) else {
        return Vec::new();
    };
    let root = tree.root_node();
    let listed = if lower.ends_with(".psm1") {
        exported_names(root, source)
    } else {
        None
    };
    let mut out = Vec::new();
    for statement in named_children(root)
        .into_iter()
        .filter(|n| n.kind() == "statement_list")
        .flat_map(named_children)
    {
        if statement.kind() != "function_statement" {
            continue;
        }
        let Some(f) = function_fn(path, statement, source) else {
            continue;
        };
        if let Some(names) = &listed
            && !names.iter().any(|n| wildcard(n, &f.name))
        {
            continue;
        }
        out.push(f);
    }
    out
}

/// The byte offset just past the line where the last top-level function ends: where `enable`
/// puts its block, after every function it redefines and before the script's own code runs.
pub fn after_functions(source: &str) -> Option<usize> {
    let tree = syntax::parse(&language(), source)?;
    let end = named_children(tree.root_node())
        .into_iter()
        .filter(|n| n.kind() == "statement_list")
        .flat_map(named_children)
        .filter(|n| n.kind() == "function_statement")
        .map(|n| n.end_byte())
        .max()?;
    Some(
        source[end..]
            .find('\n')
            .map_or(source.len(), |i| end + i + 1),
    )
}

/// The parameter block a generated wrapper declares, as the original wrote it.
pub fn param_text(f: &ExportedFn) -> &str {
    f.prototype.as_deref().unwrap_or("param()")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULE: &str = include_str!("../../tests/fixtures/powershell-module/TextTools.psm1");

    fn find<'a>(fns: &'a [ExportedFn], name: &str) -> &'a ExportedFn {
        fns.iter().find(|f| f.name == name).unwrap_or_else(|| {
            panic!(
                "{name} not in {:?}",
                fns.iter().map(|f| &f.name).collect::<Vec<_>>()
            )
        })
    }

    #[test]
    fn module_functions_and_their_parameter_types() {
        let fns = exports("TextTools.psm1", MODULE);
        let slug = find(&fns, "ConvertTo-Slug");
        let params: Vec<(&str, &str, bool, Option<&str>)> = slug
            .params
            .iter()
            .map(|p| {
                (
                    p.name.as_str(),
                    p.ts_type.as_str(),
                    p.optional,
                    p.default.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            params,
            [
                ("Text", "string", false, None),
                ("Separator", "string", false, Some("'-'")),
                ("MaxLength", "int", false, Some("0")),
            ]
        );
        assert_eq!(slug.returns, "string");
        assert!(slug.blocker.is_none());
        assert!(
            slug.prototype
                .as_deref()
                .unwrap()
                .starts_with("[CmdletBinding()]"),
            "{:?}",
            slug.prototype
        );
        let mean = find(&fns, "Measure-Mean");
        assert_eq!(mean.params[0].ts_type, "number[]");
        assert!(mean.params[0].optional);
        assert_eq!(mean.returns, "number");
        let initials = find(&fns, "Get-Initials");
        assert_eq!(
            initials
                .params
                .iter()
                .map(|p| p.ts_type.as_str())
                .collect::<Vec<_>>(),
            ["string[]", "boolean"]
        );
        let setting = find(&fns, "Get-Setting");
        assert_eq!(setting.params[0].ts_type, "Record<string, unknown>");
        assert_eq!(setting.params[2].ts_type, "unknown");
        assert!(setting.params[2].optional);
        assert_eq!(find(&fns, "Test-Equal").returns, "boolean");
    }

    #[test]
    fn what_stays_in_powershell_says_why() {
        let fns = exports("TextTools.psm1", MODULE);
        let reasons: Vec<(&str, &str)> = fns
            .iter()
            .filter_map(|f| Some((f.name.as_str(), f.blocker.as_deref()?)))
            .collect();
        assert_eq!(
            reasons,
            [
                ("Get-Lines", "takes pipeline input `$Line`"),
                ("Format-Stamp", "takes a .NET object `$At` (`[datetime]`)"),
                ("Invoke-Twice", "takes a script block `$Action`"),
            ]
        );
    }

    #[test]
    fn export_module_member_limits_the_exports() {
        let text = "function Public-One { 1 }\nfunction helper { 2 }\nfunction Public-Two { 3 }\nExport-ModuleMember -Function Public-*\n";
        let names: Vec<String> = exports("m.psm1", text)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["Public-One", "Public-Two"]);
        let all: Vec<String> = exports("m.psm1", "function a { 1 }\nfunction b { 2 }\n")
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(all, ["a", "b"]);
        let script: Vec<String> = exports("s.ps1", text).into_iter().map(|f| f.name).collect();
        assert_eq!(script.len(), 3, "a script has no export list");
    }

    #[test]
    fn inline_parameters_and_nested_functions() {
        let text = "function Add([int]$a, [int]$b = 2) {\n    function inner { 1 }\n    $a + $b\n}\nfilter Odd { if ($_ % 2) { $_ } }\nfunction Stream {\n    param([int[]]$xs)\n    begin { $n = 0 }\n    process { $n++ }\n    end { $n }\n}\n";
        let fns = exports("x.ps1", text);
        let names: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Add", "Odd", "Stream"]);
        assert_eq!(fns[0].params[1].default.as_deref(), Some("2"));
        assert_eq!(
            fns[0].prototype.as_deref(),
            Some("param([int]$a, [int]$b = 2)")
        );
        assert!(fns[1].blocker.as_deref().unwrap().starts_with("a filter"));
        assert!(fns[2].blocker.as_deref().unwrap().contains("process"));
    }

    #[test]
    fn types_map_into_the_dialect() {
        assert_eq!(ps_to_ts("string").as_deref(), Some("string"));
        assert_eq!(ps_to_ts("System.Int64").as_deref(), Some("int"));
        assert_eq!(ps_to_ts("double[]").as_deref(), Some("number[]"));
        assert_eq!(
            ps_to_ts("hashtable[]").as_deref(),
            Some("(Record<string, unknown>)[]")
        );
        assert_eq!(ps_to_ts("switch").as_deref(), Some("boolean"));
        assert_eq!(ps_to_ts("DateTime"), None);
    }

    #[test]
    fn imports_and_risks() {
        let text = "using module ./Other.psm1\n. \"$PSScriptRoot/Helpers.ps1\"\nImport-Module PSReadLine\nImport-Module Pester\nfunction F {\n    param([string]$a)\n    if ($a -eq 'x') { $null }\n    $h = @{ a = 1 }\n    $a -replace '(?<=a)b', 'c'\n    $a\n}\n";
        let facts = scan(text);
        let specs: Vec<&str> = facts.imports.iter().map(|i| i.specifier.as_str()).collect();
        assert_eq!(
            specs,
            ["./Other.psm1", "./Helpers.ps1", "PSReadLine", "Pester"]
        );
        for risk in [
            Risk::CaseInsensitive,
            Risk::NullReferences,
            Risk::UnorderedIteration,
            Risk::RegexSyntax,
            Risk::PipelineOutput,
        ] {
            assert!(
                facts.risks.contains_key(&risk),
                "{risk:?}: {:?}",
                facts.risks
            );
        }
    }

    #[test]
    fn a_manifest_names_its_required_modules() {
        let text = "@{\n    ModuleVersion = '1.0'\n    RequiredModules = @('PSReadLine', @{ ModuleName = 'Pester'; ModuleVersion = '5.0' })\n}\n";
        assert_eq!(required_modules(text), ["PSReadLine", "Pester"]);
    }
}
