import { describe, expect, test } from "bun:test";

import { normalizeEvent, type EventEnvelope } from "../src/envelope.js";
import { EventBus } from "../src/event-bus.js";
import { AssetStore, type PerformanceAsset } from "../src/asset-store.js";
import { runSession } from "../src/session.js";

const asset: PerformanceAsset = {
  schema_version: "0.1.0",
  id: "gratitude.happy.01",
  intent: "gratitude.viewer",
  class: "phrase",
  variant_group: "gratitude.viewer",
  speech: { text: "ありがとう！", audio_ref: "audio://phrases/thanks-01.opus", duration_ms: 700 },
  gaze: "camera",
  timeline: [],
  interrupt_points_ms: [0, 700],
  variation: { start_delay_ms: 100 },
  compatibility: { compiler_version: "0.1.0", voice_model: "example-voice-v1" },
  provenance: { generated: false, generator: null, created_at: null },
};

function chat(sequence: number, intent?: string): EventEnvelope {
  return normalizeEvent({
    kind: "chat.message",
    source: "example-chat",
    event_id: `evt-${sequence}`,
    sequence,
    payload: intent ? { intent } : { text: "hi" },
  });
}

function donation(sequence: number): EventEnvelope {
  return normalizeEvent({
    kind: "chat.donation",
    source: "example-donation",
    event_id: `evt-don-${sequence}`,
    sequence,
    payload: { amount: 1000 },
  });
}

function operatorStop(sequence: number): EventEnvelope {
  return normalizeEvent({
    kind: "operator.command",
    source: "local-operator-hotkey",
    event_id: `evt-op-${sequence}`,
    sequence,
    payload: { action: "stop" },
    authorization: { principal: "operator:local", method: "operator_hotkey", capabilities: ["performer.stop"] },
  });
}

function makeAssets(): AssetStore {
  const store = new AssetStore();
  store.put(asset);
  return store;
}

describe("EventBus ordering", () => {
  test("control plane drains before content, then rank then sequence", () => {
    const bus = new EventBus();
    bus.publish(chat(3, "gratitude.viewer"));
    bus.publish(chat(4, "gratitude.viewer"));
    bus.publish(operatorStop(1));
    bus.publish(donation(2));
    const drained = bus.drain();
    expect(drained.map((e) => e.event_id)).toEqual(["evt-op-1", "evt-don-2", "evt-3", "evt-4"]);
  });

  test("overflow drops the lowest-priority buffered event", () => {
    const bus = new EventBus({ contentQueueLimit: 2 });
    bus.publish(chat(1, "gratitude.viewer"));
    bus.publish(chat(2, "gratitude.viewer"));
    const result = bus.publish(chat(3, "gratitude.viewer"));
    expect(result).toBe("dropped_overflow");
    expect(bus.stats.dropped).toBe(1);
    // One of the three chats was dropped; two remain.
    expect(bus.drain().length).toBe(2);
  });

  test("content queue limit never affects the control plane", () => {
    const bus = new EventBus({ contentQueueLimit: 1 });
    bus.publish(chat(1, "gratitude.viewer"));
    expect(bus.publish(operatorStop(2))).toBe("queued");
    expect(bus.stats.controlBuffered).toBe(1);
  });
});

describe("session end-to-end (bus -> performer -> adapters)", () => {
  const trace = [
    { event: chat(1, "gratitude.viewer"), atMs: 0 },
    { event: donation(2), atMs: 2000 },
    { event: chat(3, "gratitude.viewer"), atMs: 5000 },
  ];

  test("deterministic fingerprint across runs with the same seed", () => {
    const a = runSession(trace, makeAssets(), { compiler_version: "0.1.0", voice_model: "example-voice-v1", seed: 4242 });
    const b = runSession(trace, makeAssets(), { compiler_version: "0.1.0", voice_model: "example-voice-v1", seed: 4242 });
    expect(a.fingerprint).toBe(b.fingerprint);
  });

  test("adapter outputs follow scheduled plans", () => {
    const session = runSession(trace, makeAssets(), {
      compiler_version: "0.1.0",
      voice_model: "example-voice-v1",
      seed: 4242,
    });
    // chat(1) and chat(3) schedule; donation(2) re-requests the same asset
    // inside its cooldown and is deterministically rejected.
    expect(session.audio.outputs.length).toBe(2);
    expect(session.avatar.outputs.length).toBe(2);
    expect(session.audio.outputs.every((o) => o.ref === "gratitude.happy.01")).toBe(true);
    expect(session.audio.outputs.every((o) => o.kind === "audio")).toBe(true);
    const actions = session.recorder.steps.map((s) => s.action);
    expect(actions).toContain("cooldown_rejected");
  });

  test("operator stop propagates to adapters", () => {
    const session = runSession(
      [
        { event: chat(1, "gratitude.viewer"), atMs: 0 },
        { event: operatorStop(2), atMs: 100 },
      ],
      makeAssets(),
      { compiler_version: "0.1.0", voice_model: "example-voice-v1", seed: 1 },
    );
    const audioRefs = session.audio.outputs.map((o) => o.ref);
    expect(audioRefs).toContain("audio.stop");
    const avatarRefs = session.avatar.outputs.map((o) => o.ref);
    expect(avatarRefs).toContain("avatar.stop");
  });

  test("unavailable adapters degrade instead of throwing", () => {
    const session = runSession(trace, makeAssets(), {
      compiler_version: "0.1.0",
      voice_model: "example-voice-v1",
      seed: 4242,
      adapters: { audioAvailable: false, avatarAvailable: false },
    });
    // Two schedule, the cooldown-window re-request is deterministically rejected.
    expect(session.recorder.steps.filter((s) => s.action === "scheduled").length).toBe(2);
    expect(session.audio.outputs.every((o) => o.ref === "degraded.silent")).toBe(true);
    expect(session.avatar.outputs.every((o) => o.ref === "degraded.idle")).toBe(true);
  });

  test("replay steps record full decision context", () => {
    const session = runSession(trace, makeAssets(), {
      compiler_version: "0.1.0",
      voice_model: "example-voice-v1",
      seed: 7,
    });
    const steps: ReplayStepLike[] = session.recorder.steps as ReplayStepLike[];
    expect(steps.length).toBe(3);
    for (const step of steps) {
      expect(step.eventId).toMatch(/^evt/);
      expect(typeof step.atMs).toBe("number");
      expect(typeof step.sequence).toBe("number");
    }
    const scheduled = steps.find((s) => s.action === "scheduled")!;
    expect(scheduled.assetId).toBe("gratitude.happy.01");
    expect(scheduled.variation).not.toBeNull();
    expect(scheduled.generation).toBeGreaterThan(0);
  });
});

type ReplayStepLike = {
  atMs: number;
  eventId: string;
  sequence: number;
  action: string;
  assetId: string | null;
  variation: { speedFactor: number; amplitudeFactor: number; startDelayMs: number } | null;
  generation: number | null;
};
