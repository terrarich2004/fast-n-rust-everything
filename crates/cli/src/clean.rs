//! `clean`: remove shared Cargo build caches, `.fast/target`, `rust/*/target`, or reset `.fast/`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Serialize;

#[derive(Debug, Clone, Default)]
pub struct CleanOptions {
    pub cache: bool,
    pub ports: bool,
    pub rust: bool,
    pub all: bool,
    pub force: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanTarget {
    pub kind: &'static str,
    pub path: PathBuf,
    pub bytes: u64,
    pub human_size: String,
    pub removed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanOutcome {
    pub dry_run: bool,
    pub reclaimed_bytes: u64,
    pub reclaimed_human: String,
    pub targets: Vec<CleanTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_cargo_target: Option<PathBuf>,
}

pub fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries.flatten().map(|entry| dir_size(&entry.path())).sum()
}

pub fn human_bytes(bytes: u64) -> String {
    fnr_migrate::cache::human(bytes)
}

fn rust_target_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let rust_dir = root.join("rust");
    if let Ok(entries) = fs::read_dir(&rust_dir) {
        for entry in entries.flatten() {
            let p = entry.path().join("target");
            if p.is_dir() {
                out.push(p);
            }
        }
    }
    out
}

fn push_if_exists(
    targets: &mut Vec<CleanTarget>,
    kind: &'static str,
    path: PathBuf,
    seen: &mut Vec<PathBuf>,
) {
    if !path.exists() {
        return;
    }
    let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
    if seen.iter().any(|s| s == &canon) {
        return;
    }
    seen.push(canon);
    let bytes = dir_size(&path);
    targets.push(CleanTarget {
        kind,
        path,
        bytes,
        human_size: human_bytes(bytes),
        removed: false,
    });
}

pub fn clean(root: &Path, opts: &CleanOptions) -> Result<CleanOutcome> {
    clean_with_shared(
        root,
        opts,
        fnr_migrate::cache::shared_dir(),
        fnr_migrate::cache::shared_dir_from_cargo_env(),
    )
}

fn clean_with_shared(
    root: &Path,
    opts: &CleanOptions,
    shared_dir: PathBuf,
    shared_from_cargo_env: bool,
) -> Result<CleanOutcome> {
    let any_flag = opts.cache || opts.ports || opts.rust || opts.all;
    let do_cache = opts.cache || opts.all || !any_flag;
    let do_rust = opts.rust || opts.all;
    let do_ports = opts.ports;

    if do_ports
        && !opts.force
        && root.join(".fast").join("state.json").is_file()
        && let Ok(ctx) = fnr_fast::state::Ctx::open(root)
    {
        let active: Vec<&str> = ctx
            .state
            .ports
            .values()
            .filter(|p| p.enabled)
            .map(|p| p.id.as_str())
            .collect();
        if !active.is_empty() {
            bail!(
                "cannot delete `.fast` while ports are still enabled ({}). Run `fnr disable --all` first, or pass `--force`.",
                active.join(", ")
            );
        }
    }

    let mut seen = Vec::new();
    let mut targets = Vec::new();
    let mut skipped_cargo_target = None;

    if do_cache {
        if shared_from_cargo_env && !opts.force {
            if shared_dir.exists() {
                skipped_cargo_target = Some(shared_dir);
            }
        } else {
            push_if_exists(&mut targets, "shared-cache", shared_dir, &mut seen);
        }
        if !do_ports {
            push_if_exists(
                &mut targets,
                "fast-target",
                root.join(".fast").join("target"),
                &mut seen,
            );
        }
    }

    if do_rust {
        for p in rust_target_dirs(root) {
            push_if_exists(&mut targets, "rust-target", p, &mut seen);
        }
    }

    if do_ports {
        push_if_exists(&mut targets, "fast-state", root.join(".fast"), &mut seen);
    }

    let mut reclaimed = 0u64;
    for t in &mut targets {
        reclaimed = reclaimed.saturating_add(t.bytes);
        if !opts.dry_run {
            if t.path.is_dir() {
                fs::remove_dir_all(&t.path)?;
            } else if t.path.exists() {
                fs::remove_file(&t.path)?;
            }
            t.removed = true;
        }
    }

    Ok(CleanOutcome {
        dry_run: opts.dry_run,
        reclaimed_bytes: reclaimed,
        reclaimed_human: human_bytes(reclaimed),
        targets,
        skipped_cargo_target,
    })
}

pub fn text(out: &CleanOutcome) -> String {
    let mut s = String::new();
    if out.targets.is_empty() && out.skipped_cargo_target.is_none() {
        return "Nothing to clean.\n".to_string();
    }
    let verb = if out.dry_run {
        "Would remove"
    } else {
        "Removed"
    };
    for t in &out.targets {
        s.push_str(&format!(
            "  {verb} [{}] {} ({})\n",
            t.kind,
            t.path.display(),
            t.human_size
        ));
    }
    if let Some(ref p) = out.skipped_cargo_target {
        s.push_str(&format!(
            "  Skipped [shared-cache] {} (from CARGO_TARGET_DIR; pass --force to remove)\n",
            p.display()
        ));
    }
    if !out.targets.is_empty() {
        let summary = if out.dry_run {
            "Would reclaim"
        } else {
            "Reclaimed"
        };
        s.push_str(&format!("{summary} {}.\n", out.reclaimed_human));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fnr-clean-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn clean_dry_run_and_remove_rust_target() {
        let root = scratch("rust-target");
        let t = root.join("rust/demo-rs/target/release");
        fs::create_dir_all(&t).unwrap();
        fs::write(t.join("artifact.bin"), vec![0u8; 4096]).unwrap();

        let dry = clean(
            &root,
            &CleanOptions {
                rust: true,
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(dry.reclaimed_bytes >= 4096);
        assert!(t.exists(), "dry-run must not delete");

        let real = clean(
            &root,
            &CleanOptions {
                rust: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(real.reclaimed_bytes >= 4096);
        assert!(!root.join("rust/demo-rs/target").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_ports_refuses_when_port_is_enabled_unless_forced() {
        let root = scratch("enabled-guard");
        let fast = root.join(".fast");
        fs::create_dir_all(&fast).unwrap();
        let state = serde_json::json!({
            "version": 1,
            "ports": {
                "app.f": {
                    "id": "app.f",
                    "file": "app.py",
                    "module": "app",
                    "func": "f",
                    "ext": "fast_app_f",
                    "crate_dir": ".fast/ports/app.f",
                    "recorded": 10,
                    "calls_seen": 10,
                    "verify": null,
                    "enabled": true
                }
            }
        });
        fs::write(
            fast.join("state.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();

        let err = clean(
            &root,
            &CleanOptions {
                ports: true,
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("disable --all"), "{err}");
        assert!(fast.exists());

        clean(
            &root,
            &CleanOptions {
                ports: true,
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!fast.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_skips_cargo_target_dir_without_force_and_removes_with_force() {
        let root = scratch("cargo-env-guard");
        let shared = root.join("shared-cargo-target");
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join("marker.bin"), vec![1u8; 1024]).unwrap();

        let no_force = clean_with_shared(
            &root,
            &CleanOptions {
                cache: true,
                ..Default::default()
            },
            shared.clone(),
            true,
        )
        .unwrap();
        assert!(
            shared.exists(),
            "CARGO_TARGET_DIR must not be deleted without --force"
        );
        assert_eq!(no_force.skipped_cargo_target.as_ref(), Some(&shared));
        assert!(text(&no_force).contains("CARGO_TARGET_DIR"));

        let forced = clean_with_shared(
            &root,
            &CleanOptions {
                cache: true,
                force: true,
                ..Default::default()
            },
            shared.clone(),
            true,
        )
        .unwrap();
        assert!(!shared.exists(), "--force must remove CARGO_TARGET_DIR");
        assert!(forced.reclaimed_bytes >= 1024);
        assert!(forced.skipped_cargo_target.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_cache_and_ports_does_not_double_count_fast_target() {
        let root = scratch("no-double-count");
        let fast_target = root.join(".fast/target");
        fs::create_dir_all(&fast_target).unwrap();
        fs::write(fast_target.join("blob.bin"), vec![2u8; 4096]).unwrap();

        let out = clean_with_shared(
            &root,
            &CleanOptions {
                cache: true,
                ports: true,
                dry_run: true,
                ..Default::default()
            },
            root.join("missing-shared"),
            false,
        )
        .unwrap();
        assert_eq!(out.targets.len(), 1);
        assert_eq!(out.targets[0].kind, "fast-state");
        assert_eq!(out.reclaimed_bytes, 4096);
        let _ = fs::remove_dir_all(&root);
    }
}
