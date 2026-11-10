// Materializes the starter reaction pack's speech audio on this machine (issue #289).
//
//   node scripts/synthesize-starter-audio.mjs --sidecar PATH/local-tts-sidecar --model-dir DIR
//        [--audio-root DIR] [--pack-root DIR] [--speaker-id N] [--ffmpeg ffmpeg]
//
// The pack ships descriptors but no audio (generated audio is never committed).
// This step reads every descriptor that names `speech.audio_ref`, asks the pinned
// local TTS sidecar (Supertonic 3 int8, #220 S2b) to speak `speech.text`, encodes
// the WAV to the Opus file the descriptor references, and writes
// `<audio-root>/starter/PROVENANCE.json` beside the files.
//
// Determinism: the sidecar always passes lang=ja and a seed derived from the text
// alone (sha256(text)[0..4] little-endian, masked to 31 bits; the same rule as
// `speech_tts::text_seed`), so the WAV is the same for the same model bundle and
// speaker. The Opus encode is bit-exact for a given ffmpeg build. The provenance
// holds no timestamp, so a rerun on the same stack reproduces it byte for byte.
//
// Voice identity: descriptors expect one `compatibility.voice_model`
// (`example-voice-v1` today). This step does not change it. The default
// AIVTUBER_VOICE_MODEL is that same value, so leave the variable unset and the
// descriptors become usable as soon as the files exist; the provenance records
// which model/speaker the pack identity stands for.
//
// Licence: Supertonic 3 weights are OpenRAIL-M (docs/supply-chain.adoc section 10):
// published audio must be disclosed as machine-generated.
//
// Defaults: audio root = `<pack-root>/audio` (what aivtuber-app uses when
// AIVTUBER_AUDIO_ROOT is unset; git-ignored). The core below takes the
// synthesizer and encoder as arguments so tests need no model and no ffmpeg.

import { createHash } from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { request } from "node:http";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const OPENRAIL_NOTICE =
  "Supertonic 3 weights are OpenRAIL-M: use restrictions apply, including the duty to disclose that published audio is machine-generated (docs/supply-chain.adoc section 10).";
export const SEED_RULE =
  "lang=ja; seed = little-endian u32 of sha256(utf8 text)[0..4] & 0x7fffffff (speech_tts::text_seed); no random seed";

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

export function textSeed(text) {
  const d = createHash("sha256").update(text, "utf8").digest();
  return d.readUInt32LE(0) & 0x7fffffff;
}

// `audio://starter/<name>.opus` only: one flat directory, no traversal.
export function starterFileName(audioRef) {
  const m = /^audio:\/\/starter\/([A-Za-z0-9][A-Za-z0-9._-]*\.opus)$/.exec(audioRef);
  if (!m) throw new Error(`unsupported audio_ref ${JSON.stringify(audioRef)} (expected audio://starter/<name>.opus)`);
  return m[1];
}

export function readTargets(packRoot) {
  const dir = join(packRoot, "descriptors");
  const targets = [];
  const voices = new Set();
  for (const name of readdirSync(dir).filter((n) => n.endsWith(".json")).sort()) {
    const d = JSON.parse(readFileSync(join(dir, name), "utf8"));
    if (d.compatibility?.voice_model) voices.add(d.compatibility.voice_model);
    const speech = d.speech;
    if (!speech?.audio_ref) continue; // speechless descriptor (reaction.neutral)
    if (!speech.text) throw new Error(`${name}: audio_ref without speech.text`);
    targets.push({ id: d.id, text: speech.text, audioRef: speech.audio_ref, fileName: starterFileName(speech.audio_ref) });
  }
  if (voices.size > 1) throw new Error(`descriptors expect more than one voice_model: ${[...voices].join(", ")}`);
  return { targets, voiceModel: [...voices][0] ?? null };
}

/**
 * @param {object} o
 * @param {string} o.packRoot
 * @param {string} o.audioRoot
 * @param {(text: string) => Promise<{wavPath: string, durationMs?: number}>} o.synthesize
 * @param {(wavPath: string, opusPath: string) => Promise<void>} o.encode
 * @param {object} o.engine  provenance of the synthesizer: model_alias, model_version, speaker_id, ...
 */
export async function materializeStarterAudio({ packRoot, audioRoot, synthesize, encode, engine }) {
  const { targets, voiceModel } = readTargets(packRoot);
  if (targets.length === 0) throw new Error("no descriptor names speech audio");
  const outDir = join(audioRoot, "starter");
  mkdirSync(outDir, { recursive: true });
  const files = [];
  for (const t of targets) {
    const { wavPath, durationMs } = await synthesize(t.text);
    const opusPath = join(outDir, t.fileName);
    await encode(wavPath, opusPath);
    files.push({
      descriptor_id: t.id,
      audio_ref: t.audioRef,
      text: t.text,
      seed: textSeed(t.text),
      wav_sha256: sha256(readFileSync(wavPath)),
      opus_sha256: sha256(readFileSync(opusPath)),
      synthesized_duration_ms: durationMs ?? null,
    });
  }
  const provenance = {
    schema: "starter-audio-provenance/1",
    issue: 289,
    generated_by: "scripts/synthesize-starter-audio.mjs",
    committed_to_git: false,
    voice_model_identity: voiceModel,
    voice_model_identity_note:
      "the descriptors' compatibility.voice_model; leave AIVTUBER_VOICE_MODEL unset or set to this exact value",
    engine,
    seed_rule: SEED_RULE,
    encoding: "opus (libopus, 32 kbit/s, mono, bit-exact muxer, no metadata)",
    licence_notice: OPENRAIL_NOTICE,
    files,
  };
  writeFileSync(join(outDir, "PROVENANCE.json"), `${JSON.stringify(provenance, null, 2)}\n`);
  return provenance;
}

// ---- real synthesizer: the pinned sidecar over the normalized TTS contract ----

function startSidecar({ sidecar, modelDir, audioRoot, speakerId }) {
  return new Promise((resolveReady, reject) => {
    const args = ["--model-dir", modelDir, "--audio-root", audioRoot, "--listen", "127.0.0.1:0", "--speaker-id", String(speakerId)];
    const child = spawn(sidecar, args, { stdio: ["pipe", "pipe", "inherit"] });
    let buffer = "";
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error("sidecar did not become ready within 120 s"));
    }, 120_000);
    child.on("error", (e) => { clearTimeout(timer); reject(e); });
    child.on("exit", (code) => { clearTimeout(timer); reject(new Error(`sidecar exited before ready (code ${code})`)); });
    child.stdout.on("data", (chunk) => {
      buffer += chunk;
      const line = buffer.split("\n").find((l) => l.includes('"tts"'));
      if (!line) return;
      try {
        const ready = JSON.parse(line).tts;
        if (ready.state !== "ready") return;
        clearTimeout(timer);
        child.removeAllListeners("exit");
        resolveReady({ child, ready });
      } catch { /* wait for the full line */ }
    });
  });
}

function postJson(listen, path, body) {
  const [host, port] = [listen.slice(0, listen.lastIndexOf(":")), Number(listen.slice(listen.lastIndexOf(":") + 1))];
  return new Promise((res, rej) => {
    const payload = JSON.stringify(body);
    const req = request({ host, port, path, method: "POST", headers: { "content-type": "application/json", "content-length": Buffer.byteLength(payload) } }, (r) => {
      let data = "";
      r.on("data", (c) => (data += c));
      r.on("end", () => (r.statusCode === 200 ? res(JSON.parse(data)) : rej(new Error(`sidecar HTTP ${r.statusCode}: ${data.slice(0, 200)}`))));
    });
    req.setTimeout(60_000, () => req.destroy(new Error("sidecar request timed out")));
    req.on("error", rej);
    req.end(payload);
  });
}

export function ffmpegEncoder(ffmpeg) {
  return async (wavPath, opusPath) => {
    const r = spawnSync(
      ffmpeg,
      ["-y", "-v", "error", "-i", wavPath, "-map_metadata", "-1", "-ac", "1", "-c:a", "libopus", "-b:a", "32k", "-application", "voip", "-fflags", "+bitexact", "-flags:a", "+bitexact", opusPath],
      { encoding: "utf8" },
    );
    if (r.error || r.status !== 0) throw new Error(`ffmpeg failed: ${r.error ?? r.stderr}`);
  };
}

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 2) {
    if (!argv[i].startsWith("--") || argv[i + 1] === undefined) throw new Error(`bad argument ${argv[i]}`);
    out[argv[i].slice(2)] = argv[i + 1];
  }
  return out;
}

async function main() {
  const repo = resolve(dirname(fileURLToPath(import.meta.url)), "..");
  const a = parseArgs(process.argv.slice(2));
  if (!a.sidecar || !a["model-dir"]) {
    throw new Error("usage: node scripts/synthesize-starter-audio.mjs --sidecar PATH --model-dir DIR [--audio-root DIR] [--pack-root DIR] [--speaker-id N] [--ffmpeg ffmpeg]");
  }
  const packRoot = resolve(a["pack-root"] ?? join(repo, "examples/starter-reaction-pack"));
  const audioRoot = resolve(a["audio-root"] ?? join(packRoot, "audio"));
  const speakerId = Number(a["speaker-id"] ?? 0);
  mkdirSync(audioRoot, { recursive: true });
  console.error(`synthesize-starter-audio: ${OPENRAIL_NOTICE}`);
  const { child, ready } = await startSidecar({ sidecar: resolve(a.sidecar), modelDir: resolve(a["model-dir"]), audioRoot, speakerId });
  try {
    const synthesize = async (text) => {
      const r = await postJson(ready.listen, "/v1/synthesize", { text });
      const m = /^audio:\/\/generated\/([A-Za-z0-9._-]+\.wav)$/.exec(r.audio_ref);
      if (!m) throw new Error(`unexpected audio_ref ${JSON.stringify(r.audio_ref)}`);
      return { wavPath: join(audioRoot, "generated", m[1]), durationMs: r.duration_ms };
    };
    const engine = { backend: "local-tts-sidecar", model_alias: ready.model_id, model_version: ready.model_version, speaker_id: speakerId, steps: 5, speed: 1.0 };
    const p = await materializeStarterAudio({ packRoot, audioRoot, synthesize, encode: ffmpegEncoder(a.ffmpeg ?? "ffmpeg"), engine });
    console.error(`synthesize-starter-audio: wrote ${p.files.length} files under ${join(audioRoot, "starter")} (voice_model_identity=${p.voice_model_identity})`);
    if (audioRoot !== join(packRoot, "audio")) console.error(`synthesize-starter-audio: export AIVTUBER_AUDIO_ROOT=${audioRoot}`);
  } finally {
    child.stdin.end(); // the sidecar exits at stdin EOF
    setTimeout(() => child.kill(), 3000).unref();
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((e) => {
    console.error(`synthesize-starter-audio: ${e.message}`);
    process.exit(1);
  });
}
