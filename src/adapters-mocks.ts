// Mock adapters proving the ThinkingEngine and TtsEngine contracts.
// Deterministic and scriptable so tests can exercise streaming order,
// finish reasons, and every typed failure path without external services.

import type {
  LlmFailureReason,
  LlmMessage,
  LlmOutcome,
  LlmRequest,
  LlmResult,
  LlmStreamChunk,
  ThinkingEngine,
} from "./thinking.js";
import type {
  TtsEngine,
  TtsFailureReason,
  TtsOutcome,
  TtsRequest,
  TtsResult,
  TtsStreamChunk,
  VisemeFrame,
} from "./tts.js";

// ---------------- ThinkingEngine mock ----------------

export interface MockLlmScript {
  /** Fixed text produced by the mock. */
  text: string;
  finishReason?: "stop" | "length" | "content_filter";
  /** Chunk count for streaming; text is split evenly, in order. */
  chunks?: number;
  /** When set, generation fails with this typed reason instead. */
  failWith?: LlmFailureReason;
  /** Simulated latency feeding result.latency_ms. */
  latencyMs?: number;
  modelAlias?: string;
}

export class MockThinkingEngine implements ThinkingEngine {
  readonly name = "mock-llm";
  /** Calls are recorded so tests can assert what the domain sent. */
  readonly calls: LlmRequest[] = [];

  constructor(private readonly script: MockLlmScript = { text: "こんにちは！", chunks: 3 }) {}

  private outcome(request: LlmRequest): LlmOutcome {
    this.calls.push(request);
    if (this.script.failWith) {
      return { ok: false, reason: this.script.failWith, detail: "mock script" };
    }

    const stream = this.script.chunks && this.script.chunks > 1 ? this.stream(request) : null;
    const result: LlmResult = {
      text: this.script.text,
      finishReason: this.script.finishReason ?? "stop",
      backend: { name: this.name, model_alias: this.script.modelAlias ?? "mock-1" },
      latency_ms: this.script.latencyMs ?? 5,
    };
    return { ok: true, result, stream };
  }

  async generate(request: LlmRequest): Promise<LlmOutcome> {
    return this.outcome(request);
  }

  async generateStreaming(request: LlmRequest): Promise<LlmOutcome> {
    return this.outcome(request);
  }

  private stream(request: LlmRequest): AsyncIterable<LlmStreamChunk> {
    const text = this.script.text;
    const parts = this.script.chunks ?? 1;
    const finish = this.script.finishReason ?? "stop";
    const size = Math.ceil(text.length / parts);
    const self = this;
    return {
      async *[Symbol.asyncIterator]() {
        for (let i = 0; i < text.length; i += size) {
          const isLast = i + size >= text.length;
          yield { delta: text.slice(i, i + size), ...(isLast ? { finishReason: finish } : {}) };
        }
        void self;
        void request;
      },
    };
  }
}

// ---------------- TtsEngine mock ----------------

export interface MockTtsScript {
  /** Deterministic synthesized duration. */
  durationMs?: number;
  /** Simple gate: fail when voiceId matches. */
  failVoiceId?: string;
  /** When set, synthesis fails with this typed reason instead. */
  failWith?: TtsFailureReason;
  /** Chunk count for streaming audio. */
  chunks?: number;
  modelAlias?: string;
}

function framesFor(durationMs: number, voice: string): VisemeFrame[] {
  // Simple deterministic viseme pattern aligned to the duration.
  const pattern = ["sil", "aa", "ih", "mm", "sil"];
  const frameMs = Math.max(1, Math.floor(durationMs / pattern.length));
  return pattern.map((viseme, i) => ({
    atMs: i * frameMs,
    viseme: `${viseme}:${voice}`,
    durationMs: frameMs,
  }));
}

export class MockTtsEngine implements TtsEngine {
  readonly name = "mock-tts";
  readonly calls: TtsRequest[] = [];

  constructor(private readonly script: MockTtsScript = { durationMs: 800, chunks: 2 }) {}

  version() {
    return { name: this.name, model_alias: this.script.modelAlias ?? "mock-tts-1", voice_model: "mock-voice-v1" };
  }

  async synthesize(request: TtsRequest): Promise<TtsOutcome> {
    this.calls.push(request);

    if (this.script.failWith) {
      return { ok: false, reason: this.script.failWith, detail: "mock script" };
    }
    if (this.script.failVoiceId && request.voiceId === this.script.failVoiceId) {
      return { ok: false, reason: "unsupported_voice", detail: `voice ${request.voiceId}` };
    }

    const durationMs = this.script.durationMs ?? 800;
    const result: TtsResult = {
      audioRef: `mock://tts/${Buffer.from(request.text).toString("base64url").slice(0, 24)}.wav`,
      audioData: new Uint8Array([1, 2, 3, 4]),
      durationMs,
      visemeTrack: {
        voice_model: "mock-voice-v1",
        viseme_mapping: "mock-map-1",
        frames: framesFor(durationMs, request.voiceId),
      },
      backend: { name: this.name, model_alias: this.version().model_alias },
      latency_ms: 3,
    };
    return { ok: true, result, stream: this.script.chunks && this.script.chunks > 1 ? this.stream() : null };
  }

  synthesizeStreaming(request: TtsRequest): AsyncIterable<TtsStreamChunk> | null {
    this.calls.push(request);
    if (this.script.failWith || (this.script.failVoiceId && request.voiceId === this.script.failVoiceId)) {
      return null; // streaming contract: null -> caller falls back to synthesize()
    }
    const chunks = this.script.chunks ?? 1;
    const payload = new Uint8Array([9, 8, 7, 6, 5, 4, 3, 2]);
    const size = Math.ceil(payload.length / chunks);
    const self = this;
    return {
      async *[Symbol.asyncIterator]() {
        for (let i = 0; i < payload.length; i += size) {
          const isLast = i + size >= payload.length;
          yield { data: payload.slice(i, i + size), final: isLast };
        }
        void self;
      },
    };
  }
}

// Re-export message type for convenience in tests.
export type { LlmMessage };
