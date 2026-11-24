//! Small helpers: paths, processes, number formatting.

use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Path from directory from to to, both absolute.
pub fn relative(from: &Path, to: &Path) -> PathBuf {
    let a: Vec<Component> = from.components().collect();
    let b: Vec<Component> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..a.len() {
        out.push("..");
    }
    for c in &b[common..] {
        out.push(c);
    }
    out
}

/// "python3" from "python3", "/usr/bin/python3" or "C:\\Python\\python.exe", on any OS.
pub fn program_stem(program: &str) -> String {
    let name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_string()
}

pub fn forward_slashes(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// A name safe for Rust crates and Python modules.
pub fn slug(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

pub fn output(cmd: &mut Command, what: &str) -> Result<Output> {
    cmd.output().with_context(|| {
        format!(
            "couldn't start {what} ({})",
            cmd.get_program().to_string_lossy()
        )
    })
}

pub fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Build a shell command (`sh -c` on Unix, `cmd /C` on Windows) in `root`.
pub fn shell_command(root: &Path, script: &str) -> Command {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", script]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    };
    cmd.current_dir(root);
    cmd
}

/// Background load-generator child process for timed server/bot profiling & recording.
pub struct LoadDriver {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
}

impl LoadDriver {
    pub fn finish(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.child.lock()
            && let Some(ref mut ch) = *guard
        {
            let _ = ch.kill();
            let _ = ch.wait();
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for LoadDriver {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Spawn `cmd` in the background after a short server-startup delay, retrying brief
/// connection failures while the server binds its socket.
pub fn spawn_load_command(root: &Path, cmd: Option<&str>) -> Result<Option<LoadDriver>> {
    let Some(raw) = cmd.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let root = root.to_path_buf();
    let script = raw.to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let stop_bg = Arc::clone(&stop);
    let child_bg = Arc::clone(&child_slot);

    let handle = std::thread::spawn(move || {
        // Wait briefly for the target server to bind its listening port.
        for _ in 0..5 {
            if stop_bg.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for attempt in 0..4 {
            if stop_bg.load(Ordering::Relaxed) {
                return;
            }
            let started = Instant::now();
            let mut command = shell_command(&root, &script);
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let Ok(spawned) = command.spawn() else {
                return;
            };
            {
                let mut guard = child_bg.lock().unwrap_or_else(|e| e.into_inner());
                *guard = Some(spawned);
            }
            loop {
                if stop_bg.load(Ordering::Relaxed) {
                    let mut guard = child_bg.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(ref mut ch) = *guard {
                        let _ = ch.kill();
                        let _ = ch.wait();
                    }
                    *guard = None;
                    return;
                }
                let exit = {
                    let mut guard = child_bg.lock().unwrap_or_else(|e| e.into_inner());
                    match guard.as_mut().map(|ch| ch.try_wait()) {
                        Some(Ok(Some(st))) => {
                            *guard = None;
                            Some(Ok(st))
                        }
                        Some(Ok(None)) => None,
                        Some(Err(e)) => {
                            *guard = None;
                            Some(Err(e))
                        }
                        None => Some(Err(std::io::Error::other("missing child"))),
                    }
                };
                match exit {
                    Some(Ok(st)) if st.success() => return,
                    Some(Ok(_))
                        if attempt < 3 && started.elapsed() < Duration::from_millis(1200) =>
                    {
                        std::thread::sleep(Duration::from_millis(250));
                        break;
                    }
                    Some(_) => return,
                    None => std::thread::sleep(Duration::from_millis(30)),
                }
            }
        }
    });

    Ok(Some(LoadDriver {
        stop,
        child: child_slot,
        thread: Some(handle),
    }))
}

/// Append standard Rust & local toolchain directories to `PATH` once at CLI startup
/// so GUI IDEs (Cursor, Windsurf, Antigravity, Claude Desktop) spawning `fnr mcp` with a
/// stripped PATH can still find `cargo`, `rustc`, `cc`, and `fnr`.
pub fn augment_toolchain_paths(home: Option<&Path>) {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(cargo_home).join("bin"));
    } else if let Some(h) = home {
        candidates.push(h.join(".cargo").join("bin"));
    }
    if let Some(h) = home {
        candidates.push(h.join(".local").join("bin"));
    }
    if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from("/opt/homebrew/bin"));
        candidates.push(PathBuf::from("/usr/local/bin"));
    }

    let current_os = std::env::var_os("PATH").unwrap_or_default();
    let mut existing: Vec<PathBuf> = std::env::split_paths(&current_os).collect();
    let mut changed = false;
    for dir in candidates {
        if dir.is_dir() && !existing.iter().any(|p| p == &dir) {
            existing.push(dir);
            changed = true;
        }
    }
    if changed && let Ok(joined) = std::env::join_paths(existing) {
        // SAFETY: called once on the main thread at CLI startup before any threads are spawned.
        unsafe { std::env::set_var("PATH", joined) };
    }
}

pub fn secs(s: f64) -> String {
    if s >= 1.0 {
        format!("{s:.2}s")
    } else if s >= 1e-3 {
        format!("{:.0}ms", s * 1e3)
    } else {
        format!("{:.0}µs", s * 1e6)
    }
}

pub fn nanos(ns: f64) -> String {
    if ns < 1e3 {
        format!("{ns:.0}ns")
    } else if ns < 1e6 {
        format!("{:.1}µs", ns / 1e3)
    } else {
        format!("{:.1}ms", ns / 1e6)
    }
}

pub fn times(x: f64) -> String {
    if x >= 10.0 {
        format!("{x:.0}×")
    } else {
        format!("{x:.1}×")
    }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        let r = relative(Path::new("/a/b/c"), Path::new("/a/.fast/lib"));
        assert_eq!(forward_slashes(&r), "../../.fast/lib");
        assert_eq!(
            forward_slashes(&relative(Path::new("/a"), Path::new("/a/x"))),
            "x"
        );
    }

    #[test]
    fn formatting() {
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(12), "12");
        assert_eq!(times(87.4), "87×");
        assert_eq!(times(3.21), "3.2×");
        assert_eq!(slug("text.utils.Lev-1"), "text_utils_lev_1");
        assert_eq!(nanos(350.0), "350ns");
        assert_eq!(secs(0.25), "250ms");
    }
}
