// One-step provisioning of every third-party asset the zero-paid-dependency
// `local` profile needs (issue #220 S3, docs/local-profile-quickstart.adoc).
//
//   node scripts/fetch-local-profile.mjs <dest-dir> [--llama-flavor vulkan|cpu] [--skip llm,llama,...]
//
// It adds only what no existing script covers (the llama.cpp release, the LLM
// GGUF and the Silero VAD model, each pinned by SHA-256 in the manifest). The sherpa-onnx
// libraries and the STT/TTS models are fetched by running
// scripts/fetch-sherpa-onnx.mjs, and the sample VRM by
// renderer/local-avatar/smoke/fetch-model.mjs, so no pin is duplicated here.
// Nothing downloaded is committed or redistributed. A mismatch deletes the
// partial download and exits non-zero.
//
// Layout under <dest-dir>: llama/ llm/ vad/ sherpa-lib/ k2v2/ supertonic/ avatar/
// Steps (for --skip): llama, llm, vad, sherpa-lib, stt, tts, avatar.
// The last lines printed are the values the quickstart needs.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream, createWriteStream, existsSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..");

// Pins (URL, SHA-256, size) come from packaging/local-profile-pins.json, the single
// source shared with `aivtuber-setup local` (crates/setup).
const PINS = JSON.parse(readFileSync(join(repo, "packaging", "local-profile-pins.json"), "utf8"));
const LLAMA_TAG = PINS.llama.tag;
const LLAMA = PINS.llama.assets;
const GGUF = PINS.llm;
const VAD = PINS.vad;

function fail(message) {
  console.error(`fetch-local-profile: ${message}`);
  process.exit(1);
}

async function sha256Of(path) {
  const hash = createHash("sha256");
  await pipeline(createReadStream(path), hash);
  return hash.digest("hex");
}

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

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const dest = args.find((a, i) => !a.startsWith("--") && !(i > 0 && args[i - 1].startsWith("--")));
if (!dest) fail("usage: fetch-local-profile.mjs <dest-dir> [--llama-flavor vulkan|cpu] [--skip llama,llm,vad,sherpa-lib,stt,tts,avatar]");
const root = resolve(dest);
const skip = new Set((flag("--skip") ?? "").split(",").filter(Boolean));
const platform = `${process.platform}-${process.arch}`;
const flavor = flag("--llama-flavor") ?? (process.platform === "win32" ? "vulkan" : "cpu");
const run = (script, ...a) => execFileSync(process.execPath, [join(repo, "scripts", script), ...a], { stdio: "inherit" });

const out = {};

if (!skip.has("llama")) {
  const asset = LLAMA[`${platform}-${flavor}`];
  if (!asset) fail(`no pinned llama.cpp asset for ${platform} ${flavor} (pinned: ${Object.keys(LLAMA).join(", ")})`);
  const dir = join(root, "llama");
  const archive = join(dir, asset.file);
  await download(asset.url, archive, asset.sha256);
  const bin = join(dir, "bin");
  mkdirSync(bin, { recursive: true });
  // Windows: the system bsdtar reads zip; a GNU tar earlier on PATH (Git Bash) cannot, and reads "I:\x" as host "I".
  const tar = process.platform === "win32" ? join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe") : "tar";
  execFileSync(tar, ["-xf", join("..", asset.file), "-C", "."], { stdio: "inherit", cwd: bin });
  const exe = process.platform === "win32" ? "llama-server.exe" : "llama-server";
  // The Linux archive nests a build-<tag> directory; the Windows one is flat.
  const found = existsSync(join(bin, exe)) ? join(bin, exe) : readdirSync(bin, { withFileTypes: true }).map((e) => join(bin, e.name, exe)).find(existsSync);
  if (!found) fail(`${exe} not found after extracting ${asset.file}`);
  out.LLAMA_SERVER = found;
}
if (!skip.has("llm")) {
  const path = join(root, "llm", GGUF.file);
  await download(GGUF.url, path, GGUF.sha256);
  out.LLM_GGUF = path;
}
if (!skip.has("vad")) {
  const path = join(root, "vad", VAD.file);
  await download(VAD.url, path, VAD.sha256);
  out.VAD_MODEL = path;
}
if (!skip.has("sherpa-lib")) {
  run("fetch-sherpa-onnx.mjs", "lib", join(root, "sherpa-lib"));
  out.SHERPA_ONNX_ARCHIVE_DIR = join(root, "sherpa-lib");
}
if (!skip.has("stt")) {
  run("fetch-sherpa-onnx.mjs", "model", "k2v2-int8", join(root, "k2v2"));
  out.STT_MODEL_DIR = join(root, "k2v2");
}
if (!skip.has("tts")) {
  run("fetch-sherpa-onnx.mjs", "model", "supertonic-3-int8", join(root, "supertonic"));
  out.TTS_MODEL_DIR = join(root, "supertonic", "sherpa-onnx-supertonic-3-tts-int8-2026-05-11");
}
if (!skip.has("avatar")) {
  const { ensureModel } = await import(pathToFileURL(join(repo, "renderer", "local-avatar", "smoke", "fetch-model.mjs")).href);
  out.AVATAR_VRM = await ensureModel(join(root, "avatar", "VRM1_Constraint_Twist_Sample.vrm"));
}

console.log("\nProvisioned (paths are absolute):");
for (const [k, v] of Object.entries(out)) console.log(`${k}=${resolve(v)}`);
