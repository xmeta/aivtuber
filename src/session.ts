// Session runner: event bus -> deterministic performer -> mock adapters.
//
// Ties the phase 1 pieces together and records everything needed for
// replay. Failures of individual adapters degrade independently and are
// recorded, never thrown into the scheduling loop (architecture section 9).

import type { EventEnvelope } from "./envelope.js";
import { EventBus } from "./event-bus.js";
import { DeterministicPerformer, type PerformerConfig } from "./performer.js";
import { AssetStore } from "./asset-store.js";
import { createMockAudioAdapter, createMockAvatarAdapter, ReplayRecorder, type MockAudioAdapter, type MockAvatarAdapter, type ReplayStep } from "./adapters.js";

export interface SessionConfig extends PerformerConfig {
  bus?: { contentQueueLimit?: number };
  adapters?: { audioAvailable?: boolean; avatarAvailable?: boolean };
}

export interface SessionResult {
  recorder: ReplayRecorder;
  audio: MockAudioAdapter;
  avatar: MockAvatarAdapter;
  busStats: { buffered: number; controlBuffered: number; dropped: number };
  fingerprint: string;
}

/**
 * Run a recorded event trace through the bus, performer, and mock
 * adapters. Deterministic: same inputs -> identical fingerprint.
 */
export function runSession(
  trace: { event: EventEnvelope; atMs: number }[],
  assets: AssetStore,
  config: SessionConfig,
): SessionResult {
  const bus = new EventBus(config.bus);
  const performer = new DeterministicPerformer(assets, config);
  const audio = createMockAudioAdapter({ available: config.adapters?.audioAvailable ?? true });
  const avatar = createMockAvatarAdapter({ available: config.adapters?.avatarAvailable ?? true });
  const recorder = new ReplayRecorder();

  for (const { event, atMs } of trace) {
    bus.publish(event);
  }

  for (const event of bus.drain()) {
    // Control-plane commands keep their recorded time; content events run
    // in drained order. The performer clamps its clock monotonically.
    const step = trace.find((t) => t.event.event_id === event.event_id);
    const atMs = Math.max(performer.currentTimeMs, step?.atMs ?? performer.currentTimeMs);
    const outcome = performer.handle(event, atMs);
    recorder.record(atMs, event, outcome);

    if (outcome.action === "scheduled" && outcome.plan) {
      audio.play(outcome.plan, outcome.variation);
      avatar.perform(outcome.plan, outcome.variation);
    } else if (outcome.action === "operator_stop") {
      audio.stop(atMs);
      avatar.stop(atMs);
    }
  }

  return {
    recorder,
    audio,
    avatar,
    busStats: bus.stats,
    fingerprint: recorder.fingerprint(),
  };
}
