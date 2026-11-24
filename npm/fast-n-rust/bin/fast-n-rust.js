#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

function defaultPlatformResolve(pkg) {
  return require.resolve(pkg);
}

function resolveBinary(
  env = process.env,
  exists = fs.existsSync,
  platformResolve = defaultPlatformResolve,
  platform = process.platform,
  arch = process.arch
) {
  if (env.FNR_BIN && exists(env.FNR_BIN)) {
    return env.FNR_BIN;
  }

  const exe = platform === "win32" ? "fast-n-rust.exe" : "fast-n-rust";
  const pkgExe = `@fast-n-rust/${platform}-${arch}/bin/${exe}`;
  try {
    const resolved = platformResolve(pkgExe);
    if (resolved && exists(resolved)) {
      return resolved;
    }
  } catch {
    // Optional platform package is not installed; continue fallback chain.
  }

  const bundled = path.join(__dirname, "..", "native", `${platform}-${arch}`, exe);
  if (exists(bundled)) {
    return bundled;
  }

  for (const profile of ["release", "debug"]) {
    const workspaceBin = path.resolve(__dirname, "..", "..", "..", "target", profile, exe);
    if (exists(workspaceBin)) {
      return workspaceBin;
    }
  }

  return exe;
}

function findNativeOnPath(
  exe,
  env = process.env,
  selfPath = __filename,
  exists = fs.existsSync,
  realpath = fs.realpathSync
) {
  const pathVal = env.PATH || env.Path || "";
  if (!pathVal) return null;
  let selfReal = null;
  try {
    selfReal = realpath(selfPath);
  } catch {
    selfReal = path.resolve(selfPath);
  }
  const selfDir = path.dirname(selfReal);
  for (const dir of pathVal.split(path.delimiter)) {
    if (!dir) continue;
    const cand = path.join(dir, exe);
    if (!exists(cand)) continue;
    try {
      const candReal = realpath(cand);
      if (candReal === selfReal || path.dirname(candReal) === selfDir) {
        continue;
      }
    } catch {
      // If realpath fails, still avoid matching self directory directly.
      if (path.resolve(dir) === path.dirname(path.resolve(selfPath))) {
        continue;
      }
    }
    return cand;
  }
  return null;
}

function ensureExecutable(binPath, chmod = fs.chmodSync, platform = process.platform) {
  if (platform !== "win32" && binPath.includes(path.sep)) {
    try {
      chmod(binPath, 0o755);
    } catch {
      // Non-fatal if binary lives on a read-only filesystem or PATH lookup.
    }
  }
}

function failMissingBinary(bin, detail) {
  console.error(`fast-n-rust launcher error (${bin}): ${detail}`);
  console.error("Re-run installer:   npm rebuild fast-n-rust");
  console.error("Or install via Cargo: cargo install --git https://github.com/terrarich2004/fast-n-rust-everything fast-n-rust");
  console.error("Or download binary: https://github.com/terrarich2004/fast-n-rust-everything/releases");
  process.exit(1);
}

function main(argv = process.argv.slice(2)) {
  if (process.env.FNR_LAUNCHER_ACTIVE === "1") {
    failMissingBinary("fast-n-rust", "refusing recursive launcher invocation; native binary is not installed");
  }
  let bin = resolveBinary();
  if (!bin.includes(path.sep)) {
    const resolvedOnPath = findNativeOnPath(bin);
    if (!resolvedOnPath) {
      failMissingBinary(bin, "prebuilt native binary not found locally or on PATH");
    }
    bin = resolvedOnPath;
  }
  ensureExecutable(bin);
  const res = spawnSync(bin, argv, {
    stdio: "inherit",
    env: { ...process.env, FNR_LAUNCHER_ACTIVE: "1" },
  });
  if (res.error) {
    failMissingBinary(bin, res.error.message);
  }
  process.exit(res.status ?? 1);
}

if (require.main === module) {
  main();
}

module.exports = { resolveBinary, findNativeOnPath, ensureExecutable };
