//! Every outside program runs with stdin closed, in its own process group, under a time limit.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const KEEP: usize = 1 << 20;

pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// `RUST_ANYTHING_COMMAND_TIMEOUT` seconds when set, else `default`.
pub fn limit(default: Duration) -> Duration {
    std::env::var("RUST_ANYTHING_COMMAND_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .and_then(seconds)
        .unwrap_or(default)
}

/// A duration from a number of seconds; `None` when the value cannot be one (not positive, not
/// finite, or too large for `Duration` — a plain conversion panics on `1e20`).
pub fn seconds(v: f64) -> Option<Duration> {
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(v).ok()
}

pub const TESTS: Duration = Duration::from_secs(30 * 60);
pub const BUILD: Duration = Duration::from_secs(30 * 60);
pub const QUICK: Duration = Duration::from_secs(60);

/// Runs `cmd` and captures its output (the last MiB of each stream).
pub fn output(cmd: &mut Command, default: Duration) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn(cmd)?;
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let status = wait(&mut child, cmd, limit(default))?;
    Ok(Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// Runs `cmd` with the output streams it was given.
pub fn status(cmd: &mut Command, default: Duration) -> Result<ExitStatus> {
    let mut child = spawn(cmd)?;
    wait(&mut child, cmd, limit(default))
}

/// Runs `cmd` with `input` on its stdin and the output streams it was given (an agent reading
/// its prompt).
pub fn status_with_input(cmd: &mut Command, input: &[u8], default: Duration) -> Result<ExitStatus> {
    let mut child = spawn_with(cmd, Stdio::piped())?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        use std::io::Write as _;
        let _ = stdin.write_all(&input);
    });
    let status = wait(&mut child, cmd, limit(default));
    let _ = writer.join();
    status
}

fn spawn(cmd: &mut Command) -> Result<Child> {
    spawn_with(cmd, Stdio::null())
}

fn spawn_with(cmd: &mut Command, stdin: Stdio) -> Result<Child> {
    cmd.stdin(stdin);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    cmd.spawn()
        .with_context(|| format!("running `{}`", shown(cmd)))
}

fn drain(stream: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let Some(mut stream) = stream else {
            return kept;
        };
        let mut buf = [0u8; 8192];
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&buf[..n]);
            if kept.len() > 2 * KEEP {
                kept.drain(..kept.len() - KEEP);
            }
        }
        if kept.len() > KEEP {
            kept.drain(..kept.len() - KEEP);
        }
        kept
    })
}

fn wait(child: &mut Child, cmd: &Command, limit: Duration) -> Result<ExitStatus> {
    let start = Instant::now();
    let mut pause = Duration::from_millis(5);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if start.elapsed() > limit {
            kill(child);
            bail!(
                "`{}` was still running after {}s and was stopped (set RUST_ANYTHING_COMMAND_TIMEOUT to allow longer; a test runner in watch mode never ends, so run it once, e.g. `vitest run`)",
                shown(cmd),
                limit.as_secs()
            );
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(100));
    }
}

fn kill(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn shown(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_value_that_cannot_be_a_duration_falls_back_instead_of_panicking() {
        // `Duration::from_secs_f64(1e20)` panics; an over-large value is ignored, as a bad one is.
        assert_eq!(seconds(1e20), None);
        assert_eq!(seconds(f64::INFINITY), None);
        assert_eq!(seconds(f64::NAN), None);
        assert_eq!(seconds(-1.0), None);
        assert_eq!(seconds(0.0), None);
        assert_eq!(seconds(2.5), Some(Duration::from_secs_f64(2.5)));
    }

    #[cfg(unix)]
    #[test]
    fn a_program_past_its_limit_is_stopped() {
        let start = Instant::now();
        let err = output(
            Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(300),
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(err.contains("still running"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn stdin_is_closed() {
        let out = output(&mut Command::new("cat"), Duration::from_secs(5)).unwrap();
        assert!(out.success());
        assert!(out.stdout.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn output_is_kept_and_capped() {
        let out = output(
            Command::new("sh").args(["-c", "yes | head -c 3000000; echo err >&2"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(out.stdout.len(), KEEP);
        assert_eq!(out.stderr_text(), "err\n");
    }
}
