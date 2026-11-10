// Smoke test for a local-profile release archive (issue #294 S1).
//
//   node scripts/smoke-release-archive.mjs <archive.zip|archive.tar.gz>
//
// Unpacks the archive into a fresh directory and uses nothing else (no Cargo,
// no Bun, no model, no network). It checks:
//   * the expected files are present and no model weights or voice files are;
//   * every program starts and answers its probe (usage / --help), which also
//     shows the loader found the bundled engine libraries beside it;
//   * the STT worker and the TTS sidecar verify the bundled engine libraries
//     and then refuse an empty (unpinned) model directory with exit 2;
//   * a tampered library beside a program is refused with exit 2;
//   * the avatar page files are present.
// Exit 0 when all pass, 1 otherwise. Runs on Windows x64 and Linux x64.
import { spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, appendFileSync, readdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const archive = process.argv[2] ? resolve(process.argv[2]) : null;
if (!archive || !existsSync(archive)) {
  console.error("usage: node scripts/smoke-release-archive.mjs <archive.zip|archive.tar.gz>");
  process.exit(2);
}

const win = process.platform === "win32";
const exe = win ? ".exe" : "";
const libs = win
  ? ["sherpa-onnx-c-api.dll", "onnxruntime.dll", "onnxruntime_providers_shared.dll"]
  : ["libsherpa-onnx-c-api.so", "libonnxruntime.so"];
const programs = [
  "aivtuber-app",
  "aivtuberctl",
  "speech-vad-sidecar",
  "speech-stt-worker",
  "local-tts-sidecar",
  "twitch-chat-ingress",
  "youtube-chat-ingress",
];

const tar = win ? join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe") : "tar";
const work = mkdtempSync(join(tmpdir(), "aivtuber-smoke-"));
let failures = 0;

function check(name, fn) {
  try {
    const detail = fn();
    console.log(`ok        ${name}${detail ? ` (${detail})` : ""}`);
  } catch (error) {
    failures += 1;
    console.log(`FAIL      ${name}: ${error.message}`);
  }
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

/// Runs a program with a closed stdin and a minimal environment (no AIVTUBER_*
/// variable, no library path), bounded in time.
function run(file, args, cwd) {
  const env = {};
  for (const key of ["PATH", "SystemRoot", "SYSTEMROOT", "TEMP", "TMP", "HOME", "USERPROFILE"]) {
    if (process.env[key] !== undefined) env[key] = process.env[key];
  }
  const result = spawnSync(file, args, { cwd, env, input: "", timeout: 30_000, encoding: "utf8", windowsHide: true });
  if (result.error) throw new Error(`could not start ${file}: ${result.error.message}`);
  return { status: result.status, out: `${result.stdout}\n${result.stderr}` };
}

try {
  // Unpack.
  const unpack = spawnSync(tar, ["-xf", archive, "-C", work], { encoding: "utf8" });
  if (unpack.status !== 0) {
    console.error(`FAIL      unpack: ${unpack.stderr}`);
    process.exit(1);
  }
  const tops = readdirSync(work);
  if (tops.length !== 1) {
    console.error(`FAIL      archive must have exactly one top-level directory, got ${JSON.stringify(tops)}`);
    process.exit(1);
  }
  const root = join(work, tops[0]);
  console.log(`unpacked ${tops[0]}`);

  check("programs and engine libraries are present", () => {
    for (const name of [...programs.map((p) => p + exe), ...libs]) {
      assert(existsSync(join(root, name)), `missing ${name}`);
    }
  });

  check("licence and notice files are present", () => {
    for (const name of ["README.txt", "THIRD-PARTY-LICENSES-RUST.html", "THIRD-PARTY-NOTICES.txt"]) {
      assert(existsSync(join(root, name)) && statSync(join(root, name)).size > 1000, `missing or empty ${name}`);
    }
  });

  check("avatar page files are present", () => {
    const page = join(root, "renderer", "local-avatar", "dist");
    assert(existsSync(join(page, "index.html")), "renderer/local-avatar/dist/index.html missing");
    const assets = existsSync(join(page, "assets")) ? readdirSync(join(page, "assets")) : [];
    assert(assets.some((f) => f.endsWith(".js")), "renderer/local-avatar/dist/assets has no script bundle");
  });

  check("operator fetch scripts are present", () => {
    for (const rel of [
      "scripts/fetch-local-profile.mjs",
      "scripts/fetch-sherpa-onnx.mjs",
      "scripts/synthesize-starter-audio.mjs",
      "renderer/local-avatar/smoke/fetch-model.mjs",
    ]) {
      assert(existsSync(join(root, rel)), `missing ${rel}`);
    }
  });

  check("starter pack has descriptors and no audio", () => {
    assert(readdirSync(join(root, "examples", "starter-reaction-pack", "descriptors")).length > 0, "no descriptors");
  });

  check("no model weights, voice files or avatar models are bundled", () => {
    const hits = [];
    const walk = (dir) => {
      for (const entry of readdirSync(dir, { withFileTypes: true })) {
        const path = join(dir, entry.name);
        if (entry.isDirectory()) walk(path);
        else if (/\.(vrm|gguf|onnx|safetensors|opus|wav|ogg|mp3)$/i.test(entry.name)) hits.push(entry.name);
      }
    };
    walk(root);
    assert(hits.length === 0, `found ${hits.join(", ")}`);
  });

  // Probes. The programs print their usage for --help (the exit code differs
  // per program: 0, 1 or 2 are all a deliberate usage exit; a loader failure
  // or a crash is a different code and prints no usage). Reaching main at all
  // means the OS loader found every linked library, including the engine
  // libraries beside the executable.
  const probes = ["speech-vad-sidecar", "speech-stt-worker", "local-tts-sidecar", "twitch-chat-ingress", "youtube-chat-ingress", "aivtuberctl"];
  for (const program of probes) {
    check(`${program} --help starts and prints usage`, () => {
      const { status, out } = run(join(root, program + exe), ["--help"], root);
      assert([0, 1, 2].includes(status), `exit ${status}: ${out.trim().slice(0, 300)}`);
      assert(/usage/i.test(out), `no usage text: ${out.trim().slice(0, 300)}`);
      return `exit ${status}`;
    });
  }

  // The app has no --help: with no configuration it must stop with an error
  // naming a missing setting, not start serving or hang.
  check("aivtuber-app with no configuration fails closed", () => {
    const { status, out } = run(join(root, "aivtuber-app" + exe), [], root);
    assert(status === 1, `exit ${status}: ${out.trim().slice(0, 300)}`);
    assert(/aivtuber-app:/.test(out), `no error line: ${out.trim().slice(0, 300)}`);
    return `exit ${status}`;
  });

  // The bundled libraries load and verify from beside the executable, then the
  // model check refuses an empty directory (exit 2).
  const emptyModel = join(work, "no-model");
  const audioRoot = join(work, "audio");
  mkdirSync(emptyModel);
  mkdirSync(audioRoot);
  check("speech-stt-worker verifies bundled libraries, refuses an unpinned model", () => {
    const { status, out } = run(
      join(root, "speech-stt-worker" + exe),
      ["--model", "k2v2-int8", "--model-dir", emptyModel, "--language", "ja"],
      root,
    );
    assert(status === 2, `exit ${status}: ${out.trim().slice(0, 300)}`);
    assert(/model files/.test(out), `refused for another reason: ${out.trim().slice(0, 300)}`);
  });
  check("local-tts-sidecar verifies bundled libraries, refuses an unpinned model", () => {
    const { status, out } = run(
      join(root, "local-tts-sidecar" + exe),
      ["--model-dir", emptyModel, "--audio-root", audioRoot, "--listen", "127.0.0.1:0"],
      root,
    );
    assert(status === 2, `exit ${status}: ${out.trim().slice(0, 300)}`);
    assert(/model files/.test(out), `refused for another reason: ${out.trim().slice(0, 300)}`);
  });

  // One byte appended to a copied library must be refused by the pin check.
  check("a tampered engine library beside the worker is refused", () => {
    const tampered = join(work, "tampered");
    mkdirSync(tampered);
    cpSync(join(root, "speech-stt-worker" + exe), join(tampered, "speech-stt-worker" + exe));
    for (const lib of libs) cpSync(join(root, lib), join(tampered, lib));
    appendFileSync(join(tampered, libs.find((l) => /onnxruntime\.|libonnxruntime/.test(l))), Buffer.from([0]));
    const { status, out } = run(
      join(tampered, "speech-stt-worker" + exe),
      ["--model", "k2v2-int8", "--model-dir", emptyModel, "--language", "ja"],
      tampered,
    );
    assert(status === 2, `exit ${status}: ${out.trim().slice(0, 300)}`);
    assert(/engine libraries beside the executable/.test(out), `refused for another reason: ${out.trim().slice(0, 300)}`);
  });
} finally {
  rmSync(work, { recursive: true, force: true });
}

if (failures > 0) {
  console.error(`${failures} smoke check(s) failed`);
  process.exit(1);
}
console.log("release archive smoke: all checks passed");
