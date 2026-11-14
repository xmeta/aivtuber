import { describe, expect, test } from "bun:test";
import { Ajv2020 } from "ajv/dist/2020.js";
import addFormats from "ajv-formats";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import { compatKey } from "../scripts/benchmark-record.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const load = (path) => JSON.parse(readFileSync(join(root, path), "utf8"));

const ajv = new Ajv2020({ allErrors: true, strict: true, strictRequired: false, allowUnionTypes: true });
addFormats(ajv);
const validate = ajv.compile(load("schemas/benchmark-result.schema.json"));

function jsonIn(dir) {
  const abs = join(root, dir);
  return existsSync(abs) ? readdirSync(abs).filter((f) => f.endsWith(".json")).sort() : [];
}

describe("benchmark result schema (#58, #83 S3e)", () => {
  for (const file of jsonIn("examples/benchmarks/result/valid")) {
    test(`valid example ${file} conforms`, () => {
      const ok = validate(load(`examples/benchmarks/result/valid/${file}`));
      expect(validate.errors ?? []).toEqual([]);
      expect(ok).toBe(true);
    });
  }

  for (const file of jsonIn("examples/benchmarks/result/invalid")) {
    test(`invalid example ${file} is rejected`, () => {
      expect(validate(load(`examples/benchmarks/result/invalid/${file}`))).toBe(false);
    });
  }

  test("an unknown mode is still rejected", () => {
    const doc = load("examples/benchmarks/result/valid/phrase-mode-streamed.json");
    expect(validate({ ...doc, mode: "phrase_mode_unknown" })).toBe(false);
  });

  test("phrase-mode results never share a compatibility series with replay modes or each other", () => {
    const doc = load("examples/benchmarks/result/valid/phrase-mode-streamed.json");
    const keys = ["phrase_mode_buffered", "phrase_mode_phrased", "phrase_mode_streamed"].map((mode) =>
      compatKey({ ...doc, mode }),
    );
    expect(new Set(keys).size).toBe(3);
    expect(keys).not.toContain(compatKey({ ...doc, mode: "full_generative" }));
  });
});
