import { describe, expect, test } from "bun:test";

import { normalizeEvent } from "../src/envelope.js";
import {
  ReflexDirector,
  fallbackDecision,
  type DecisionEngine,
  type ReflexContext,
  type ReflexDecision,
} from "../src/reflex.js";

function chatCtx(overrides: Partial<ReflexContext["performer"]> = {}): ReflexContext {
  const event = normalizeEvent({
    kind: "chat.message",
    source: "example-chat",
    event_id: `evt-${Math.random().toString(36).slice(2, 8)}`,
    sequence: 1,
    payload: { text: "テスト" },
  });
  return {
    event,
    performer: {
      currently_speaking: true,
      current_interruptible: false,
      mood_valence: 0.5,
      arousal: 0.5,
      ...overrides,
    },
    recent: { reaction_families: [], seconds_since_direct_reply: 2 },
    retrieval: { candidate_ids: ["reaction.tease.01"], best_similarity: 0.9 },
    nowMs: 1000,
  };
}

function decision(overrides: Partial<ReflexDecision> = {}): ReflexDecision {
  return {
    route: { value: "reaction", confidence: 0.9 },
    reaction_family: "laugh",
    gesture_family: "nod",
    attention_target: "camera",
    interrupt_probability: 0.1,
    cache_reuse_probability: 0.5,
    importance: 0.4,
    emotion_intensity: 0.6,
    backend: { name: "jev", model_alias: "jev-latest", model_version: "example" },
    latency_ms: 12,
    fallback_reason: "none",
    ...overrides,
  };
}

function engine(result: ReflexDecision | Promise<ReflexDecision> | Error): DecisionEngine {
  return {
    name: "jev",
    async evaluate() {
      if (result instanceof Error) throw result;
      if (result instanceof Promise) return result;
      return result;
    },
  };
}

describe("ReflexDirector deterministic policy", () => {
  test("engine unavailable -> deterministic fallback without throwing", async () => {
    const outcome = await new ReflexDirector().direct(null, chatCtx());
    expect(outcome.action).toBe("fallback_deterministic");
    expect(outcome.fallback_reason).toBe("unavailable");
    expect(outcome.decision?.backend.name).toBe("deterministic");
    expect(outcome.decision?.route.value).toBe("silent");
  });

  test("timeout -> deterministic fallback (normal control flow)", async () => {
    const slow: DecisionEngine = {
      name: "jev",
      evaluate: () => new Promise<ReflexDecision>((resolve) => setTimeout(() => resolve(decision()), 500)),
    };
    const outcome = await new ReflexDirector({ timeoutMs: 30 }).direct(slow, chatCtx());
    expect(outcome.action).toBe("fallback_deterministic");
    expect(outcome.fallback_reason).toBe("timeout");
  });

  test("low route confidence -> fallback", async () => {
    const outcome = await new ReflexDirector().direct(engine(decision({ route: { value: "llm", confidence: 0.2 } })), chatCtx());
    expect(outcome.action).toBe("fallback_deterministic");
    expect(outcome.fallback_reason).toBe("low_confidence");
  });

  test("cache reuse below threshold -> deterministic fallback", async () => {
    const outcome = await new ReflexDirector({ reuseThreshold: 0.85 }).direct(
      engine(decision({ route: { value: "cached", confidence: 0.9 }, cache_reuse_probability: 0.6 })),
      chatCtx(),
    );
    expect(outcome.action).toBe("fallback_deterministic");
  });

  test("cache reuse at/above threshold -> route_cached", async () => {
    const outcome = await new ReflexDirector().direct(
      engine(decision({ route: { value: "cached", confidence: 0.91 }, cache_reuse_probability: 0.94 })),
      chatCtx(),
    );
    expect(outcome.action).toBe("route_cached");
  });

  test("interrupt policy stays in code: immediate requires interruptible", async () => {
    const d = decision({ interrupt_probability: 0.99 });
    const interruptible = await new ReflexDirector().direct(engine(d), chatCtx({ current_interruptible: true }));
    expect(interruptible.action).toBe("interrupt_immediate");

    const notInterruptible = await new ReflexDirector().direct(engine(d), chatCtx({ current_interruptible: false }));
    // Not interruptible: falls through to route handling (reaction).
    expect(notInterruptible.action).toBe("route_reaction");
  });

  test("boundary interrupt defers to phrase boundary", async () => {
    const outcome = await new ReflexDirector().direct(
      engine(decision({ interrupt_probability: 0.75 })),
      chatCtx({ current_interruptible: true }),
    );
    expect(outcome.action).toBe("interrupt_at_boundary");
  });

  test("llm route is not preempted by interrupt action", async () => {
    const outcome = await new ReflexDirector().direct(
      engine(decision({ route: { value: "llm", confidence: 0.9 }, interrupt_probability: 0.99 })),
      chatCtx({ current_interruptible: true }),
    );
    expect(outcome.action).toBe("route_llm");
  });

  test("engine-reported failure reason is honored", async () => {
    const outcome = await new ReflexDirector().direct(
      engine(decision({ fallback_reason: "rate_limited" })),
      chatCtx(),
    );
    expect(outcome.action).toBe("fallback_deterministic");
    expect(outcome.fallback_reason).toBe("rate_limited");
  });

  test("fallbackDecision shape matches the normalized contract", () => {
    const ctx = chatCtx();
    const d = fallbackDecision(ctx, "timeout", 42);
    expect(d.route.value).toBe("silent");
    expect(d.route.confidence).toBeNull();
    expect(d.fallback_reason).toBe("timeout");
    expect(d.latency_ms).toBe(42);
  });
});
