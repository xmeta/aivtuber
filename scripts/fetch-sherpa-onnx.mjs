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
// A mismatch deletes the partial download and exits non-zero. The pins here
// are asserted equal to the Rust pins by a unit test (pins.rs), so the two
// copies cannot drift.

import { createHash } from "node:crypto";
import { createReadStream, createWriteStream, existsSync, mkdirSync, renameSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { pipeline } from "node:stream/promises";
import { Readable } from "node:stream";
import { execFileSync } from "node:child_process";

const SHERPA_RELEASE = "https://github.com/k2-fsa/sherpa-onnx/releases/download";
const HF = "https://huggingface.co";
const RZ_REV = "291488c8151be24d7da4bf7af26e533fad96e407"; // reazon-research/reazonspeech-k2-v2

// Engine library archives, sherpa-onnx v2.0.1, `shared` flavour (what the
// crate's build script asks for with its `shared` feature). SHA-256 values were
// verified by downloading each archive, and equal the digests GitHub publishes.
const LIB_ARCHIVES = {
  "linux-x64": {
    name: "sherpa-onnx-v2.0.1-linux-x64-shared-lib.tar.bz2",
    sha256: "532c45bbd41036d83cd70606797529aa8086c2053395a58967736c727625b120",
  },
  "win32-x64": {
    name: "sherpa-onnx-v2.0.1-win-x64-shared-MT-Release-lib.tar.bz2",
    sha256: "afa4de3ca4ab0afd53bb4aab40d28ee15608364b549ffc0fe0caf6489bfc39a5",
  },
};

const MODELS = {
  "supertonic-3-int8": {
    // sherpa-onnx tts-models release asset (the tag is mutable, the hash is the pin).
    archive: {
      name: "sherpa-onnx-supertonic-3-tts-int8-2026-05-11.tar.bz2",
      url: `${SHERPA_RELEASE}/tts-models/sherpa-onnx-supertonic-3-tts-int8-2026-05-11.tar.bz2`,
      sha256: "82fa96f91c4ef8abaae3a14a3f4153facf88bed821d1f7331cec2700f432c427",
    },
    innerDir: "sherpa-onnx-supertonic-3-tts-int8-2026-05-11",
    files: [
      ["duration_predictor.int8.onnx", "c3eb91414d5ff8a7a239b7fe9e34e7e2bf8a8140d8375ffb14718b1c639325db"],
      ["text_encoder.int8.onnx", "c7befd5ea8c3119769e8a6c1486c4edc6a3bc8365c67621c881bbb774b9902ff"],
      ["vector_estimator.int8.onnx", "20cd86fa5c6effedfda0e7cffe5b0569ca401c440a0c3a1d72bf39286c0db3fd"],
      ["vocoder.int8.onnx", "e923d60f53f95eb1ce235f1dc33ec56d9c057823c96fa6f8acf98f32b0da6152"],
      ["tts.json", "42078d3aef1cd43ab43021f3c54f47d2d75ceb4e75f627f118890128b06a0d09"],
      ["unicode_indexer.bin", "8402ca48e5189a8950138580b0fff64db6f072f24ac07cd54ba8b2fbb9883b30"],
      ["voice.bin", "67d5209b0ee8ce6c74105ffbe12fe6a7628aea3b4ba2fcb308a4a67938a93ce8"],
    ],
  },
  "k2v2-int8": {
    files: [
      ["encoder-epoch-99-avg-1.int8.onnx", "2c7bd08a8a99f9ddd0d9e458456577b1f6279214e51426f114f9eced44c54e1d"],
      ["decoder-epoch-99-avg-1.int8.onnx", "8f0bff94d38797b03b762634ed03211a8e303d06cc4603cdd0cf4199d6eb1485"],
      ["joiner-epoch-99-avg-1.int8.onnx", "49cc7ea1d3d35a40a27442db5e89996da64bf0e683a903dce76e99e57a12e4de"],
      ["tokens.txt", "2c3ac659818a48a0c04010e0593bbc4d7c8a24a054340b01131499c05fd52def"],
    ].map(([file, sha256]) => ({ file, sha256, url: `${HF}/reazon-research/reazonspeech-k2-v2/resolve/${RZ_REV}/${file}` })),
  },
  "parakeet-ja": {
    // One archive; its two files are pinned inside the worker.
    archive: {
      name: "sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8.tar.bz2",
      url: `${SHERPA_RELEASE}/asr-models/sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8.tar.bz2`,
      sha256: "4b0a800ef29f4f4c8667339bf6f60d5bfdc2852ddc9dc5741aea65b6f8d1306b",
    },
    innerDir: "sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8",
    files: [
      ["model.int8.onnx", "3addd00ef5bd1742078389e540b77394e4a508bdf2f4c9ad1b4a76d93e76598e"],
      ["tokens.txt", "732f64c53909f2620c713f4106b487d92e6f54a6915b3cd3d1dbd32f9f4f392a"],
    ],
  },
};

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
  await download(`${SHERPA_RELEASE}/v2.0.1/${archive.name}`, join(dest, archive.name), archive.sha256);
  console.log(`\nSHERPA_ONNX_ARCHIVE_DIR=${resolve(dest)}`);
}

async function fetchModel(name, dest) {
  const model = MODELS[name];
  if (!model) fail(`unknown model ${name} (${Object.keys(MODELS).join(", ")})`);
  if (model.archive) {
    const archivePath = join(dest, model.archive.name);
    await download(model.archive.url, archivePath, model.archive.sha256);
    // System tar reads bz2 on Linux, macOS and Windows 10+.
    execFileSync("tar", ["-xjf", archivePath, "-C", dest], { stdio: "inherit" });
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
