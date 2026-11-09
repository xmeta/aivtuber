import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

// Offline checks for the local-profile provisioning scripts (issue #220 S3).
// Nothing here downloads anything or needs a model.
const root = join(import.meta.dir, "..");
const read = (p) => readFileSync(join(root, p), "utf8");
const wrapper = read("scripts/fetch-local-profile.mjs");
const sherpa = read("scripts/fetch-sherpa-onnx.mjs");

const hexes = (text) => [...text.matchAll(/\b[0-9a-f]{64}\b/g)].map((m) => m[0]);

describe("fetch-local-profile.mjs pins", () => {
  test("every pin is a 64-hex SHA-256", () => {
    for (const m of wrapper.matchAll(/sha256:\s*"([^"]+)"/g)) expect(m[1]).toMatch(/^[0-9a-f]{64}$/);
  });

  test("the llama.cpp Windows Vulkan archive and the GGUF equal the digests recorded in provider-conformance.adoc", () => {
    const doc = read("docs/provider-conformance.adoc");
    const winVulkan = /"win32-x64-vulkan":\s*\{[^}]*sha256:\s*"([0-9a-f]{64})"/.exec(wrapper)[1];
    const gguf = /GGUF = \{[^}]*sha256:\s*"([0-9a-f]{64})"/s.exec(wrapper)[1];
    expect(hexes(doc)).toContain(winVulkan);
    expect(hexes(doc)).toContain(gguf);
  });

  test("the Silero VAD pin equals the one in production-runtime.adoc", () => {
    const doc = read("docs/production-runtime.adoc");
    const vad = /VAD = \{[^}]*sha256:\s*"([0-9a-f]{64})"/s.exec(wrapper)[1];
    expect(hexes(doc)).toContain(vad);
  });

  test("the pinned llama.cpp tag matches the release named in the asset names and in provider-conformance.adoc", () => {
    const tag = /LLAMA_TAG = "([^"]+)"/.exec(wrapper)[1];
    expect(read("docs/provider-conformance.adoc")).toContain(tag);
    for (const m of wrapper.matchAll(/name: "(llama-[^"]+)"/g)) expect(m[1]).toContain(tag);
  });

  test("reuses the existing scripts instead of repeating their pins", () => {
    expect(wrapper).toContain("fetch-sherpa-onnx.mjs");
    expect(wrapper).toContain("fetch-model.mjs");
    const own = new Set(hexes(wrapper));
    for (const h of hexes(sherpa)) expect(own.has(h)).toBe(false);
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
  test("documents every option and step the wrapper accepts", () => {
    for (const s of ["--llama-flavor", "--skip", "fetch-local-profile.mjs"]) expect(doc).toContain(s);
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
});
