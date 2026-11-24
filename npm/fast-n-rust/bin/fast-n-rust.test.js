"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { resolveBinary, ensureExecutable } = require("./fast-n-rust.js");
const { install, parseChecksum, releaseAssetName, verifySha256 } = require("../install.js");

test("resolveBinary honors FNR_BIN override before everything else", () => {
  const custom = "/opt/fnr/fast-n-rust";
  const got = resolveBinary({ FNR_BIN: custom }, (p) => p === custom);
  assert.equal(got, custom);
});

test("resolveBinary prefers optional platform package over native/ and target/", () => {
  const pkgBin = "/node_modules/@fast-n-rust/linux-x64/bin/fast-n-rust";
  const got = resolveBinary(
    {},
    (p) => p === pkgBin,
    (id) => (id === "@fast-n-rust/linux-x64/bin/fast-n-rust" ? pkgBin : null),
    "linux",
    "x64"
  );
  assert.equal(got, pkgBin);
});

test("resolveBinary falls back to bundled native binary before workspace target/", () => {
  const exe = process.platform === "win32" ? "fast-n-rust.exe" : "fast-n-rust";
  const expected = path.join(__dirname, "..", "native", `${process.platform}-${process.arch}`, exe);
  const got = resolveBinary({}, (p) => p === expected, () => {
    throw new Error("not installed");
  });
  assert.equal(got, expected);
});

test("resolveBinary checks workspace target/release before target/debug and PATH", () => {
  const releaseBin = path.resolve(__dirname, "..", "..", "..", "target", "release", "fast-n-rust");
  const debugBin = path.resolve(__dirname, "..", "..", "..", "target", "debug", "fast-n-rust");
  assert.equal(
    resolveBinary({}, (p) => p === releaseBin || p === debugBin, () => null, "linux", "x64"),
    releaseBin
  );
  assert.equal(
    resolveBinary({}, (p) => p === debugBin, () => null, "linux", "x64"),
    debugBin
  );
  assert.equal(
    resolveBinary({}, () => false, () => null, "linux", "x64"),
    "fast-n-rust"
  );
});

test("ensureExecutable sets 0o755 on path-qualified binaries on POSIX", () => {
  let called = null;
  ensureExecutable(
    "/tmp/fake/fast-n-rust",
    (p, mode) => {
      called = { p, mode };
    },
    "linux"
  );
  assert.deepEqual(called, { p: "/tmp/fake/fast-n-rust", mode: 0o755 });
});

test("install.js honors FNR_SKIP_DOWNLOAD=1 and verifies SHA-256 checksums", async () => {
  const res = await install({ FNR_SKIP_DOWNLOAD: "1" });
  assert.equal(res.status, "skipped");
  assert.equal(res.reason, "FNR_SKIP_DOWNLOAD=1");

  assert.equal(
    releaseAssetName("linux", "x64", "0.1.0"),
    "fast-n-rust-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
  );
  assert.equal(
    releaseAssetName("win32", "x64", "0.1.0"),
    "fast-n-rust-v0.1.0-x86_64-pc-windows-msvc.zip"
  );

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "fnr-npm-sum-"));
  const file = path.join(tmp, "artifact.tar.gz");
  fs.writeFileSync(file, "hello-fnr");
  const digest = crypto.createHash("sha256").update("hello-fnr").digest("hex");
  const parsed = parseChecksum(`${digest}  dist/artifact.tar.gz\n`, "artifact.tar.gz");
  assert.equal(parsed, digest);
  assert.equal(verifySha256(file, parsed), true);
  assert.equal(verifySha256(file, "0".repeat(64)), false);
  fs.rmSync(tmp, { recursive: true, force: true });
});
