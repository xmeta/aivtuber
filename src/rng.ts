// Seeded deterministic PRNG and bounded variation.
//
// Implements docs/architecture.adoc section 8: variation MUST be
// deterministic under a recorded random seed so sessions can be replayed.

export type SeededRng = () => number;

/** mulberry32: tiny, fast, fully deterministic. */
export function createRng(seed: number): SeededRng {
  let a = seed >>> 0;
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export interface VariationSpec {
  speed_pct?: number;
  amplitude_pct?: number;
  start_delay_ms?: number;
}

export interface AppliedVariation {
  speedFactor: number;
  amplitudeFactor: number;
  startDelayMs: number;
}

/**
 * Apply bounded procedural variation. Each factor stays within the
 * asset's declared bounds: speed/amplitude in ±pct%, delay in 0..max.
 */
export function applyVariation(spec: VariationSpec, rng: SeededRng): AppliedVariation {
  const speedPct = spec.speed_pct ?? 0;
  const amplitudePct = spec.amplitude_pct ?? 0;
  const maxDelay = spec.start_delay_ms ?? 0;
  return {
    speedFactor: 1 + (rng() * 2 - 1) * (speedPct / 100),
    amplitudeFactor: 1 + (rng() * 2 - 1) * (amplitudePct / 100),
    startDelayMs: Math.round(rng() * maxDelay),
  };
}
