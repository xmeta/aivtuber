// TtsEngine contract: provider-neutral speech synthesis boundary
// (roadmap phase 4). Same pattern as DecisionEngine/ThinkingEngine:
// generic domain interface, provider wire details inside adapters.
//
// Docs invariants:
//   - TTS failures degrade to cached audio or non-verbal reaction, never
//     freeze the performer (architecture section 9).
//   - Produced audio/visemes are versioned asset components; cache keys
//     include model/version information (architecture section 7).
//   - Public output text has already passed the output gate before
//     synthesis (threat model section 11).

export type TtsFailureReason =
  | "timeout"
  | "unavailable"
  | "rate_limited"
  | "overloaded"
  | "authentication"
  | "invalid_request"
  | "unsupported_voice"
  | "text_too_long";

export interface TtsRequest {
  /** Gate-approved text for synthesis. */
  text: string;
  voiceId: string;
  /** Audio format the audio adapter consumes. */
  format: "opus" | "wav" | "mp3";
  /** Language hint for phonemization. */
  language?: string;
  /** Speaking-rate multiplier in [0.5, 2]; 1 is normal. */
  speed?: number;
}

/** Viseme timeline aligned with the produced audio. */
export interface VisemeFrame {
  /** Offset from audio start in ms. */
  atMs: number;
  /** Canonical viseme id (e.g. "aa", "ih", "sil"). */
  viseme: string;
  durationMs: number;
}

export interface TtsResult {
  /** Reference handle for the encoded audio (opaque to the scheduler). */
  audioRef: string;
  /** Exact encoded byte payload for mock/test use; adapters may omit. */
  audioData?: Uint8Array;
  durationMs: number;
  visemeTrack: VisemeTrack | null;
  backend: { name: string; model_alias: string | null };
  latency_ms: number;
}

export interface VisemeTrack {
  voice_model: string;
  viseme_mapping: string;
  frames: VisemeFrame[];
}

export type TtsOutcome =
  | { ok: true; result: TtsResult; stream: AsyncIterable<TtsStreamChunk> | null }
  | { ok: false; reason: TtsFailureReason; detail?: string };

/** Incremental audio chunks in play order; final chunk is final=true. */
export interface TtsStreamChunk {
  data: Uint8Array;
  final: boolean;
}

export interface TtsEngine {
  readonly name: string;
  /** Single-shot synthesis with the complete audio payload. */
  synthesize(request: TtsRequest): Promise<TtsOutcome>;
  /**
   * Streaming synthesis. Returns null when the backend cannot stream;
   * callers fall back to `synthesize`. Chunks arrive in play order.
   */
  synthesizeStreaming(request: TtsRequest): AsyncIterable<TtsStreamChunk> | null;
  /** Version identity for cache keys; a change invalidates reuse (section 7). */
  version(): { name: string; model_alias: string | null; voice_model: string | null };
}
