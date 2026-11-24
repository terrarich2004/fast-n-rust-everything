//! Terminal output. Silent in MCP mode, where stdout belongs to the protocol.

use std::cell::RefCell;
use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);
static COLOR: OnceLock<bool> = OnceLock::new();

type Progress = Box<dyn FnMut(&str) + Send>;
thread_local! {
    static PROGRESS: RefCell<Option<Progress>> = const { RefCell::new(None) };
}

/// While set, every step() line on the current thread is passed here too (MCP progress notifications).
pub fn set_progress(progress: Option<Progress>) {
    PROGRESS.with(|slot| {
        *slot.borrow_mut() = progress;
    });
}

pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

fn color() -> bool {
    *COLOR.get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

fn paint(code: &str, s: impl Display) -> String {
    if color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: impl Display) -> String {
    paint("1", s)
}
pub fn dim(s: impl Display) -> String {
    paint("2", s)
}
pub fn green(s: impl Display) -> String {
    paint("32", s)
}
pub fn red(s: impl Display) -> String {
    paint("31", s)
}
pub fn yellow(s: impl Display) -> String {
    paint("33", s)
}
pub fn rust(s: impl Display) -> String {
    paint("38;5;208", s)
}

pub fn line(s: impl Display) {
    if !quiet() {
        println!("{s}");
    }
}

pub fn step(s: impl Display) {
    let text = s.to_string();
    PROGRESS.with(|slot| {
        if let Some(progress) = slot.borrow_mut().as_mut() {
            progress(&text);
        }
    });
    line(format!("{} {text}", rust("▸")));
}

pub fn ok(s: impl Display) {
    line(format!("{} {s}", green("✓")));
}

pub fn warn(s: impl Display) {
    if !quiet() {
        eprintln!("{} {s}", yellow("!"));
    }
}

pub fn error(s: impl Display) {
    eprintln!("{} {s}", red("✗"));
}

pub fn banner() {
    line(format!(
        "{} {}",
        rust(bold("⚡ fast-n-rust")),
        dim(concat!("v", env!("CARGO_PKG_VERSION")))
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, Mutex};

    #[test]
    fn progress_callbacks_are_isolated_per_thread() {
        set_quiet(true);
        let log_a = Arc::new(Mutex::new(Vec::<String>::new()));
        let log_b = Arc::new(Mutex::new(Vec::<String>::new()));
        let barrier = Arc::new(Barrier::new(2));

        let la = Arc::clone(&log_a);
        let ba = Arc::clone(&barrier);
        let ha = std::thread::spawn(move || {
            set_progress(Some(Box::new(move |msg| {
                la.lock().unwrap().push(msg.to_string());
            })));
            ba.wait();
            step("from-a-1");
            step("from-a-2");
            set_progress(None);
        });

        let lb = Arc::clone(&log_b);
        let bb = Arc::clone(&barrier);
        let hb = std::thread::spawn(move || {
            set_progress(Some(Box::new(move |msg| {
                lb.lock().unwrap().push(msg.to_string());
            })));
            bb.wait();
            step("from-b-1");
            set_progress(None);
        });

        ha.join().unwrap();
        hb.join().unwrap();
        assert_eq!(*log_a.lock().unwrap(), vec!["from-a-1", "from-a-2"]);
        assert_eq!(*log_b.lock().unwrap(), vec!["from-b-1"]);
    }
}
