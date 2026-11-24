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

function ensureExecutable(binPath, chmod = fs.chmodSync, platform = process.platform) {
  if (platform !== "win32" && binPath.includes(path.sep)) {
    try {
      chmod(binPath, 0o755);
    } catch {
      // Non-fatal if binary lives on a read-only filesystem or PATH lookup.
    }
  }
}

function main(argv = process.argv.slice(2)) {
  const bin = resolveBinary();
  ensureExecutable(bin);
  const res = spawnSync(bin, argv, { stdio: "inherit" });
  if (res.error) {
    console.error(`fast-n-rust launcher error (${bin}): ${res.error.message}`);
    console.error("Re-run installer:   npm rebuild fast-n-rust");
    console.error("Or install via Cargo: cargo install --git https://github.com/terrarich2004/fast-n-rust-everything fast-n-rust");
    console.error("Or download binary: https://github.com/terrarich2004/fast-n-rust-everything/releases");
    process.exit(1);
  }
  process.exit(res.status ?? 1);
}

if (require.main === module) {
  main();
}

module.exports = { resolveBinary, ensureExecutable };
