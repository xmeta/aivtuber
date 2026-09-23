import { describe, expect, test } from "bun:test";

import { normalizeEvent } from "../src/envelope.js";
import { JevAdapter, httpStatusToFailure, wireToDecision } from "../src/adapters-jev.js";
import { ReflexDirector, type ReflexContext, type ReflexDecision } from "../src/reflex.js";

function ctx(): ReflexContext {
  const event = normalizeEvent({
    kind: "chat.message",
    source: "example-chat",
    event_id: "evt-jev-1",
    sequence: 1,
    payload: { text: "やあ" },
  });
  return {
    event,
    performer: { currently_speaking: false, current_interruptible: true, mood_valence: 0.5, arousal: 0.5 },
    recent: { reaction_families: [], seconds_since_direct_reply: 1 },
    retrieval: { candidate_ids: [], best_similarity: 0 },
    nowMs: 1000,
  };
}

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

const okWire = {
  choices: {
    response_route: { value: "cached", confidence: 0.91 },
    reaction: { value: "embarrassed_laugh", confidence: 0.78 },
    gesture: { value: "head_shake_small" },
    attention: { value: "camera" },
  },
  scores: { interrupt: 0.08, cache_reuse: 0.94, importance: 0.37, emotion_intensity: 1.7 },
  model: { alias: "jev-latest", version: "example" },
};

describe("wire mapping", () => {
  test("maps a full Jev wire response to the normalized decision", () => {
    const { decision, error } = wireToDecision(okWire);
    expect(error).toBeUndefined();
    expect(decision?.route).toEqual({ value: "cached", confidence: 0.91 });
    expect(decision?.reaction_family).toBe("embarrassed_laugh");
    expect(decision?.attention_target).toBe("camera");
    // Out-of-range scores are clamped, never trusted.
    expect(decision?.emotion_intensity).toBe(1);
  });

  test("unknown route value is rejected as invalid_request, never guessed", () => {
    const { decision, error } = wireToDecision({
      choices: { response_route: { value: "yolo", confidence: 0.99 } },
    });
    expect(decision).toBeNull();
    expect(error).toBe("unknown_route");
  });

  test("missing route is rejected", () => {
    const { decision } = wireToDecision({});
    expect(decision).toBeNull();
  });

  test("attention fallback is away, scores default to 0", () => {
    const { decision } = wireToDecision({
      choices: { response_route: { value: "silent" } },
      scores: {},
    });
    expect(decision?.attention_target).toBe("away");
    expect(decision?.interrupt_probability).toBe(0);
    expect(decision?.backend.name).toBe("jev");
  });
});

describe("http failure mapping", () => {
  test("maps status codes to typed fallback reasons", () => {
    expect(httpStatusToFailure(429)).toBe("rate_limited");
    expect(httpStatusToFailure(401)).toBe("authentication");
    expect(httpStatusToFailure(403)).toBe("authentication");
    expect(httpStatusToFailure(503)).toBe("overloaded");
    expect(httpStatusToFailure(400)).toBe("invalid_request");
    expect(httpStatusToFailure(500)).toBe("unavailable");
  });
});

describe("JevAdapter over HTTP", () => {
  test("successful evaluation returns normalized decision with measured latency", async () => {
    const adapter = new JevAdapter({
      endpoint: "https://jev.example/decide",
      modelAlias: "jev-latest",
      fetchImpl: async () => jsonResponse(200, okWire),
    });
    const d = await adapter.evaluate(ctx());
    expect(d.route.value).toBe("cached");
    expect(d.backend.model_alias).toBe("jev-latest");
    expect(d.fallback_reason).toBe("none");
  });

  test("HTTP failures become typed fallback decisions, not exceptions", async () => {
    const adapter = new JevAdapter({
      endpoint: "https://jev.example/decide",
      fetchImpl: async () => jsonResponse(429, { error: "slow down" }),
    });
    const d = await adapter.evaluate(ctx());
    expect(d.fallback_reason).toBe("rate_limited");
    expect(d.route.value).toBe("silent");
  });

  test("network failure becomes unavailable", async () => {
    const adapter = new JevAdapter({
      endpoint: "https://jev.example/decide",
      fetchImpl: async () => {
        throw new Error("ECONNREFUSED");
      },
    });
    const d = await adapter.evaluate(ctx());
    expect(d.fallback_reason).toBe("unavailable");
  });

  test("deadline abort propagates so the director records timeout", async () => {
    const adapter = new JevAdapter({
      endpoint: "https://jev.example/decide",
      timeoutMs: 20,
      fetchImpl: (_url, init) =>
        new Promise<Response>((resolve, reject) => {
          init?.signal?.addEventListener("abort", () => {
            const err = new Error("aborted");
            err.name = "AbortError";
            reject(err);
          });
        }),
    });
    const director = new ReflexDirector({ timeoutMs: 200 });
    const outcome = await director.direct(adapter, ctx());
    expect(outcome.action).toBe("fallback_deterministic");
    expect(outcome.fallback_reason).toBe("timeout");
  });

  test("adapter integrates with the director end-to-end", async () => {
    const adapter = new JevAdapter({
      endpoint: "https://jev.example/decide",
      fetchImpl: async () => jsonResponse(200, okWire),
    });
    const outcome = await new ReflexDirector().direct(adapter, ctx());
    // cached + reuse 0.94 >= 0.85 threshold
    expect(outcome.action).toBe("route_cached");
  });

  test("secret is referenced by identifier, never embedded in the request body", async () => {
    let capturedBody: string | undefined;
    let capturedHeaders: Headers | undefined;
    const adapter = new JevAdapter({
      endpoint: "https://jev.example/decide",
      secretRef: "jev-api-key",
      fetchImpl: async (_url, init) => {
        capturedBody = init?.body as string;
        capturedHeaders = new Headers(init?.headers);
        return jsonResponse(200, okWire);
      },
    });
    await adapter.evaluate(ctx());
    const body = JSON.parse(capturedBody ?? "{}");
    expect(JSON.stringify(body)).not.toContain("sk-");
    expect(capturedHeaders?.get("x-secret-ref")).toBe("jev-api-key");
  });
});
