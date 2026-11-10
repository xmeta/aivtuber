import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { materializeStarterAudio, readTargets, starterFileName, textSeed } from "../scripts/synthesize-starter-audio.mjs";

// Issue #289. No model, no sidecar, no ffmpeg: a fake synthesizer and encoder.
const packRoot = join(import.meta.dir, "..", "examples", "starter-reaction-pack");

function fakes(work) {
  return {
    // Deterministic bytes from the text, like the real seed rule.
    synthesize: async (text) => {
      const wavPath = join(work, `${textSeed(text)}.wav`);
      writeFileSync(wavPath, `RIFFfake:${text}`);
      return { wavPath, durationMs: text.length * 100 };
    },
    encode: async (wav, opus) => writeFileSync(opus, `OPUS:${readFileSync(wav, "utf8")}`),
    engine: { backend: "fake", model_alias: "fake", model_version: "0", speaker_id: 0 },
  };
}

async function run() {
  const base = mkdtempSync(join(tmpdir(), "starter-audio-"));
  const work = mkdtempSync(join(tmpdir(), "starter-wav-"));
  const provenance = await materializeStarterAudio({ packRoot, audioRoot: base, ...fakes(work) });
  return { base, work, provenance };
}

describe("synthesize-starter-audio", () => {
  test("writes exactly the files the pack references, plus provenance", async () => {
    const { base, work, provenance } = await run();
    try {
      const { targets } = readTargets(packRoot);
      expect(targets.length).toBe(6); // reaction.neutral is speechless
      const expected = targets.map((t) => t.fileName).sort();
      expect(readdirSync(join(base, "starter")).filter((n) => n.endsWith(".opus")).sort()).toEqual(expected);
      expect(readdirSync(join(base, "starter"))).toContain("PROVENANCE.json");
      expect(provenance.files.length).toBe(6);
      expect(provenance.voice_model_identity).toBe("example-voice-v1");
      expect(provenance.licence_notice).toContain("OpenRAIL-M");
      expect(provenance.seed_rule).toContain("sha256");
      expect(provenance.committed_to_git).toBe(false);
    } finally {
      rmSync(base, { recursive: true, force: true });
      rmSync(work, { recursive: true, force: true });
    }
  });

  test("is deterministic: two runs give identical files and provenance", async () => {
    const a = await run();
    const b = await run();
    try {
      const names = readdirSync(join(a.base, "starter")).sort();
      expect(readdirSync(join(b.base, "starter")).sort()).toEqual(names);
      for (const n of names) {
        expect(readFileSync(join(b.base, "starter", n), "utf8")).toBe(readFileSync(join(a.base, "starter", n), "utf8"));
      }
    } finally {
      for (const d of [a.base, a.work, b.base, b.work]) rmSync(d, { recursive: true, force: true });
    }
  });

  test("the seed rule matches the Rust text_seed on a known text", () => {
    // sha256("a") = ca978112...; first four bytes LE = 0x128197ca? -> mask to 31 bits.
    expect(textSeed("a")).toBe(0x128197ca & 0x7fffffff);
    expect(textSeed("a")).toBeGreaterThanOrEqual(0);
  });

  test("only flat audio://starter/*.opus references are accepted", () => {
    expect(starterFileName("audio://starter/reaction-agree-01.opus")).toBe("reaction-agree-01.opus");
    for (const bad of ["audio://starter/../x.opus", "audio://other/x.opus", "audio://starter/x.wav", "file:///x.opus", "audio://starter/a/b.opus"]) {
      expect(() => starterFileName(bad)).toThrow();
    }
  });
});
