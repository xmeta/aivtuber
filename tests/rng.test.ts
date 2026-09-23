import { describe, expect, test } from "bun:test";

import { createRng, applyVariation } from "../src/rng.js";

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
