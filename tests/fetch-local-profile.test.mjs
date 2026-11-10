import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

// Offline checks for the local-profile provisioning scripts (issue #220 S3) and
// the single-source pin manifest they share with `aivtuber-setup local`
// (issue #294 S2, crates/setup). Nothing here downloads anything or needs a model.
const root = join(import.meta.dir, "..");
const read = (p) => readFileSync(join(root, p), "utf8");
const wrapper = read("scripts/fetch-local-profile.mjs");
const sherpa = read("scripts/fetch-sherpa-onnx.mjs");
const avatarFetch = read("renderer/local-avatar/smoke/fetch-model.mjs");
const manifest = JSON.parse(read("packaging/local-profile-pins.json"));

const hexes = (text) => [...text.matchAll(/\b[0-9a-f]{64}\b/g)].map((m) => m[0]);
const allAssets = () => [
  ...Object.values(manifest.llama.assets),
  manifest.llm,
  manifest.vad,
  manifest.avatar,
  ...Object.values(manifest.sherpa_lib.assets),
  ...Object.values(manifest.models).flatMap((m) => (m.archive ? [m.archive] : [])),
];

describe("packaging/local-profile-pins.json (the one pin manifest)", () => {
  test("every pin is a 64-hex SHA-256 with an exact size and an https URL", () => {
    for (const a of allAssets()) {
      expect(a.sha256).toMatch(/^[0-9a-f]{64}$/);
      expect(a.size).toBeGreaterThan(0);
      expect(a.url).toMatch(/^https:\/\//);
    }
    for (const m of Object.values(manifest.models)) {
      for (const f of m.files) {
        expect(f.sha256).toMatch(/^[0-9a-f]{64}$/);
        expect(f.size).toBeGreaterThan(0);
        if (!m.archive) expect(f.url).toMatch(/^https:\/\//);
      }
    }
  });

  test("the llama.cpp Windows Vulkan archive and the GGUF equal the digests recorded in provider-conformance.adoc", () => {
    const doc = read("docs/provider-conformance.adoc");
    expect(hexes(doc)).toContain(manifest.llama.assets["win32-x64-vulkan"].sha256);
    expect(hexes(doc)).toContain(manifest.llm.sha256);
  });

  test("the Silero VAD pin equals the one in production-runtime.adoc", () => {
    expect(hexes(read("docs/production-runtime.adoc"))).toContain(manifest.vad.sha256);
  });

  test("the pinned llama.cpp tag matches the release named in the asset names, URLs and provider-conformance.adoc", () => {
    expect(read("docs/provider-conformance.adoc")).toContain(manifest.llama.tag);
    for (const a of Object.values(manifest.llama.assets)) {
      expect(a.file).toContain(manifest.llama.tag);
      expect(a.url).toContain(`/${manifest.llama.tag}/${a.file}`);
    }
  });

  test("every URL ends in the file it names, and the avatar URL is the pinned commit", () => {
    for (const a of allAssets()) expect(a.url.endsWith(`/${a.file}`)).toBe(true);
    expect(manifest.avatar.url).toContain(manifest.avatar.commit);
  });

  test("the model file pins are the ones crates/adapters verifies at every sidecar start", () => {
    // The Rust tests (crates/setup manifest tests, adapters pins tests) assert
    // digest and size equality; this reads the Rust sources as text for the
    // same digest and file name, so a bun-only run catches drift too.
    const rs = read("crates/adapters/src/speech_stt/pins.rs") + read("crates/adapters/src/speech_tts/pins.rs");
    for (const m of Object.values(manifest.models)) {
      for (const f of m.files) {
        expect(rs).toContain(f.sha256);
        expect(rs).toContain(f.file);
        expect(rs.replaceAll("_", "")).toContain(String(f.size));
      }
    }
  });
});

describe("the scripts read the manifest and hold no pins of their own", () => {
  for (const [name, text] of [["fetch-local-profile.mjs", wrapper], ["fetch-sherpa-onnx.mjs", sherpa], ["fetch-model.mjs", avatarFetch]]) {
    test(`${name} loads local-profile-pins.json and embeds no digest or download host`, () => {
      expect(text).toContain("local-profile-pins.json");
      expect(hexes(text)).toEqual([]);
      expect(text).not.toMatch(/https:\/\/(github\.com|huggingface\.co|raw\.githubusercontent\.com)\//);
    });
  }

  test("reuses the existing scripts instead of repeating their pins", () => {
    expect(wrapper).toContain("fetch-sherpa-onnx.mjs");
    expect(wrapper).toContain("fetch-model.mjs");
  });
});

describe("Windows archive extraction", () => {
  // Regression: on Windows with Git Bash first on PATH, `tar` is GNU tar, which
  // reads `I:\path` as a remote host ("Cannot connect to I") and cannot read zip.
  // Both scripts must call the system bsdtar there and pass names relative to cwd.
  for (const [name, text] of [["fetch-local-profile.mjs", wrapper], ["fetch-sherpa-onnx.mjs", sherpa]]) {
    test(`${name} uses the system tar.exe on win32 and no bare tar on an absolute path`, () => {
      expect(text).toContain('"System32", "tar.exe"');
      expect(text).toMatch(/execFileSync\(tar,/);
      expect(text).not.toMatch(/execFileSync\("tar"/);
    });
  }
});

describe("fetch-local-profile.mjs invocation", () => {
  test("refuses to run without a destination directory", () => {
    const run = Bun.spawnSync(["node", join(root, "scripts/fetch-local-profile.mjs")], { stderr: "pipe", stdout: "pipe" });
    expect(run.exitCode).not.toBe(0);
    expect(run.stderr.toString()).toContain("usage:");
  });

  test("refuses a llama.cpp flavour that is not pinned", () => {
    const run = Bun.spawnSync(
      ["node", join(root, "scripts/fetch-local-profile.mjs"), join(root, "target", "never-created"), "--llama-flavor", "metal", "--skip", "llm,vad,sherpa-lib,stt,tts,avatar"],
      { stderr: "pipe", stdout: "pipe" },
    );
    expect(run.exitCode).not.toBe(0);
    expect(run.stderr.toString()).toContain("no pinned llama.cpp asset");
  });
});

describe("local-profile-quickstart.adoc", () => {
  const doc = read("docs/local-profile-quickstart.adoc");
  test("documents every option and step the wrapper and the setup command accept", () => {
    for (const s of ["--llama-flavor", "--skip", "fetch-local-profile.mjs", "aivtuber-setup local", "--dry-run", "--offline", "--force", "aivtuber-setup start"]) {
      expect(doc).toContain(s);
    }
  });
  test("tells the operator about the OpenRAIL-M disclosure duty and the ReazonSpeech provenance", () => {
    expect(doc).toMatch(/machine-generated/);
    expect(doc).toMatch(/Article 30-4/);
    expect(doc).toMatch(/CC-BY-4\.0/);
  });
  test("sets no remote credential in its configuration block", () => {
    expect(doc).not.toMatch(/export AIVTUBER_(OPENAI|TTS|JEV)_API_KEY/);
    expect(doc).toContain("AIVTUBER_OPENAI_AUTH=unauthenticated-loopback");
  });
  test("the archive path needs no Node, Bun or Cargo", () => {
    const section = doc.split("[#download-a-release]")[1].split("\n== 1.")[0];
    expect(section).toContain("aivtuber-setup local");
    expect(section).not.toMatch(/node scripts\//);
  });
});
