// Jev HTTP adapter: implements the generic DecisionEngine boundary while
// keeping every Jev-specific wire type sealed inside this file
// (jev-routing section 1: "Jev-specific request/response types remain
// inside the adapter").
//
// HTTP failure modes map to typed fallback reasons so replay/scheduler
// logic never depends on provider errors (domain contract section 7).
// A deadline abort is thrown and converted to `timeout` by the director.
// Secrets are referenced by identifier only; they never enter domain
// state or telemetry (threat model section 10).

import type {
  AttentionTarget,
  DecisionEngine,
  FallbackReason,
  ReflexContext,
  ReflexDecision,
  ResponseRoute,
} from "./reflex.js";

// --- Jev wire DTOs (sealed here; do not import outside this adapter) ---

interface JevChoice {
  value: string;
  confidence?: number | null;
}

interface JevWireResponse {
  choices?: {
    response_route?: JevChoice;
    reaction?: JevChoice;
    gesture?: JevChoice;
    attention?: JevChoice;
  };
  scores?: {
    interrupt?: number;
    cache_reuse?: number;
    importance?: number;
    emotion_intensity?: number;
  };
  model?: { alias?: string | null; version?: string | null };
  latency_ms?: number;
}

// --- Wire mapping ---

const ROUTE_VALUES: readonly ResponseRoute[] = ["silent", "reaction", "cached", "template", "llm"];
const ATTENTION_VALUES: readonly AttentionTarget[] = ["camera", "chat", "game", "speaker", "away"];

const clamp01 = (n: unknown): number =>
  typeof n === "number" && Number.isFinite(n) ? Math.min(1, Math.max(0, n)) : 0;

function mapRoute(choice: JevChoice | undefined): {
  value: ResponseRoute;
  confidence: number | null;
  error?: string;
} {
  if (!choice || typeof choice.value !== "string") {
    return { value: "silent", confidence: null, error: "missing_route" };
  }
  if (!ROUTE_VALUES.includes(choice.value as ResponseRoute)) {
    return { value: "silent", confidence: null, error: "unknown_route" };
  }
  const confidence =
    typeof choice.confidence === "number" && Number.isFinite(choice.confidence)
      ? Math.min(1, Math.max(0, choice.confidence))
      : null;
  return { value: choice.value as ResponseRoute, confidence };
}

function mapAttention(choice: JevChoice | undefined): AttentionTarget {
  return choice && ATTENTION_VALUES.includes(choice.value as AttentionTarget)
    ? (choice.value as AttentionTarget)
    : "away";
}

export function wireToDecision(wire: JevWireResponse): { decision: ReflexDecision | null; error?: string } {
  const route = mapRoute(wire.choices?.response_route);
  if (route.error) return { decision: null, error: route.error };

  const decision: ReflexDecision = {
    route: { value: route.value, confidence: route.confidence },
    reaction_family: wire.choices?.reaction?.value ?? null,
    gesture_family: wire.choices?.gesture?.value ?? null,
    attention_target: mapAttention(wire.choices?.attention),
    interrupt_probability: clamp01(wire.scores?.interrupt),
    cache_reuse_probability: clamp01(wire.scores?.cache_reuse),
    importance: clamp01(wire.scores?.importance),
    emotion_intensity: clamp01(wire.scores?.emotion_intensity),
    backend: {
      name: "jev",
      model_alias: wire.model?.alias ?? null,
      model_version: wire.model?.version ?? null,
    },
    latency_ms: clamp01(wire.latency_ms) > 0 ? (wire.latency_ms as number) : 0,
    fallback_reason: "none",
  };
  return { decision };
}

export type JevHttpFailureReason = Extract<
  FallbackReason,
  "rate_limited" | "overloaded" | "authentication" | "invalid_request" | "unavailable"
>;

export function httpStatusToFailure(status: number): JevHttpFailureReason {
  if (status === 429) return "rate_limited";
  if (status === 401 || status === 403) return "authentication";
  if (status === 503 || status === 529) return "overloaded";
  if (status === 400 || status === 422) return "invalid_request";
  return "unavailable";
}

// --- Adapter ---

export interface JevAdapterOptions {
  endpoint: string;
  /** Secret *identifier* resolved by the host process; never the value. */
  secretRef?: string;
  modelAlias?: string;
  /** Injectable for tests; defaults to global fetch. */
  fetchImpl?: typeof fetch;
  /** Adapter-level deadline in ms; the director enforces its own as well. */
  timeoutMs?: number;
}

export class JevAdapter implements DecisionEngine {
  readonly name = "jev";
  private readonly endpoint: string;
  private readonly secretRef?: string;
  private readonly modelAlias?: string;
  private readonly fetchImpl: typeof fetch;
  private readonly timeoutMs: number;

  constructor(options: JevAdapterOptions) {
    this.endpoint = options.endpoint;
    this.secretRef = options.secretRef;
    this.modelAlias = options.modelAlias;
    this.fetchImpl = options.fetchImpl ?? globalThis.fetch;
    this.timeoutMs = options.timeoutMs ?? 200;
  }

  async evaluate(ctx: ReflexContext): Promise<ReflexDecision> {
    const startedAt = Date.now();
    const controller = new AbortController();
    const abortTimer = setTimeout(() => controller.abort(), this.timeoutMs);

    try {
      const response = await this.fetchImpl(this.endpoint, {
        method: "POST",
        headers: this.secretRef ? { "content-type": "application/json", "x-secret-ref": this.secretRef } : { "content-type": "application/json" },
        body: JSON.stringify(this.toWireRequest(ctx)),
        signal: controller.signal,
      });

      if (!response.ok) {
        return this.failure(httpStatusToFailure(response.status), Date.now() - startedAt);
      }

      const wire = (await response.json()) as JevWireResponse;
      const { decision, error } = wireToDecision(wire);
      if (!decision) return this.failure("invalid_request", Date.now() - startedAt, error);
      return { ...decision, latency_ms: Date.now() - startedAt };
    } catch (error) {
      // Deadline aborts and network failures surface as unavailable here;
      // the director's own race converts deadline exceeded into `timeout`.
      if ((error as Error)?.name === "AbortError") throw error;
      return this.failure("unavailable", Date.now() - startedAt);
    } finally {
      clearTimeout(abortTimer);
    }
  }

  private toWireRequest(ctx: ReflexContext): Record<string, unknown> {
    return {
      model: this.modelAlias ? { alias: this.modelAlias } : undefined,
      event: {
        kind: ctx.event.kind,
        trust_level: ctx.event.trust_level,
        payload: ctx.event.payload,
      },
      performer: ctx.performer,
      recent: ctx.recent,
      retrieval: ctx.retrieval,
      now_ms: ctx.nowMs,
    };
  }

  private failure(reason: JevHttpFailureReason, latency: number, detail?: string): ReflexDecision {
    return {
      route: { value: "silent", confidence: null },
      reaction_family: null,
      gesture_family: null,
      attention_target: "away",
      interrupt_probability: 0,
      cache_reuse_probability: 0,
      importance: 0.1,
      emotion_intensity: 0,
      backend: { name: "jev", model_alias: this.modelAlias ?? null, model_version: null },
      latency_ms: latency,
      fallback_reason: reason,
      ...(detail ? {} : {}),
    };
  }
}
