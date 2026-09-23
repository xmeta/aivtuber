// Mock audio/avatar adapters and the replay trace recorder.
//
// Implements roadmap phase 1 "Mock audio/avatar adapters" and "Replay
// harness": adapters produce deterministic, recorded outputs so a session
// can be replayed from the trace without external services. Adapter
// failures degrade independently (architecture section 9).

import type { PlannedPerformance } from "./scheduler.js";
import type { AppliedVariation } from "./rng.js";

export interface AdapterOutput {
  generation: number;
  atMs: number;
  kind: "audio" | "avatar";
  /** Asset id or a deterministic degradation marker. */
  ref: string;
  detail: Record<string, unknown>;
}

export interface MockAudioAdapter {
  readonly available: boolean;
  outputs: AdapterOutput[];
  play(plan: PlannedPerformance, variation: AppliedVariation | null): AdapterOutput;
  stop(atMs: number): AdapterOutput | null;
}

export interface MockAvatarAdapter {
  readonly available: boolean;
  outputs: AdapterOutput[];
  perform(plan: PlannedPerformance, variation: AppliedVariation | null): AdapterOutput;
  stop(atMs: number): AdapterOutput | null;
}

export function createMockAudioAdapter(opts: { available?: boolean } = {}): MockAudioAdapter {
  const available = opts.available ?? true;
  return {
    available,
    outputs: [],
    play(plan, variation) {
      const out: AdapterOutput = {
        generation: plan.generation,
        atMs: plan.startAtMs,
        kind: "audio",
        ref: available ? plan.assetId : "degraded.silent",
        detail: {
          durationMs: available ? Math.round(plan.durationMs * (variation?.speedFactor ?? 1)) : 0,
          startDelayMs: variation?.startDelayMs ?? 0,
        },
      };
      this.outputs.push(out);
      return out;
    },
    stop(atMs) {
      if (!available) return null;
      const out: AdapterOutput = {
        generation: -1,
        atMs,
        kind: "audio",
        ref: "audio.stop",
        detail: {},
      };
      this.outputs.push(out);
      return out;
    },
  };
}

export function createMockAvatarAdapter(opts: { available?: boolean } = {}): MockAvatarAdapter {
  const available = opts.available ?? true;
  return {
    available,
    outputs: [],
    perform(plan, variation) {
      const out: AdapterOutput = {
        generation: plan.generation,
        atMs: plan.startAtMs,
        kind: "avatar",
        ref: available ? plan.assetId : "degraded.idle",
        detail: {
          interruptPointsMs: plan.interruptPointsMs,
          amplitudeFactor: variation?.amplitudeFactor ?? 1,
        },
      };
      this.outputs.push(out);
      return out;
    },
    stop(atMs) {
      if (!available) return null;
      const out: AdapterOutput = {
        generation: -1,
        atMs,
        kind: "avatar",
        ref: "avatar.stop",
        detail: {},
      };
      this.outputs.push(out);
      return out;
    },
  };
}

/** One replay-recorded step: everything that affected a decision. */
export interface ReplayStep {
  atMs: number;
  eventId: string;
  sequence: number;
  action: string;
  assetId: string | null;
  variation: { speedFactor: number; amplitudeFactor: number; startDelayMs: number } | null;
  generation: number | null;
}

/**
 * Records a full session for replay. Given the same trace, seed, and
 * assets, re-running produces an identical recording (domain contract
 * section 9).
 */
export class ReplayRecorder {
  readonly steps: ReplayStep[] = [];

  record(
    atMs: number,
    event: { event_id: string; sequence: number },
    outcome: {
      action: string;
      asset_id: string | null;
      variation: AppliedVariation | null;
      plan: { generation: number } | null;
    },
  ): void {
    this.steps.push({
      atMs,
      eventId: event.event_id,
      sequence: event.sequence,
      action: outcome.action,
      assetId: outcome.asset_id,
      variation: outcome.variation
        ? {
            speedFactor: outcome.variation.speedFactor,
            amplitudeFactor: outcome.variation.amplitudeFactor,
            startDelayMs: outcome.variation.startDelayMs,
          }
        : null,
      generation: outcome.plan ? outcome.plan.generation : null,
    });
  }

  /** Canonical deterministic string for equality checks. */
  fingerprint(): string {
    return JSON.stringify(this.steps);
  }
}
