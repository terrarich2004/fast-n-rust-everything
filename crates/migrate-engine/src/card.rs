//! `report`: a unit's result as Markdown and an SVG card, for a pull request or a README.
//!
//! Every number comes from a run made for the report: `diff` for the recorded cases that
//! match, `bench` for the speedup, and the cases of functions the Rust module does not provide
//! for the fallback rate. The machine and its core count go next to the timing.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::bridge::Manifest;
use crate::harness::{self, BenchRow};
use crate::walk::Canonical;

pub const SCHEMA: &str = "rust-anything/report@1";

#[derive(Debug, Serialize)]
pub struct Card {
    pub schema: &'static str,
    pub crate_name: String,
    pub unit: String,
    pub language: String,
    pub functions: Vec<String>,
    pub cases: usize,
    pub matched: usize,
    /// Geometric mean of `original_ns / rust_ns` over the timed functions, call included.
    pub speedup: f64,
    pub bench: Vec<BenchRow>,
    /// Recorded cases whose function the Rust module does not provide, so the original answers.
    pub fallback_cases: usize,
    /// `Some(true)` while `enable` has switched the unit on; `None` where `enable` does not apply.
    pub enabled: Option<bool>,
    pub machine: String,
    pub markdown: PathBuf,
    pub svg: PathBuf,
}

impl Card {
    pub fn fallback_rate(&self) -> f64 {
        if self.cases == 0 {
            0.0
        } else {
            self.fallback_cases as f64 / self.cases as f64
        }
    }
}

pub struct Options<'a> {
    pub crate_dir: &'a Path,
    pub out: Option<&'a Path>,
    pub min_ms: u64,
    pub node_args: &'a [String],
}

pub fn report(opts: &Options) -> Result<Card> {
    anyhow::ensure!(
        opts.crate_dir.exists(),
        "folder {} does not exist (run `fast-n-rust bridge` first)",
        opts.crate_dir.display()
    );
    anyhow::ensure!(
        opts.crate_dir.is_dir(),
        "{} is not a directory",
        opts.crate_dir.display()
    );
    let crate_dir = opts
        .crate_dir
        .canonical()
        .with_context(|| format!("resolving {}", opts.crate_dir.display()))?;
    let manifest = Manifest::load(&crate_dir)?;
    harness::build(&crate_dir)?;
    let diff = harness::diff(&harness::DiffOptions {
        crate_dir: &crate_dir,
        module: None,
        ignore_key_order: false,
        node_args: opts.node_args,
        float_tolerance: None,
    })?;
    let bench = harness::bench(&harness::BenchOptions {
        crate_dir: &crate_dir,
        min_ms: opts.min_ms,
        resolve_from: None,
        node_args: opts.node_args,
    })?;
    let verdict = harness::verdict(&bench, manifest.language);
    let per_function = cases_per_function(&crate_dir);
    let fallback_cases = diff
        .missing
        .iter()
        .map(|f| per_function.get(f).copied().unwrap_or(0))
        .sum();
    let out = opts
        .out
        .map_or_else(|| crate_dir.join("report"), Path::to_path_buf);
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let mut card = Card {
        schema: SCHEMA,
        crate_name: manifest.crate_name.clone(),
        unit: manifest.unit.clone(),
        language: manifest.language.name().to_string(),
        functions: manifest.exports.iter().map(|f| f.name.clone()).collect(),
        cases: diff.total,
        matched: diff.passed,
        speedup: verdict.speedup,
        bench,
        fallback_cases,
        enabled: crate::enable::is_enabled(&crate_dir).ok(),
        machine: machine(),
        markdown: out.join("REPORT.md"),
        svg: out.join("card.svg"),
    };
    card.enabled = card
        .enabled
        .filter(|_| crate::enable::is_supported(&manifest));
    std::fs::write(&card.svg, svg(&card))?;
    std::fs::write(&card.markdown, markdown(&card))?;
    Ok(card)
}

fn cases_per_function(crate_dir: &Path) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    let text = std::fs::read_to_string(crate_dir.join(harness::CASES)).unwrap_or_default();
    for line in text.lines() {
        if let Ok(case) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(name) = case["fn"].as_str()
        {
            *out.entry(name.to_string()).or_default() += 1;
        }
    }
    out
}

/// The CPU's name and its core count, as `bench` numbers must be published with.
pub fn machine() -> String {
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let macos_brand = if cfg!(target_os = "macos") {
        let mut cmd = std::process::Command::new("sysctl");
        cmd.args(["-n", "machdep.cpu.brand_string"]);
        crate::process::output(&mut cmd, crate::process::QUICK)
            .ok()
            .filter(crate::process::Output::success)
            .map(|o| o.stdout_text().trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };
    let name = macos_brand
        .or_else(|| {
            std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|t| {
                t.lines()
                    .find(|l| l.starts_with("model name"))
                    .and_then(|l| l.split_once(':'))
                    .map(|(_, v)| v.trim().to_string())
            })
        })
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let os = match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    };
    format!("{name}, {cores} cores, {os}")
}

fn times(x: f64) -> String {
    // The boundary is where the one-decimal form would read `10.0x`: print the whole number it
    // already shows instead of a decimal that adds nothing.
    if x >= 9.95 {
        format!("{x:.0}x")
    } else {
        format!("{x:.1}x")
    }
}

/// Each threshold is the value at which the unit below it would print as `1000` of that unit,
/// rounded: `999_999 ns` is `1000.0 µs`, so it belongs in `ms`.
fn ns(v: f64) -> String {
    if v >= 999_950_000.0 {
        format!("{:.2} s", v / 1e9)
    } else if v >= 999_950.0 {
        format!("{:.1} ms", v / 1e6)
    } else if v >= 999.5 {
        format!("{:.1} µs", v / 1e3)
    } else {
        format!("{v:.0} ns")
    }
}

fn state(card: &Card) -> &'static str {
    match card.enabled {
        Some(true) => "on",
        Some(false) => "off",
        None => "not switched by `enable`",
    }
}

fn format_pct(numerator: usize, denominator: usize) -> String {
    if denominator == 0 || numerator == 0 {
        return "0.0%".to_string();
    }
    if numerator >= denominator {
        return "100.0%".to_string();
    }
    let pct = 100.0 * numerator as f64 / denominator as f64;
    let rounded = format!("{pct:.1}");
    if rounded == "0.0" {
        "<0.1%".to_string()
    } else if rounded == "100.0" {
        ">99.9%".to_string()
    } else {
        format!("{rounded}%")
    }
}

pub fn markdown(card: &Card) -> String {
    let mut out = format!(
        "## `{}` in Rust\n\n![{} report card](card.svg)\n\n\
         | | |\n| --- | --- |\n\
         | Unit | `{}` ({}) |\n\
         | Recorded cases matched | {} / {} |\n\
         | Speedup, call included | {} (geometric mean) |\n\
         | Fallback to the original | {} ({} of {} cases) |\n\
         | Switched on | {} |\n\n",
        card.crate_name,
        card.crate_name,
        card.unit,
        card.language,
        card.matched,
        card.cases,
        times(card.speedup),
        format_pct(card.fallback_cases, card.cases),
        card.fallback_cases,
        card.cases,
        state(card),
    );
    if !card.bench.is_empty() {
        out.push_str(
            "| Function | Cases | Original per call | Rust per call | Speedup |\n| --- | ---: | ---: | ---: | ---: |\n",
        );
        for r in &card.bench {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} | {} |",
                r.name,
                r.cases,
                ns(r.original_ns),
                ns(r.rust_ns),
                times(r.speedup)
            );
        }
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "Measured with `fnr report` on {}: `diff` replays every recorded case, `bench` \
         times them on both sides as callers call them. Reproduce: `fnr diff` and \
         `fnr bench` in the crate folder.",
        card.machine
    );
    out
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Light card in the v3 style: white, an outlined panel for the original, a tinted orange one
/// for Rust, the headline result below.
pub fn svg(card: &Card) -> String {
    const W: u32 = 1200;
    const H: u32 = 600;
    let font = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif";
    let mono = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";
    let (orig, rust) = card
        .bench
        .iter()
        .fold((0.0, 0.0), |(o, r), b| (o + b.original_ns, r + b.rust_ns));
    let max = orig.max(rust).max(1.0);
    let bar = |v: f64| (5.0_f64).max(470.0 * v / max);
    let all = card.matched == card.cases && card.cases > 0;
    let headline = if all {
        format!(
            "Same result as the original on all {} recorded cases",
            card.cases
        )
    } else {
        format!(
            "{} of {} recorded cases match the original",
            card.matched, card.cases
        )
    };
    let per_call = if card.bench.len() == 1 {
        "per call"
    } else {
        "per call, all functions"
    };
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="{label}">
<rect width="{W}" height="{H}" fill="#ffffff"/>
<g font-family="{font}">
<svg x="64" y="52" width="60" height="60" viewBox="0 0 200 200">{crab}</svg>
<text x="140" y="94" font-size="34" font-weight="700" fill="#1a1a1a">fast-n-rust</text>
<text x="380" y="94" font-size="{title_size}" font-weight="700" fill="#1a1a1a">{name}, ported to <tspan fill="#e8561a">Rust</tspan></text>
<text x="64" y="140" font-size="18" fill="#666666">{unit} ({lang}) · {functions}</text>
<rect x="64" y="172" width="520" height="230" rx="20" fill="#ffffff" stroke="#e5e5e5" stroke-width="3"/>
<text x="96" y="218" font-size="26" font-weight="700" fill="#1a1a1a">Original</text>
<text x="552" y="218" font-size="17" fill="#666666" text-anchor="end">{per_call}</text>
<text x="96" y="318" font-family="{mono}" font-size="64" font-weight="700" fill="#1a1a1a">{orig_t}</text>
<rect x="96" y="360" width="{orig_w:.0}" height="14" rx="7" fill="#9e9e9e"/>
<rect x="616" y="172" width="520" height="230" rx="20" fill="#fdf4ef" stroke="#f2b89a" stroke-width="3"/>
<text x="648" y="218" font-size="26" font-weight="700" fill="#1a1a1a">Rust port</text>
<text x="1104" y="218" font-size="17" fill="#666666" text-anchor="end">switched {state}</text>
<text x="648" y="318" font-family="{mono}" font-size="64" font-weight="700" fill="#e8561a">{rust_t}</text>
<rect x="648" y="360" width="{rust_w:.0}" height="14" rx="7" fill="#e8561a"/>
<text x="600" y="460" font-size="28" fill="#1a1a1a" text-anchor="middle">{headline}: <tspan font-weight="700" fill="#e8561a">{speed}</tspan></text>
<text x="600" y="500" font-size="18" fill="#777777" text-anchor="middle">Fallback to the original: {fallback} of cases · call included · {machine}</text>
<text x="600" y="556" font-size="17" fill="#888888" text-anchor="middle">github.com/terrarich2004/fast-n-rust-everything</text>
</g>
</svg>
"##,
        label = esc(&format!(
            "{}: {} of {} recorded cases match, {} faster",
            card.crate_name,
            card.matched,
            card.cases,
            times(card.speedup)
        )),
        name = esc(&card.crate_name),
        title_size = if card.crate_name.chars().count() > 22 {
            24
        } else {
            34
        },
        unit = esc(&card.unit),
        lang = esc(&card.language),
        functions = esc(&card.functions.join(", ")),
        orig_t = esc(&ns(orig)),
        rust_t = esc(&ns(rust)),
        orig_w = bar(orig),
        rust_w = bar(rust),
        state = if card.enabled == Some(true) {
            "on"
        } else {
            "off"
        },
        speed = if card.speedup >= 1.0 {
            format!("{} faster", times(card.speedup))
        } else {
            format!("{} slower", times(1.0 / card.speedup.max(1e-9)))
        },
        crab = include_str!("card_crab.svg").trim(),
        fallback = esc(&format_pct(card.fallback_cases, card.cases)),
        machine = esc(&card.machine),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_that_rounds_up_to_the_next_unit_prints_in_that_unit() {
        assert_eq!(ns(999.6), "1.0 µs");
        assert_eq!(ns(999_999.0), "1.0 ms");
        assert_eq!(ns(999_999_999.0), "1.00 s");
        assert_eq!(times(9.96), "10x");
        // Just under each boundary stays where it is.
        assert_eq!(ns(999.0), "999 ns");
        assert_eq!(ns(999_000.0), "999.0 µs");
        assert_eq!(ns(999_000_000.0), "999.0 ms");
        assert_eq!(times(9.94), "9.9x");
        // The ordinary cases are unchanged.
        assert_eq!(ns(500.0), "500 ns");
        assert_eq!(ns(1_500_000.0), "1.5 ms");
        assert_eq!(times(31.4), "31x");
    }

    fn card() -> Card {
        Card {
            schema: SCHEMA,
            crate_name: "fuzzy-rs".into(),
            unit: "app.py".into(),
            language: "Python".into(),
            functions: vec!["dedupe".into()],
            cases: 412,
            matched: 412,
            speedup: 31.4,
            bench: vec![BenchRow {
                name: "dedupe".into(),
                cases: 412,
                original_ns: 9_120_000.0,
                rust_ns: 290_000.0,
                speedup: 31.4,
            }],
            fallback_cases: 0,
            enabled: Some(true),
            machine: "Apple M4, 10 cores, macOS".into(),
            markdown: "REPORT.md".into(),
            svg: "card.svg".into(),
        }
    }

    #[test]
    fn the_card_shows_what_was_measured() {
        let svg = svg(&card());
        assert!(svg.starts_with("<svg"), "{svg}");
        for want in [
            "fuzzy-rs",
            "9.1 ms",
            "290.0 µs",
            "all 412 recorded cases",
            "31x faster",
            "switched on",
            "0.0% of cases",
            "Apple M4, 10 cores",
        ] {
            assert!(svg.contains(want), "{want} missing:\n{svg}");
        }
        let md = markdown(&card());
        assert!(
            md.contains("| Recorded cases matched | 412 / 412 |"),
            "{md}"
        );
        assert!(
            md.contains("| `dedupe` | 412 | 9.1 ms | 290.0 µs | 31x |"),
            "{md}"
        );
    }

    #[test]
    fn text_from_the_project_is_escaped() {
        let mut c = card();
        c.unit = "src/<a&b>.ts".into();
        c.matched = 400;
        let svg = svg(&c);
        assert!(svg.contains("src/&lt;a&amp;b&gt;.ts"));
        assert!(svg.contains("400 of 412 recorded cases match"));
    }

    #[test]
    fn fallback_rate_never_rounds_nonzero_to_zero_or_partial_to_hundred() {
        assert_eq!(format_pct(0, 2001), "0.0%");
        assert_eq!(format_pct(1, 2001), "<0.1%");
        assert_eq!(format_pct(2000, 2001), ">99.9%");
        assert_eq!(format_pct(2001, 2001), "100.0%");
    }

    #[test]
    fn report_refuses_missing_crate_dir() {
        let missing = std::env::temp_dir().join("fnr-no-such-report-crate");
        let err = report(&Options {
            crate_dir: &missing,
            out: None,
            min_ms: 10,
            node_args: &[],
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }
}
