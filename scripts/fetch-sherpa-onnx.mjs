// Fetches and verifies the pinned third-party material for the optional
// `sherpa-stt` feature (issue #92 S3b). Nothing here is part of any default
// build, and nothing it downloads is committed or redistributed.
//
//   node scripts/fetch-sherpa-onnx.mjs lib <dest-dir>
//       Downloads the sherpa-onnx v2.0.1 shared-library archive for this
//       platform into <dest-dir>, verifies its SHA-256 against the pin below,
//       and prints the line to export:
//           SHERPA_ONNX_ARCHIVE_DIR=<dest-dir>
//       Every build that enables the `sherpa-stt` feature MUST set that
//       variable. Without it the sherpa-onnx-sys build script downloads
//       prebuilt binaries from GitHub over HTTPS and verifies nothing. CI
//       always runs this script first (docs/supply-chain.adoc).
//
//   node scripts/fetch-sherpa-onnx.mjs model k2v2-int8|parakeet-ja <dest-dir>
//       Operator step: downloads the model files into <dest-dir>, verifying
//       every file against the pin below, and prints the --model-dir to pass
//       to the worker. The worker verifies the same SHA-256 pins again at
//       every start (crates/adapters/src/speech_stt/pins.rs).
//
//   node scripts/fetch-sherpa-onnx.mjs model supertonic-3-int8 <dest-dir>
//       The local TTS voice (#220 S2b, `sherpa-tts` feature). Same step, same
//       rule; the sidecar verifies the pins in crates/adapters/src/speech_tts/
//       pins.rs at every start. LICENCE: the Supertonic 3 weights are
//       OpenRAIL-M (use restrictions, and the duty to disclose that published
//       audio is machine-generated); see docs/supply-chain.adoc.
//
// A mismatch deletes the partial download and exits non-zero. The pins come
// from packaging/local-profile-pins.json and are asserted equal to the Rust
// pins by unit tests (pins.rs), so the copies cannot drift.

import { createHash } from "node:crypto";
import { createReadStream, createWriteStream, existsSync, mkdirSync, renameSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { pipeline } from "node:stream/promises";
import { Readable } from "node:stream";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

// Every pin (URL, SHA-256, size) lives in packaging/local-profile-pins.json, the
// single source shared with `aivtuber-setup local` (crates/setup) and the other
// fetch scripts. Nothing is repeated here.
const PINS = JSON.parse(readFileSync(join(dirname(fileURLToPath(import.meta.url)), "..", "packaging", "local-profile-pins.json"), "utf8"));
const LIB_ARCHIVES = PINS.sherpa_lib.assets;
const MODELS = Object.fromEntries(
  Object.entries(PINS.models).map(([name, m]) => [
    name,
    { archive: m.archive, innerDir: m.inner_dir, files: m.files.map((f) => (m.archive ? [f.file, f.sha256] : f)) },
  ]),
);

function fail(message) {
  console.error(`fetch-sherpa-onnx: ${message}`);
  process.exit(1);
}

async function sha256Of(path) {
  const hash = createHash("sha256");
  await pipeline(createReadStream(path), hash);
  return hash.digest("hex");
}

// Download to <dest>.part, verify, then rename: a file at its final name has
// always passed its check.
async function download(url, dest, expected) {
  if (existsSync(dest) && (await sha256Of(dest)) === expected) {
    console.log(`have      ${dest}`);
    return;
  }
  mkdirSync(dirname(dest), { recursive: true });
  const part = `${dest}.part`;
  console.log(`download  ${url}`);
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok || !response.body) fail(`${url} answered HTTP ${response.status}`);
  await pipeline(Readable.fromWeb(response.body), createWriteStream(part));
  const actual = await sha256Of(part);
  if (actual !== expected) {
    rmSync(part, { force: true });
    fail(`sha256 mismatch for ${url}: got ${actual}, pinned ${expected}`);
  }
  renameSync(part, dest);
  console.log(`ok        ${dest} ${actual}`);
}

async function fetchLib(dest) {
  const key = `${process.platform}-${process.arch}`;
  const archive = LIB_ARCHIVES[key];
  if (!archive) {
    fail(`no pinned sherpa-onnx library archive for ${key} (pinned: ${Object.keys(LIB_ARCHIVES).join(", ")})`);
  }
  await download(archive.url, join(dest, archive.file), archive.sha256);
  console.log(`\nSHERPA_ONNX_ARCHIVE_DIR=${resolve(dest)}`);
}

async function fetchModel(name, dest) {
  const model = MODELS[name];
  if (!model) fail(`unknown model ${name} (${Object.keys(MODELS).join(", ")})`);
  if (model.archive) {
    const archivePath = join(dest, model.archive.file);
    await download(model.archive.url, archivePath, model.archive.sha256);
    // System tar reads bz2 on Linux, macOS and Windows 10+.
    // Windows: use the system bsdtar; a GNU tar earlier on PATH (Git Bash) reads "I:\x" as host "I".
    const tar = process.platform === "win32" ? join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe") : "tar";
    execFileSync(tar, ["-xjf", model.archive.file, "-C", "."], { stdio: "inherit", cwd: dest });
    const dir = join(dest, model.innerDir);
    for (const [file, sha256] of model.files) {
      const actual = await sha256Of(join(dir, file));
      if (actual !== sha256) fail(`sha256 mismatch for ${join(dir, file)}: got ${actual}, pinned ${sha256}`);
    }
    console.log(`\n--model-dir ${resolve(dir)}`);
    return;
  }
  for (const { file, sha256, url } of model.files) {
    await download(url, join(dest, file), sha256);
  }
  console.log(`\n--model-dir ${resolve(dest)}`);
}

const [command, ...rest] = process.argv.slice(2);
if (command === "lib" && rest.length === 1) await fetchLib(rest[0]);
else if (command === "model" && rest.length === 2) await fetchModel(rest[0], rest[1]);
else fail("usage: fetch-sherpa-onnx.mjs lib <dest-dir> | model k2v2-int8|parakeet-ja|supertonic-3-int8 <dest-dir>");
