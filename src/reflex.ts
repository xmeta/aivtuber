// Reflex layer: provider-neutral DecisionEngine contract and the
// ReflexDirector policy.
//
// Implements docs/domain-contract.adoc sections 5-7 and
// docs/jev-routing.adoc sections 4-6:
//   - the domain depends on a generic DecisionEngine; Jev wire types stay
//     inside the adapter;
//   - Jev returns evidence; deterministic code owns final policy;
//   - a strict deadline makes timeout a normal control-flow event;
//   - low confidence or timeout falls back to deterministic defaults.

import type { EventEnvelope } from "./envelope.js";

export type ResponseRoute = "silent" | "reaction" | "cached" | "template" | "llm";
export type AttentionTarget = "camera" | "chat" | "game" | "speaker" | "away";
export type FallbackReason =
  | "none"
  | "timeout"
  | "unavailable"
  | "rate_limited"
  | "overloaded"
  | "authentication"
  | "invalid_request"
  | "low_confidence"
  | "policy_override"
  | "operator_override";

export interface ReflexDecision {
  route: { value: ResponseRoute; confidence: number | null };
  reaction_family: string | null;
  gesture_family: string | null;
  attention_target: AttentionTarget;
  interrupt_probability: number;
  cache_reuse_probability: number;
  importance: number;
  emotion_intensity: number;
  backend: { name: string; model_alias: string | null; model_version: string | null };
  latency_ms: number;
  fallback_reason: FallbackReason;
}

/** Compact state snapshot; never the full transcript (jev-routing section 3). */
export interface ReflexContext {
  event: EventEnvelope;
  performer: {
    currently_speaking: boolean;
    current_interruptible: boolean;
    mood_valence: number;
    arousal: number;
  };
  recent: { reaction_families: string[]; seconds_since_direct_reply: number | null };
  retrieval: { candidate_ids: string[]; best_similarity: number };
  nowMs: number;
}

/**
 * Generic decision-engine boundary. Jev-specific request/response DTOs
 * remain inside the adapter implementing this interface.
 */
export interface DecisionEngine {
  readonly name: string;
  /** Evaluate one event; must return within the director's deadline. */
  evaluate(ctx: ReflexContext): Promise<ReflexDecision>;
}

export interface ReflexPolicyConfig {
  /** Strict deadline in ms; timeout is normal control flow. */
  timeoutMs?: number;
  /** Reuse probability at or above which a semantic candidate is accepted. */
  reuseThreshold?: number;
  /** Route confidence below which we fall back deterministically. */
  minRouteConfidence?: number;
  /** Interrupt probability that triggers an immediate interrupt. */
  interruptImmediate?: number;
  /** Interrupt probability that defers to the next phrase boundary. */
  interruptBoundary?: number;
}

export interface ReflexOutcome {
  decision: ReflexDecision | null;
  /** Policy applied to the decision: what the runtime will actually do. */
  action:
    | "interrupt_immediate"
    | "interrupt_at_boundary"
    | "keep_current"
    | "route_cached"
    | "route_template"
    | "route_llm"
    | "route_reaction"
    | "silent"
    | "fallback_deterministic";
  fallback_reason: FallbackReason;
  latency_ms: number;
}

/** Deterministic fallback when the reflex layer cannot answer. */
export function fallbackDecision(ctx: ReflexContext, reason: FallbackReason, latency_ms: number): ReflexDecision {
  return {
    route: { value: "silent", confidence: null },
    reaction_family: null,
    gesture_family: null,
    attention_target: "away",
    interrupt_probability: 0,
    cache_reuse_probability: 0,
    importance: 0.1,
    emotion_intensity: 0,
    backend: { name: "deterministic", model_alias: null, model_version: null },
    latency_ms,
    fallback_reason: reason,
  };
}

export class ReflexDirector {
  private readonly timeoutMs: number;
  private readonly reuseThreshold: number;
  private readonly minRouteConfidence: number;
  private readonly interruptImmediate: number;
  private readonly interruptBoundary: number;

  constructor(config: ReflexPolicyConfig = {}) {
    this.timeoutMs = config.timeoutMs ?? 250;
    this.reuseThreshold = config.reuseThreshold ?? 0.85;
    this.minRouteConfidence = config.minRouteConfidence ?? 0.5;
    this.interruptImmediate = config.interruptImmediate ?? 0.95;
    this.interruptBoundary = config.interruptBoundary ?? 0.7;
  }

  /**
   * Evaluate via the engine, apply deterministic policy, and never throw.
   * Engine failure/timeout produces the deterministic fallback.
   */
  async direct(engine: DecisionEngine | null, ctx: ReflexContext): Promise<ReflexOutcome> {
    const started = ctx.nowMs;

    if (engine === null) {
      const decision = fallbackDecision(ctx, "unavailable", 0);
      return { decision, action: "fallback_deterministic", fallback_reason: "unavailable", latency_ms: 0 };
    }

    let decision: ReflexDecision;
    try {
      decision = await this.withTimeout(engine.evaluate(ctx), ctx.nowMs);
    } catch {
      const latency = ctx.nowMs - started;
      const fallback = fallbackDecision(ctx, "timeout", latency);
      return { decision: fallback, action: "fallback_deterministic", fallback_reason: "timeout", latency_ms: latency };
    }

    const latency = decision.latency_ms;

    // Deterministic policy on top of engine evidence (jev-routing section 5-6).
    if (decision.route.confidence !== null && decision.route.confidence < this.minRouteConfidence) {
      return {
        decision: { ...decision, fallback_reason: "low_confidence" },
        action: "fallback_deterministic",
        fallback_reason: "low_confidence",
        latency_ms: latency,
      };
    }

    if (
      decision.fallback_reason !== "none" &&
      decision.fallback_reason !== "policy_override" &&
      decision.fallback_reason !== "operator_override"
    ) {
      return { decision, action: "fallback_deterministic", fallback_reason: decision.fallback_reason, latency_ms: latency };
    }

    // Interrupt policy is code, not model output. An immediate interrupt
    // requires the current performance to be interruptible; a boundary
    // interrupt only applies when the performance can actually be left at
    // a phrase boundary (i.e. it is interruptible at all).
    let interruptAction: ReflexOutcome["action"];
    if (decision.interrupt_probability >= this.interruptImmediate && ctx.performer.current_interruptible) {
      interruptAction = "interrupt_immediate";
    } else if (decision.interrupt_probability >= this.interruptBoundary && ctx.performer.current_interruptible) {
      interruptAction = "interrupt_at_boundary";
    } else {
      interruptAction = "keep_current";
    }

    const routeAction: ReflexOutcome["action"] =
      decision.route.value === "cached" && decision.cache_reuse_probability >= this.reuseThreshold
        ? "route_cached"
        : decision.route.value === "cached"
          ? "fallback_deterministic"
          : decision.route.value === "template"
            ? "route_template"
            : decision.route.value === "llm"
              ? "route_llm"
              : decision.route.value === "reaction"
                ? "route_reaction"
                : "silent";

    const action: ReflexOutcome["action"] =
      routeAction === "silent" || routeAction === "fallback_deterministic"
        ? routeAction
        : interruptAction !== "keep_current" && decision.route.value !== "llm"
          ? interruptAction
          : routeAction;

    return { decision, action, fallback_reason: decision.fallback_reason, latency_ms: latency };
  }

  /** Race the engine against the deadline using a deterministic clock tick helper. */
  private async withTimeout(promise: Promise<ReflexDecision>, nowMs: number): Promise<ReflexDecision> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timeout = new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error("reflex_deadline_exceeded")), this.timeoutMs);
    });
    try {
      return await Promise.race([promise, timeout]);
    } finally {
      if (timer) clearTimeout(timer);
    }
  }
}
