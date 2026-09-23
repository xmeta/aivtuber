// ThinkingEngine contract: provider-neutral LLM boundary (roadmap
// phase 4). Follows the DecisionEngine pattern established in
// src/reflex.ts: the domain depends on generic interfaces; provider wire
// types stay inside adapters.
//
// Key invariants from the docs:
//   - LLM output is DATA, never authorization (threat model section 2):
//     generation results carry no capabilities and cannot mint operator
//     events. Typed structurally here so the compiler helps enforce it.
//   - The ThinkingEngine receives a curated context, not the raw stream
//     history (architecture section 6.3).
//   - Failures normalize to typed reasons; adapters never leak provider
//     errors into domain logic (domain contract section 7).
//   - Generated output must pass the deterministic output gate before it
//     becomes public (threat model section 11).

export type LlmFailureReason =
  | "timeout"
  | "unavailable"
  | "rate_limited"
  | "overloaded"
  | "authentication"
  | "invalid_request"
  | "context_too_long"
  | "content_filtered";

export interface LlmMessage {
  role: "system" | "user" | "assistant";
  /** Marked untrusted by the caller; the adapter must pass trust markers through. */
  content: string;
  trust: "untrusted" | "semi_trusted" | "trusted";
}

/** Curated generation context — never the complete raw history. */
export interface LlmRequest {
  messages: LlmMessage[];
  max_output_tokens?: number;
  temperature?: number;
  /** Structural expectation for the reply; adapters should validate against it. */
  expect?: { kind: "text" } | { kind: "json"; schemaHint?: string };
  /** Recorded in telemetry; never a secret value. */
  modelHint?: string;
}

/** One incremental chunk of a streaming text response. */
export interface LlmStreamChunk {
  /** Incremental text delta. */
  delta: string;
  /** Present on the final chunk. */
  finishReason?: "stop" | "length" | "content_filter" | "error";
}

export interface LlmResult {
  text: string;
  finishReason: "stop" | "length" | "content_filter";
  backend: { name: string; model_alias: string | null };
  latency_ms: number;
}

export type LlmOutcome =
  | { ok: true; result: LlmResult; stream: AsyncIterable<LlmStreamChunk> | null }
  | { ok: false; reason: LlmFailureReason; detail?: string };

/**
 * Provider-neutral generative boundary. Implementations may stream;
 * consumers must treat every produced text as untrusted data and run it
 * through the output gate before publishing.
 */
export interface ThinkingEngine {
  readonly name: string;
  /** Single-shot generation with the complete result. */
  generate(request: LlmRequest): Promise<LlmOutcome>;
  /**
   * Streaming generation. Returns null when the backend cannot stream;
   * callers must fall back to `generate`. Chunks arrive in order and the
   * final chunk carries the finish reason.
   */
  generateStreaming(request: LlmRequest): Promise<LlmOutcome> | null;
}
