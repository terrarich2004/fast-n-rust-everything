//! Times the whole app: original Python vs Rust ports, interleaved so drift hits both equally.

use crate::backend::Lang;
use crate::python::DISABLE_ENV;
use crate::state::Ctx;
use crate::{ui, util};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub median: f64,
    pub mean: f64,
    pub min: f64,
    pub max: f64,
    pub stddev: f64,
    pub ci95_low: f64,
    pub ci95_high: f64,
    pub cv_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_kb: Option<u64>,
}

fn t_crit_95(df: usize) -> f64 {
    const TABLE: [f64; 30] = [
        12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
        2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
        2.052, 2.048, 2.045, 2.042,
    ];
    if df == 0 {
        0.0
    } else if df <= 30 {
        TABLE[df - 1]
    } else {
        1.96
    }
}

impl Stats {
    pub fn of(xs: &[f64]) -> Stats {
        Self::with_rss(xs, &[])
    }

    pub fn with_rss(xs: &[f64], rss_samples: &[u64]) -> Stats {
        if xs.is_empty() {
            return Stats::default();
        }
        let mut v = xs.to_vec();
        v.sort_by(f64::total_cmp);
        let n = v.len() as f64;
        let mean = v.iter().sum::<f64>() / n;
        let median = if v.len() % 2 == 1 {
            v[v.len() / 2]
        } else {
            (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
        };
        let stddev = if v.len() > 1 {
            (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
        } else {
            0.0
        };
        let half_ci = if v.len() > 1 {
            t_crit_95(v.len() - 1) * stddev / n.sqrt()
        } else {
            0.0
        };
        let cv_pct = if mean > 0.0 {
            stddev / mean * 100.0
        } else {
            0.0
        };
        let peak_rss_kb = if rss_samples.is_empty() {
            None
        } else {
            let mut r = rss_samples.to_vec();
            r.sort_unstable();
            Some(r[r.len() / 2])
        };
        Stats {
            median,
            mean,
            min: v[0],
            max: v[v.len() - 1],
            stddev,
            ci95_low: (mean - half_ci).max(0.0),
            ci95_high: mean + half_ci,
            cv_pct,
            peak_rss_kb,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BenchResult {
    pub lang: Lang,
    pub command: Vec<String>,
    pub runs: usize,
    pub ports: Vec<String>,
    pub before: Stats,
    pub after: Stats,
    pub speedup: f64,
    pub noisy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p_value: Option<f64>,
    /// First line where the app's output with the Rust ports differs from the original's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_diff: Option<String>,
}

/// Standard normal survival function approximation (`erfc(z / sqrt(2))`) for two-sided p-value.
fn two_sided_normal_p(z: f64) -> f64 {
    let x = z.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let poly = t
        * (0.254829592
            + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))));
    (poly * (-x * x).exp()).clamp(0.0, 1.0)
}

/// Two-sided Mann-Whitney U p-value comparing two sample distributions (with tie correction).
pub fn mann_whitney_p(a: &[f64], b: &[f64]) -> Option<f64> {
    let (n1, n2) = (a.len(), b.len());
    if n1 < 2 || n2 < 2 {
        return None;
    }
    let mut u = 0.0f64;
    for &x in a {
        for &y in b {
            if x < y {
                u += 1.0;
            } else if (x - y).abs() <= f64::EPSILON {
                u += 0.5;
            }
        }
    }
    let mut combined = Vec::with_capacity(n1 + n2);
    combined.extend_from_slice(a);
    combined.extend_from_slice(b);
    combined.sort_by(f64::total_cmp);

    let mut tie_sum = 0.0f64;
    let mut i = 0;
    while i < combined.len() {
        let mut j = i + 1;
        while j < combined.len() && (combined[j] - combined[i]).abs() <= f64::EPSILON {
            j += 1;
        }
        let t = (j - i) as f64;
        if t > 1.0 {
            tie_sum += t * t * t - t;
        }
        i = j;
    }

    let (fn1, fn2) = (n1 as f64, n2 as f64);
    let total = fn1 + fn2;
    let total_cubed_minus = total * (total * total - 1.0);
    if tie_sum >= total_cubed_minus {
        return Some(1.0);
    }
    let mean_u = fn1 * fn2 / 2.0;
    let var_u = (fn1 * fn2 / 12.0) * ((total + 1.0) - tie_sum / total_cubed_minus);
    if var_u <= 0.0 {
        return Some(1.0);
    }
    let sigma_u = var_u.sqrt();
    let z = ((u - mean_u).abs() - 0.5).max(0.0) / sigma_u;
    Some(two_sided_normal_p(z))
}

struct RunOutput {
    secs: f64,
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    peak_rss_kb: Option<u64>,
}

#[cfg(all(
    unix,
    target_pointer_width = "64",
    any(target_os = "linux", target_os = "macos")
))]
fn spawn_and_time_with_rss(cmd: &mut Command) -> Result<RunOutput> {
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;

    #[repr(C)]
    struct RUsage {
        // Oversized buffer (512 bytes on LP64) so musl's 272-byte `struct rusage`
        // (with `__reserved[16]`) never overflows stack; `ru_maxrss` is at index 4.
        data: [std::ffi::c_long; 64],
    }

    unsafe extern "C" {
        fn wait4(pid: i32, status: *mut i32, options: i32, rusage: *mut RUsage) -> i32;
    }

    let start = Instant::now();
    let mut child = cmd.spawn().context("running the app")?;
    let pid = child.id() as i32;
    drop(child.stdin.take());
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let (stdout_buf, stderr_buf, raw_status, rss_kb) = std::thread::scope(|s| -> Result<_> {
        let t_out = s.spawn(move || {
            let mut buf = Vec::new();
            if let Some(ref mut p) = stdout_pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        });
        let t_err = s.spawn(move || {
            let mut buf = Vec::new();
            if let Some(ref mut p) = stderr_pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        });

        let mut status: i32 = 0;
        let mut rusage = RUsage { data: [0; 64] };
        let mut waited;
        loop {
            // SAFETY: `pid` is our live child process; `status` and `rusage` are valid stack targets
            // with sufficient capacity for glibc, musl, and macOS `struct rusage`.
            waited = unsafe { wait4(pid, &mut status, 0, &mut rusage) };
            if waited == -1 && std::io::Error::last_os_error().raw_os_error() == Some(4) {
                continue;
            }
            break;
        }
        let out_bytes = t_out.join().unwrap_or_default();
        let err_bytes = t_err.join().unwrap_or_default();
        ensure!(
            waited == pid,
            "wait4 failed: {}",
            std::io::Error::last_os_error()
        );
        let raw_rss = rusage.data[4];
        let rss = if raw_rss > 0 {
            if cfg!(target_os = "macos") {
                Some((raw_rss as u64) / 1024)
            } else {
                Some(raw_rss as u64)
            }
        } else {
            None
        };
        Ok((out_bytes, err_bytes, status, rss))
    })?;
    let took = start.elapsed().as_secs_f64();
    std::mem::forget(child);
    let status = std::process::ExitStatus::from_raw(raw_status);
    Ok(RunOutput {
        secs: took,
        status,
        stdout: stdout_buf,
        stderr: stderr_buf,
        peak_rss_kb: rss_kb,
    })
}

#[cfg(not(all(
    unix,
    target_pointer_width = "64",
    any(target_os = "linux", target_os = "macos")
)))]
fn spawn_and_time_with_rss(cmd: &mut Command) -> Result<RunOutput> {
    let start = Instant::now();
    let out = cmd.output().context("running the app")?;
    let took = start.elapsed().as_secs_f64();
    Ok(RunOutput {
        secs: took,
        status: out.status,
        stdout: out.stdout,
        stderr: out.stderr,
        peak_rss_kb: None,
    })
}

/// One run of the app: how long it took, what it printed, and peak RSS in KiB.
fn time_once(
    ctx: &Ctx,
    python: &str,
    args: &[String],
    original: bool,
) -> Result<(f64, Vec<u8>, Option<u64>)> {
    let mut cmd = Command::new(python);
    cmd.args(args)
        .current_dir(&ctx.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if original {
        cmd.env(DISABLE_ENV, "1");
        cmd.env("FNR_DISABLE", "1");
    } else {
        cmd.env_remove(DISABLE_ENV);
        cmd.env_remove("FNR_DISABLE");
    }
    let out = spawn_and_time_with_rss(&mut cmd)?;
    ensure!(
        out.status.success(),
        "the app failed ({}) with the {} version:\n{}",
        out.status,
        if original {
            ctx.state.lang.label()
        } else {
            "Rust"
        },
        util::tail(&String::from_utf8_lossy(&out.stderr), 20)
    );
    Ok((out.secs, out.stdout, out.peak_rss_kb))
}

/// The first line that differs between two outputs, if any.
pub fn first_difference(original: &[u8], rust: &[u8]) -> Option<String> {
    if original == rust {
        return None;
    }
    let (a, b) = (
        String::from_utf8_lossy(original),
        String::from_utf8_lossy(rust),
    );
    let (mut a_lines, mut b_lines) = (a.lines(), b.lines());
    for n in 1.. {
        match (a_lines.next(), b_lines.next()) {
            (Some(x), Some(y)) if x == y => continue,
            (None, None) => {
                return Some("the outputs differ only in line endings or a final newline".into());
            }
            (x, y) => {
                let clip = |l: Option<&str>| match l {
                    None => "(no line)".to_string(),
                    Some(l) if l.chars().count() > 80 => {
                        format!("{}…", l.chars().take(79).collect::<String>())
                    }
                    Some(l) => l.to_string(),
                };
                return Some(format!("line {n}: {:?} became {:?}", clip(x), clip(y)));
            }
        }
    }
    unreachable!()
}

pub fn run(ctx: &Ctx, runs: usize) -> Result<BenchResult> {
    let (python, args) = ctx.state.lang.bench_command(ctx)?;
    let (python, args) = (python.as_str(), args.as_slice());
    let ports: Vec<String> = ctx
        .state
        .ports
        .values()
        .filter(|p| p.enabled)
        .map(|p| p.id.clone())
        .collect();
    if ports.is_empty() {
        ui::warn("no ports are enabled yet, so before and after are the same code");
    }
    ui::step(format!(
        "timing {} ({runs} runs each, {} vs Rust, interleaved)",
        ui::bold(ctx.state.command.join(" ")),
        ctx.state.lang.label()
    ));
    // the warm-up runs double as a check that the ports don't change what the app prints
    let (_, original_out, _) = time_once(ctx, python, args, true)?;
    let (_, rust_out, _) = time_once(ctx, python, args, false)?;
    let output_diff = first_difference(&original_out, &rust_out);
    let (mut before, mut after) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    let (mut before_rss, mut after_rss) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    for i in 0..runs {
        if i % 2 == 0 {
            let (t1, _, r1) = time_once(ctx, python, args, true)?;
            before.push(t1);
            if let Some(kb) = r1 {
                before_rss.push(kb);
            }
            let (t2, _, r2) = time_once(ctx, python, args, false)?;
            after.push(t2);
            if let Some(kb) = r2 {
                after_rss.push(kb);
            }
        } else {
            let (t2, _, r2) = time_once(ctx, python, args, false)?;
            after.push(t2);
            if let Some(kb) = r2 {
                after_rss.push(kb);
            }
            let (t1, _, r1) = time_once(ctx, python, args, true)?;
            before.push(t1);
            if let Some(kb) = r1 {
                before_rss.push(kb);
            }
        }
    }
    let p_value = mann_whitney_p(&before, &after);
    let (before, after) = (
        Stats::with_rss(&before, &before_rss),
        Stats::with_rss(&after, &after_rss),
    );
    let speedup = if after.median > 0.0 {
        before.median / after.median
    } else {
        0.0
    };
    let noisy = before.cv_pct > 5.0 || after.cv_pct > 5.0;
    Ok(BenchResult {
        lang: ctx.state.lang,
        command: ctx.state.command.clone(),
        runs,
        ports,
        before,
        after,
        speedup,
        noisy,
        p_value,
        output_diff,
    })
}

#[cfg(test)]
mod tests {
    use super::{Stats, first_difference, mann_whitney_p, spawn_and_time_with_rss};
    use std::process::{Command, Stdio};

    #[test]
    fn stats() {
        let s = Stats::with_rss(&[3.0, 1.0, 2.0, 10.0], &[400, 100, 250]);
        assert_eq!(s.median, 2.5);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 10.0);
        assert_eq!(s.mean, 4.0);
        assert_eq!(s.peak_rss_kb, Some(250));
        // Sample stddev of [1, 2, 3, 10] is sqrt(50 / 3) ~= 4.0825.
        assert!((s.stddev - (50.0_f64 / 3.0).sqrt()).abs() < 1e-9);
        assert!((s.cv_pct - (s.stddev / s.mean * 100.0)).abs() < 1e-9);
        assert!(s.ci95_low < s.mean && s.ci95_high > s.mean);
        assert_eq!(Stats::of(&[5.0]).median, 5.0);
        assert_eq!(Stats::of(&[5.0]).stddev, 0.0);
        let p = mann_whitney_p(&[10.0, 11.0, 12.0, 10.5], &[1.0, 1.1, 1.2, 0.9]).unwrap();
        assert!(p < 0.05, "p={p}");
        assert_eq!(
            mann_whitney_p(&[1.0, 1.0, 1.0], &[1.0, 1.0, 1.0]),
            Some(1.0)
        );
        let tied_p = mann_whitney_p(&[1.0, 1.0, 2.0, 3.0], &[2.0, 2.0, 4.0, 5.0]).unwrap();
        assert!((0.0..=1.0).contains(&tied_p));
    }

    #[test]
    fn output_difference() {
        assert_eq!(first_difference(b"a\nb\n", b"a\nb\n"), None);
        assert_eq!(
            first_difference(b"a\n1.0\n", b"a\n1\n").as_deref(),
            Some(r#"line 2: "1.0" became "1""#)
        );
        assert_eq!(
            first_difference(b"a\nb\n", b"a\n").as_deref(),
            Some(r#"line 2: "b" became "(no line)""#)
        );
        assert!(
            first_difference(b"a\r\n", b"a\n")
                .unwrap()
                .contains("line endings")
        );
    }

    #[test]
    fn measures_child_rss_without_deadlock() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo ok"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let run = spawn_and_time_with_rss(&mut cmd).unwrap();
        assert!(run.status.success());
        assert_eq!(run.stdout, b"ok\n");
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(
                run.peak_rss_kb.is_some_and(|kb| kb > 0),
                "rss={:?}",
                run.peak_rss_kb
            );
        }
    }
}
