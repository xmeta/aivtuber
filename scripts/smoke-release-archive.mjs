// Smoke test for a local-profile release archive (issue #294 S1).
//
//   node scripts/smoke-release-archive.mjs <archive.zip|archive.tar.gz>
//
// Unpacks the archive into a fresh directory and uses nothing else (no Cargo,
// no Bun, no model, no network). It checks:
//   * the expected files are present and no model weights or voice files are;
//   * THIRD-PARTY-NOTICES.txt holds every vendored engine licence text in full
//     (packaging/third-party/, issue #349) and never the missing-licence stub;
//   * on Windows, no shipped .exe or .dll imports the Visual C++ runtime or the
//     universal CRT: a clean Windows has none of them, so the Rust programs are
//     built with the static CRT and the bundled engine DLLs are the "MT" build
//     (a CI runner has the runtime installed, so imports are checked, not
//     whether the program runs);
//   * no Visual C++ redistributable DLL (vcruntime140*, msvcp140*, concrt140*,
//     vcomp140*, mfc140*) is anywhere in the archive: this project does not
//     redistribute them (#349, docs/supply-chain.adoc section 11);
//   * every program starts and answers its probe (usage / --help), which also
//     shows the loader found the bundled engine libraries beside it;
//   * the STT worker and the TTS sidecar verify the bundled engine libraries
//     and then refuse an empty (unpinned) model directory with exit 2;
//   * a tampered library beside a program is refused with exit 2;
//   * the avatar page files and the operation panel page (#335) are present, and
//     the panel page loads no script or style from anywhere but its own files;
//   * the first-run command (aivtuber-setup, issue #294 S2) prints its plan with
//     --dry-run and refuses --offline when nothing is fetched, without touching
//     the network or writing a file.
// Exit 0 when all pass, 1 otherwise. Runs on Windows x64 and Linux x64.
import { spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, appendFileSync, readdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { peImports } from "./pe-imports.mjs";
import { MISSING_LICENCE_STUB, VENDORED_NOTICES, heading, readVendoredNotice } from "./release-notices.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
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
  "aivtuber-setup",
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

  // A clean Windows lacks VCRUNTIME140.dll (issue #294: exit 0xC0000135). The CI
  // runner has it, so a missing runtime cannot show up as a failed start; read
  // the import tables instead. If a pinned engine library ever needs the runtime,
  // this fails; fix the pin or the build, do not ship the runtime DLLs
  // (docs/supply-chain.adoc section 11).
  if (win) {
    check("no shipped .exe or .dll imports the Visual C++ runtime or universal CRT", () => {
      const forbidden = /^(vcruntime|msvcp|concrt|vcomp|mfc|api-ms-win-crt-)/;
      const bad = [];
      let scanned = 0;
      for (const file of readdirSync(root).filter((f) => /\.(exe|dll)$/i.test(f))) {
        scanned += 1;
        for (const dll of peImports(join(root, file))) if (forbidden.test(dll)) bad.push(`${file} -> ${dll}`);
      }
      assert(scanned >= programs.length + libs.length, `scanned only ${scanned} files`);
      assert(bad.length === 0, `needs a runtime a clean Windows lacks: ${bad.join("; ")}`);
      return `${scanned} files scanned`;
    });
  }

  // The pinned llama.cpp build needs the Visual C++ runtime, which setup checks
  // for on the machine; it must not travel in the archive (#349).
  check("no Visual C++ redistributable DLL is in the archive", () => {
    const redist = /^(vcruntime|msvcp|concrt|vcomp|mfc)\d*(_\d+)?\.dll$/i;
    const found = [];
    let files = 0;
    const walk = (dir) => {
      for (const entry of readdirSync(dir, { withFileTypes: true })) {
        const p = join(dir, entry.name);
        if (entry.isDirectory()) walk(p);
        else {
          files += 1;
          if (redist.test(entry.name)) found.push(p.slice(root.length + 1));
        }
      }
    };
    walk(root);
    assert(files > programs.length, `walked only ${files} files`);
    assert(found.length === 0, `Visual C++ redistributable DLLs in the archive: ${found.join(", ")}`);
    assert(!existsSync(join(root, "runtime", "msvc")), "runtime/msvc exists");
    return `${files} files checked`;
  });

  check("licence and notice files are present", () => {
    for (const name of ["README.txt", "THIRD-PARTY-LICENSES-RUST.html", "THIRD-PARTY-NOTICES.txt"]) {
      assert(existsSync(join(root, name)) && statSync(join(root, name)).size > 1000, `missing or empty ${name}`);
    }
  });

  // Issue #349: the engine libraries' upstream licence texts (ONNX Runtime's
  // ThirdPartyNotices, the sherpa-onnx static dependencies, Eigen's MPL-2.0) must
  // be in the archive in full, not pointed to, and no stub may stand in for a
  // missing renderer licence. The expected texts are the vendored files this
  // archive was built from (packaging/third-party/).
  check("third-party notices carry every vendored engine text and no stub", () => {
    const notices = readFileSync(join(root, "THIRD-PARTY-NOTICES.txt"), "utf8").replace(/\r\n/g, "\n");
    for (const name of VENDORED_NOTICES) {
      const section = `${heading(name)}\n\n${readVendoredNotice(name)}`;
      assert(notices.includes(section), `THIRD-PARTY-NOTICES.txt lacks the full text of packaging/third-party/${name}`);
    }
    assert(notices.includes("Eigen (MPL-2.0) source code"), "THIRD-PARTY-NOTICES.txt lacks the Eigen MPL-2.0 source pointer");
    assert(notices.includes("==== Avatar page"), "THIRD-PARTY-NOTICES.txt lacks the avatar page section");
    assert(!notices.includes(MISSING_LICENCE_STUB), `THIRD-PARTY-NOTICES.txt contains the missing-licence stub "${MISSING_LICENCE_STUB}"`);
    return `${VENDORED_NOTICES.length} texts`;
  });

  check("avatar page files are present", () => {
    const page = join(root, "renderer", "local-avatar", "dist");
    assert(existsSync(join(page, "index.html")), "renderer/local-avatar/dist/index.html missing");
    const assets = existsSync(join(page, "assets")) ? readdirSync(join(page, "assets")) : [];
    assert(assets.some((f) => f.endsWith(".js")), "renderer/local-avatar/dist/assets has no script bundle");
  });

  check("operation panel page is present and loads only its own files", () => {
    const page = join(root, "renderer", "local-avatar", "dist");
    assert(existsSync(join(page, "panel.html")), "renderer/local-avatar/dist/panel.html missing");
    const html = readFileSync(join(page, "panel.html"), "utf8");
    // The server's CSP is script-src 'self'; an inline script or a remote URL would be dead on arrival.
    const scripts = [...html.matchAll(/<script\b[^>]*>/g)].map((m) => m[0]);
    assert(scripts.length >= 1 && scripts.every((s) => /\ssrc="\.\/assets\/[^"]+\.js"/.test(s)), `unexpected script tags: ${scripts.join(" ")}`);
    assert(!/(?:src|href)="(?:https?:)?\/\//.test(html), "panel.html references a remote URL");
    const asset = (re) => [...html.matchAll(re)].map((m) => m[1]);
    for (const file of [...asset(/src="\.\/(assets\/[^"]+)"/g), ...asset(/href="\.\/(assets\/[^"]+)"/g)]) {
      assert(existsSync(join(page, file)), `panel.html references missing ${file}`);
    }
    // The page must send the secret only in a header, never to a URL it builds.
    const bundle = readdirSync(join(page, "assets")).filter((f) => f.startsWith("panel-") && f.endsWith(".js"));
    assert(bundle.length === 1, `expected one panel bundle, got ${bundle.join(", ")}`);
    const js = readFileSync(join(page, "assets", bundle[0]), "utf8");
    assert(js.includes("X-Panel-Token") && !/[?&]token=/.test(js), "panel bundle does not use the header-only secret");
  });

  check("no Node script ships: the first-run command replaces them", () => {
    assert(!existsSync(join(root, "scripts")), "scripts/ must not ship");
    assert(!existsSync(join(root, "renderer", "local-avatar", "smoke")), "renderer smoke scripts must not ship");
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
  const probes = ["aivtuber-setup", "speech-vad-sidecar", "speech-stt-worker", "local-tts-sidecar", "twitch-chat-ingress", "youtube-chat-ingress", "aivtuberctl"];
  for (const program of probes) {
    check(`${program} --help starts and prints usage`, () => {
      const { status, out } = run(join(root, program + exe), ["--help"], root);
      assert([0, 1, 2].includes(status), `exit ${status}: ${out.trim().slice(0, 300)}`);
      assert(/usage/i.test(out), `no usage text: ${out.trim().slice(0, 300)}`);
      return `exit ${status}`;
    });
  }

  // First-run command: the plan, offline. Nothing may be fetched or written.
  const listing = () => readdirSync(root, { recursive: true }).map(String).sort().join("\n");
  check("aivtuber-setup local --dry-run prints the plan and touches nothing", () => {
    const before = listing();
    const assets = join(work, "setup-assets");
    const { status, out } = run(join(root, "aivtuber-setup" + exe), ["local", "--dry-run", "--assets", assets], root);
    assert(status === 0, `exit ${status}: ${out.trim().slice(0, 400)}`);
    for (const needle of ["dry run", "gemma-4-E4B-it-qat-UD-Q4_K_XL.gguf", "Supertonic 3", "OpenRAIL-M", "machine-generated", "ReazonSpeech", "start"]) {
      assert(out.includes(needle), `plan lacks "${needle}": ${out.trim().slice(0, 400)}`);
    }
    assert(!existsSync(assets), "dry run created the assets directory");
    assert(listing() === before, "dry run changed the unpacked directory");
  });
  check("aivtuber-setup local --dry-run --with-model plans the optional model and refuses an unknown one", () => {
    const assets = join(work, "setup-assets-3");
    const ok = run(join(root, "aivtuber-setup" + exe), ["local", "--dry-run", "--with-model", "qwen3-4b", "--assets", assets], root);
    assert(ok.status === 0, `exit ${ok.status}: ${ok.out.trim().slice(0, 400)}`);
    assert(ok.out.includes("Qwen3-4B-Instruct-2507-Q4_K_M.gguf"), `plan lacks the optional model: ${ok.out.trim().slice(0, 400)}`);
    const bad = run(join(root, "aivtuber-setup" + exe), ["local", "--dry-run", "--with-model", "nope", "--assets", assets], root);
    assert(bad.status === 1 && /unknown model/.test(bad.out), `exit ${bad.status}: ${bad.out.trim().slice(0, 400)}`);
    assert(!existsSync(assets), "dry run created the assets directory");
  });
  check("aivtuber-setup local --offline fails closed and writes no env file", () => {
    const { status, out } = run(join(root, "aivtuber-setup" + exe), ["local", "--offline", "--assets", join(work, "setup-assets-2")], root);
    assert(status === 1, `exit ${status}: ${out.trim().slice(0, 400)}`);
    assert(/--offline: missing/.test(out), `no offline refusal: ${out.trim().slice(0, 400)}`);
    assert(!existsSync(join(root, "local-profile.env")), "an env file was written");
  });
  check("aivtuber-setup start --dry-run without an env file points at the setup step", () => {
    const { status, out } = run(join(root, "aivtuber-setup" + exe), ["start", "--dry-run"], root);
    assert(status === 1 && /aivtuber-setup local/.test(out), `exit ${status}: ${out.trim().slice(0, 400)}`);
  });

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
