"use strict";

const crypto = require("node:crypto");
const fs = require("node:fs");
const https = require("node:https");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const { resolveBinary, ensureExecutable } = require("./bin/fast-n-rust.js");
const pkg = require("./package.json");

const TARGET_MAP = {
  "linux-x64": "x86_64-unknown-linux-gnu",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "win32-x64": "x86_64-pc-windows-msvc",
};

function releaseAssetName(platform = process.platform, arch = process.arch, version = pkg.version) {
  const target = TARGET_MAP[`${platform}-${arch}`];
  if (!target) return null;
  const ext = platform === "win32" ? "zip" : "tar.gz";
  return `fast-n-rust-v${version}-${target}.${ext}`;
}

function parseChecksum(checksumsText, filename) {
  for (const line of checksumsText.split(/\r?\n/)) {
    const m = line.trim().match(/^([a-fA-F0-9]{64})\s+\*?(.+)$/);
    if (m && path.basename(m[2].trim()) === filename) {
      return m[1].toLowerCase();
    }
  }
  return null;
}

function verifySha256(filePath, expectedHex) {
  const hash = crypto.createHash("sha256");
  hash.update(fs.readFileSync(filePath));
  return hash.digest("hex").toLowerCase() === expectedHex.toLowerCase();
}

function downloadBuffer(url, redirects = 0) {
  return new Promise((resolve, reject) => {
    if (redirects > 5) {
      reject(new Error(`too many redirects for ${url}`));
      return;
    }
    https
      .get(url, { headers: { "User-Agent": "fast-n-rust-npm-installer" } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          resolve(downloadBuffer(res.headers.location, redirects + 1));
          return;
        }
        if (res.statusCode !== 200) {
          res.resume();
          reject(new Error(`HTTP ${res.statusCode} for ${url}`));
          return;
        }
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve(Buffer.concat(chunks)));
        res.on("error", reject);
      })
      .on("error", reject);
  });
}

async function install(env = process.env) {
  if (env.FNR_SKIP_DOWNLOAD === "1") {
    return { status: "skipped", reason: "FNR_SKIP_DOWNLOAD=1" };
  }

  const existing = resolveBinary(env);
  if (existing.includes(path.sep) && fs.existsSync(existing)) {
    return { status: "skipped", reason: `already available at ${existing}` };
  }

  const asset = releaseAssetName();
  if (!asset) {
    console.warn(`[fast-n-rust] No prebuilt binary for ${process.platform}-${process.arch}; build with: cargo install --path crates/cli`);
    return { status: "unsupported" };
  }

  const repo = env.FNR_REPO || "terrarich2004/fast-n-rust-everything";
  const baseUrl = (
    env.FNR_RELEASE_BASE_URL ||
    `https://github.com/${repo}/releases/download/v${pkg.version}`
  ).replace(/\/+$/, "");

  const destDir = path.join(__dirname, "native", `${process.platform}-${process.arch}`);
  fs.mkdirSync(destDir, { recursive: true });
  const archivePath = path.join(destDir, asset);

  try {
    const buf = await downloadBuffer(`${baseUrl}/${asset}`);
    fs.writeFileSync(archivePath, buf);

    try {
      const sums = (await downloadBuffer(`${baseUrl}/checksums.txt`)).toString("utf8");
      const expected = parseChecksum(sums, asset);
      if (expected && !verifySha256(archivePath, expected)) {
        throw new Error(`SHA-256 mismatch for ${asset}`);
      }
    } catch (sumErr) {
      if (String(sumErr.message).includes("SHA-256 mismatch")) {
        throw sumErr;
      }
    }

    const extract =
      process.platform === "win32"
        ? spawnSync("powershell", ["-NoProfile", "-Command", `Expand-Archive -Force '${archivePath}' '${destDir}'`], { stdio: "pipe" })
        : spawnSync("tar", ["-xzf", archivePath, "-C", destDir], { stdio: "pipe" });
    if (extract.status !== 0) {
      throw new Error(`archive extraction failed: ${extract.stderr?.toString() || extract.status}`);
    }
    const exe = path.join(destDir, process.platform === "win32" ? "fast-n-rust.exe" : "fast-n-rust");
    if (fs.existsSync(exe)) {
      ensureExecutable(exe);
    }
    fs.rmSync(archivePath, { force: true });
    return { status: "installed", path: exe };
  } catch (err) {
    fs.rmSync(archivePath, { force: true });
    console.warn(`[fast-n-rust] Prebuilt download skipped (${err.message}). Install from source with: cargo install --path crates/cli`);
    return { status: "fallback", error: err.message };
  }
}

if (require.main === module) {
  install().catch((err) => {
    console.warn(`[fast-n-rust] postinstall non-fatal warning: ${err.message}`);
  });
}

module.exports = { install, parseChecksum, releaseAssetName, verifySha256 };
