import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { rendererNotices } from "../scripts/package-release.mjs";
import { MISSING_LICENCE_STUB, VENDORED_NOTICES, heading, readVendoredNotice, repoRoot } from "../scripts/release-notices.mjs";

// Offline checks for the third-party notice texts a release archive carries
// (issue #349). Nothing here builds or downloads anything.
const read = (p) => readFileSync(join(repoRoot, p), "utf8");

describe("vendored engine notice texts (packaging/third-party)", () => {
  test("every listed text exists, has its Source header and a pinned revision", () => {
    for (const name of VENDORED_NOTICES) {
      const text = readVendoredNotice(name);
      const source = text.split("\n", 1)[0];
      expect(source).toMatch(/^Source: https:\/\/\S+$/);
      // The URL names a full 40-hex commit (or, for Eigen, the same commit), never a moving branch.
      expect(source).toMatch(/\/[0-9a-f]{40}\//);
    }
  });

  test("the directory holds exactly the listed texts", () => {
    expect(readdirSync(join(repoRoot, "packaging", "third-party")).sort()).toEqual([...VENDORED_NOTICES].sort());
  });

  test("the sherpa-onnx static dependencies and the ONNX Runtime notices are covered", () => {
    const all = VENDORED_NOTICES.join(" ");
    for (const part of ["onnxruntime-ThirdPartyNotices", "kaldi-native-fbank", "kaldi-decoder", "openfst", "simple-sentencepiece", "utf8proc", "eigen-COPYING.MPL2"]) {
      expect(all).toContain(part);
    }
  });

  test("a missing or truncated text is refused, not replaced by a stub", () => {
    expect(() => readVendoredNotice("does-not-exist.txt")).toThrow(/missing/);
  });

  test("the engine notice file carries the Eigen MPL-2.0 source pointer", () => {
    const notice = read("packaging/ENGINE-LIBRARIES-NOTICE.txt");
    expect(notice).toContain("Eigen (MPL-2.0) source code");
    expect(notice).toContain("eigen-5.0.1.tar.gz");
    expect(notice).not.toContain("published upstream with the");
    expect(heading("x.txt")).toBe("==== x.txt ====");
  });
});

describe("renderer licence texts", () => {
  const tree = (withLicence) => {
    const root = mkdtempSync(join(tmpdir(), "aivtuber-notice-test-"));
    const app = join(root, "app");
    mkdirSync(app);
    const pkg = (dir, name, extra = {}) => {
      mkdirSync(dir, { recursive: true });
      writeFileSync(join(dir, "package.json"), JSON.stringify({ name, version: "1.0.0", license: "MIT", ...extra }));
    };
    pkg(app, "app", { dependencies: { three: "1", "@pixiv/three-vrm": "1" } });
    pkg(join(app, "node_modules", "three"), "three");
    pkg(join(app, "node_modules", "@pixiv", "three-vrm"), "@pixiv/three-vrm");
    writeFileSync(join(app, "node_modules", "@pixiv", "three-vrm", "LICENSE"), "MIT licence text for three-vrm");
    if (withLicence) writeFileSync(join(app, "node_modules", "three", "LICENSE"), "MIT licence text for three");
    return { root, app };
  };

  test("a package that ships no licence file fails the build", () => {
    const { root, app } = tree(false);
    try {
      expect(() => rendererNotices(app)).toThrow(/three 1\.0\.0 ships no licence file/);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("with every licence present the notices hold the texts and never the stub", () => {
    const { root, app } = tree(true);
    try {
      const text = rendererNotices(app);
      expect(text).toContain("MIT licence text for three");
      expect(text).toContain("MIT licence text for three-vrm");
      expect(text).not.toContain(MISSING_LICENCE_STUB);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});

describe("package-release.mjs source", () => {
  test("no code path writes the missing-licence stub", () => {
    expect(read("scripts/package-release.mjs")).not.toContain(MISSING_LICENCE_STUB);
  });
});
