import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { createRng, applyVariation } from "../src/rng.js";

interface RngGoldenFixture {
  schema_version: string;
  algorithm: string;
  stream_vectors: Array<{ label: string; seed: number; raw_u32: number[]; values: number[] }>;
  variation_cases: Array<{
    label: string;
    seed: number;
    spec: { speed_pct: number; amplitude_pct: number; start_delay_ms: number };
    expected: { speed_factor: number; amplitude_factor: number; start_delay_ms: number };
  }>;
}

const goldenFixture: RngGoldenFixture = JSON.parse(
  readFileSync(join(import.meta.dir, "..", "examples", "rng", "mulberry32-golden.json"), "utf8"),
);

describe("seeded rng", () => {
  test("same seed produces identical streams", () => {
    const a = createRng(42);
    const b = createRng(42);
    const seqA = Array.from({ length: 10 }, () => a());
    const seqB = Array.from({ length: 10 }, () => b());
    expect(seqA).toEqual(seqB);
  });

  test("different seeds diverge", () => {
    const a = createRng(1);
    const b = createRng(2);
    expect(Array.from({ length: 5 }, () => a())).not.toEqual(Array.from({ length: 5 }, () => b()));
  });

  test("values stay in [0,1)", () => {
    const rng = createRng(7);
    for (let i = 0; i < 1000; i++) {
      const v = rng();
      expect(v).toBeGreaterThanOrEqual(0);
      expect(v).toBeLessThan(1);
    }
  });
});

describe("applyVariation", () => {
  test("stays within declared bounds", () => {
    const spec = { speed_pct: 10, amplitude_pct: 15, start_delay_ms: 120 };
    for (const seed of [0, 1, 2, 12345, 999999]) {
      const v = applyVariation(spec, createRng(seed));
      expect(v.speedFactor).toBeGreaterThanOrEqual(0.9);
      expect(v.speedFactor).toBeLessThanOrEqual(1.1);
      expect(v.amplitudeFactor).toBeGreaterThanOrEqual(0.85);
      expect(v.amplitudeFactor).toBeLessThanOrEqual(1.15);
      expect(v.startDelayMs).toBeGreaterThanOrEqual(0);
      expect(v.startDelayMs).toBeLessThanOrEqual(120);
    }
  });

  test("same seed reproduces the same variation", () => {
    const spec = { speed_pct: 10, amplitude_pct: 15, start_delay_ms: 120 };
    expect(applyVariation(spec, createRng(31337))).toEqual(applyVariation(spec, createRng(31337)));
  });

  test("empty spec is identity", () => {
    const v = applyVariation({}, createRng(99));
    expect(v.speedFactor).toBe(1);
    expect(v.amplitudeFactor).toBe(1);
    expect(v.startDelayMs).toBe(0);
  });
});

describe("shared golden fixture (#41, #506)", () => {
  test("stream vectors match mulberry32-golden.json across boundary seeds", () => {
    expect(goldenFixture.schema_version).toBe("0.1.0");
    expect(goldenFixture.algorithm).toBe("mulberry32");
    expect(goldenFixture.stream_vectors.length).toBeGreaterThan(0);

    for (const vector of goldenFixture.stream_vectors) {
      const rng = createRng(vector.seed);
      const actual = vector.values.map(() => rng());
      expect(actual.map((v) => v * 4294967296)).toEqual(vector.raw_u32);
      expect(actual).toEqual(vector.values);
    }
  });

  test("variation cases match mulberry32-golden.json across boundary seeds", () => {
    expect(goldenFixture.variation_cases.length).toBeGreaterThan(0);

    for (const entry of goldenFixture.variation_cases) {
      const actual = applyVariation(entry.spec, createRng(entry.seed));
      expect(actual.speedFactor).toBe(entry.expected.speed_factor);
      expect(actual.amplitudeFactor).toBe(entry.expected.amplitude_factor);
      expect(actual.startDelayMs).toBe(entry.expected.start_delay_ms);
    }
  });
});
