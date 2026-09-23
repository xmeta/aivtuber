import { describe, expect, test } from "bun:test";

import { normalizeEvent, type EventEnvelope } from "../src/envelope.js";
import { AssetStore } from "../src/asset-store.js";
import { DeterministicPerformer, type EventOutcome } from "../src/performer.js";
import type { PerformanceAsset } from "../src/asset-store.js";

const surpriseAsset: PerformanceAsset = {
  schema_version: "0.1.0",
  id: "reaction.surprise.01",
  intent: "surprise",
  class: "reaction",
  variant_group: "surprise",
  speech: { text: "え、嘘でしょう！", audio_ref: "audio://reactions/surprise-01.opus", duration_ms: 940 },
  expression: { preset: "eyes_wide", intensity: 0.8 },
  gaze: "camera",
  timeline: [{ at_ms: 0, event: "speech.start" }],
  interrupt_points_ms: [0, 620, 940],
  variation: { speed_pct: 10, amplitude_pct: 15, start_delay_ms: 120 },
  compatibility: { compiler_version: "0.1.0", voice_model: "example-voice-v1" },
  provenance: { generated: false, generator: null, created_at: null },
};

const gratitudeAsset: PerformanceAsset = {
  ...surpriseAsset,
  id: "gratitude.happy.01",
  intent: "gratitude.viewer",
  variant_group: "gratitude.viewer",
  speech: { text: "ありがとう！", audio_ref: "audio://phrases/thanks-01.opus", duration_ms: 700 },
  interrupt_points_ms: [0, 700],
};

function chatEvent(sequence: number, intent?: string): EventEnvelope {
  return normalizeEvent({
    kind: "chat.message",
    source: "example-chat",
    event_id: `evt-chat-${sequence}`,
    sequence,
    payload: intent ? { intent } : { text: "hello" },
  });
}

function donationEvent(sequence: number): EventEnvelope {
  return normalizeEvent({
    kind: "chat.donation",
    source: "example-donation",
    event_id: `evt-don-${sequence}`,
    sequence,
    payload: { amount: 500 },
  });
}

function makePerformer(seed = 1234): DeterministicPerformer {
  const store = new AssetStore();
  store.put(surpriseAsset);
  store.put(gratitudeAsset);
  return new DeterministicPerformer(store, { compiler_version: "0.1.0", voice_model: "example-voice-v1", seed });
}

describe("deterministic replay (roadmap phase 1 exit criterion)", () => {
  const trace = [
    { event: chatEvent(1, "surprise"), atMs: 0 },
    { event: donationEvent(2), atMs: 1200 },
    { event: chatEvent(3, "surprise"), atMs: 3000 },
    { event: chatEvent(4, "surprise"), atMs: 6000 },
  ];

  test("same trace + seed produces the identical plan", () => {
    const runA = makePerformer(777).runTrace(trace);
    const runB = makePerformer(777).runTrace(trace);
    expect(JSON.stringify(runA)).toBe(JSON.stringify(runB));
  });

  test("different seed changes variation but not structure", () => {
    const runA = makePerformer(777).runTrace(trace);
    const runB = makePerformer(778).runTrace(trace);
    expect(runA.map((o) => o.action)).toEqual(runB.map((o) => o.action));
    expect(runA.map((o) => o.asset_id)).toEqual(runB.map((o) => o.asset_id));
    const varsA = runA.filter((o) => o.variation).map((o) => JSON.stringify(o.variation));
    const varsB = runB.filter((o) => o.variation).map((o) => JSON.stringify(o.variation));
    expect(varsA).not.toEqual(varsB);
  });

  test("outcome shape is well-formed", () => {
    const run: EventOutcome[] = makePerformer(42).runTrace(trace);
    expect(run[0].action).toBe("scheduled");
    expect(run[0].asset_id).toBe("reaction.surprise.01");
    expect(run[0].plan?.interruptPointsMs).toEqual([0, 620, 940]);
    expect(run[1].action).toBe("scheduled"); // donation -> gratitude asset
    expect(run[1].asset_id).toBe("gratitude.happy.01");
    // The paid reaction was deferred by min spacing and still playing at 3s;
    // a lower-priority conversation reaction must not interrupt it.
    expect(run[2].action).toBe("priority_rejected");
    expect(run[2].asset_id).toBe("reaction.surprise.01");
    expect(run[3].action).toBe("scheduled"); // once the donation ended
  });

  test("events without intent or mapping resolve no asset", () => {
    const run = makePerformer().runTrace([{ event: chatEvent(10), atMs: 0 }]);
    expect(run[0].action).toBe("no_asset");
  });
});

describe("scheduler arbitration", () => {
  test("operator stop cancels the playing performance deterministically", () => {
    const store = new AssetStore();
    store.put(surpriseAsset);
    const performer = new DeterministicPerformer(store, {
      compiler_version: "0.1.0",
      voice_model: "example-voice-v1",
      seed: 5,
    });

    performer.runTrace([{ event: chatEvent(1, "surprise"), atMs: 0 }]);
    expect(performer.scheduler.queue[0]?.status).toBe("playing");

    const stop = normalizeEvent({
      kind: "operator.command",
      source: "local-operator-hotkey",
      event_id: "evt-op-1",
      sequence: 2,
      payload: { action: "stop" },
      authorization: { principal: "operator:local", method: "operator_hotkey", capabilities: ["performer.stop"] },
    });
    const outcome = performer.handle(stop, 300);
    expect(outcome.action).toBe("operator_stop");
    expect(performer.scheduler.queue[0]?.status).toBe("cancelled");
  });

  test("unauthorized operator commands are dropped, never executed", () => {
    const store = new AssetStore();
    const performer = new DeterministicPerformer(store, { compiler_version: "0.1.0", seed: 5 });
    const forged = normalizeEvent({
      kind: "operator.command",
      source: "local-operator-hotkey",
      event_id: "evt-op-2",
      sequence: 3,
      payload: { action: "obs.control" },
      authorization: { principal: "operator:local", method: "operator_hotkey", capabilities: ["performer.stop"] },
    });
    expect(performer.handle(forged, 0).action).toBe("dropped");
  });
});
