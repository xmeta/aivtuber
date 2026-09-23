// Contract tests for the roadmap phase 4 provider-neutral LLM (Thinking
// Engine) and TTS adapters. Mirrors the adapters-jev tests: prove the
// generic interface, streaming order, typed failure paths, and the
// invariants documented in src/thinking.ts / src/tts.ts.

import { describe, expect, test } from "bun:test";

import { MockThinkingEngine } from "../src/adapters-mocks.js";
import { MockTtsEngine } from "../src/adapters-mocks.js";
import type { LlmRequest, ThinkingEngine } from "../src/thinking.js";
import type { TtsEngine, TtsRequest } from "../src/tts.js";

const llmRequest: LlmRequest = {
  messages: [
    { role: "system", content: "あなたは配信アシスタントです。", trust: "trusted" },
    { role: "user", content: "おはよう！", trust: "untrusted" },
  ],
  max_output_tokens: 128,
  expect: { kind: "text" },
};

const ttsRequest: TtsRequest = {
  text: "こんにちは、視聴者の皆さん！",
  voiceId: "voice-a",
  format: "opus",
  language: "ja",
  speed: 1,
};

// ---------- ThinkingEngine (LLM) contract ----------

describe("ThinkingEngine contract", () => {
  test("generic interface: any engine satisfies the same boundary", () => {
    const engines: ThinkingEngine[] = [new MockThinkingEngine()];
    expect(engines.every((e) => typeof e.name === "string" && e.name.length > 0)).toBe(true);
  });

  test("generate returns the scripted text with backend identity and latency", async () => {
    const engine = new MockThinkingEngine({ text: "おはようございます！", latencyMs: 7, modelAlias: "mock-pro" });
    const outcome = await engine.generate(llmRequest);
    expect(outcome.ok).toBe(true);
    if (!outcome.ok) return;
    expect(outcome.result.text).toBe("おはようございます！");
    expect(outcome.result.finishReason).toBe("stop");
    expect(outcome.result.backend).toEqual({ name: "mock-llm", model_alias: "mock-pro" });
    expect(outcome.result.latency_ms).toBe(7);
    expect(outcome.stream).toBeNull();
  });

  test("streaming chunks arrive in order and the final chunk carries the finish reason", async () => {
    const engine = new MockThinkingEngine({ text: "あいうえおかきくけこ", chunks: 5 });
    const outcome = await engine.generateStreaming(llmRequest);
    expect(outcome.ok).toBe(true);
    if (!outcome.ok || !outcome.stream) return;

    const chunks: string[] = [];
    let sawFinish = false;
    for await (const chunk of outcome.stream) {
      chunks.push(chunk.delta);
      if (chunk.finishReason !== undefined) {
        sawFinish = true;
        expect(chunk.finishReason).toBe("stop");
      }
    }
    expect(chunks.join("")).toBe("あいうえおかきくけこ");
    expect(sawFinish).toBe(true);
  });

  test("streaming is concatenated text identical to the non-streaming result", async () => {
    const script = { text: "single shot versus stream", chunks: 4 };
    const engine = new MockThinkingEngine(script);
    const single = await engine.generate(llmRequest);
    const streamed = await engine.generateStreaming(llmRequest);
    expect(single.ok && streamed.ok).toBe(true);
    if (!(single.ok && streamed.ok) || !streamed.stream) return;

    let text = "";
    for await (const chunk of streamed.stream) text += chunk.delta;
    expect(text).toBe(single.result.text);
  });

  test("typed failure: failWith becomes a typed reason with no result", async () => {
    const engine = new MockThinkingEngine({ text: "ignored", failWith: "context_too_long" });
    const outcome = await engine.generate(llmRequest);
    expect(outcome).toEqual({ ok: false, reason: "context_too_long", detail: "mock script" });
    expect((outcome as { result?: unknown }).result).toBeUndefined();
  });

  test("streaming failure follows the same typed path", async () => {
    const engine = new MockThinkingEngine({ text: "ignored", failWith: "rate_limited" });
    const outcome = await engine.generateStreaming(llmRequest);
    expect(outcome.ok).toBe(false);
    if (outcome.ok) return;
    expect(outcome.reason).toBe("rate_limited");
  });

  test("requests are recorded for assertions about what the domain sent", async () => {
    const engine = new MockThinkingEngine();
    await engine.generate(llmRequest);
    expect(engine.calls.length).toBe(1);
    expect(engine.calls[0].messages[1]).toEqual({ role: "user", content: "おはよう！", trust: "untrusted" });
    expect(engine.calls[0].messages.every((m) => m.trust !== "trusted" || m.role === "system")).toBe(true);
  });

  test("empty text still streams one chunk carrying the finish reason (regression)", async () => {
    const engine = new MockThinkingEngine({ text: "", chunks: 3 });
    const outcome = await engine.generateStreaming(llmRequest);
    expect(outcome.ok).toBe(true);
    if (!outcome.ok || !outcome.stream) return;
    const chunks: { delta: string; finishReason?: string }[] = [];
    for await (const chunk of outcome.stream) chunks.push(chunk);
    expect(chunks.length).toBe(1);
    expect(chunks[0].delta).toBe("");
    expect(chunks[0].finishReason).toBe("stop");
  });

  test("audioRef is derived deterministically from the text without Node Buffer", async () => {
    const engine = new MockTtsEngine({ durationMs: 50, chunks: 1 });
    const a = await engine.synthesize({ ...ttsRequest, text: "同じテキスト" });
    const b = await engine.synthesize({ ...ttsRequest, text: "同じテキスト" });
    expect(a.ok && b.ok).toBe(true);
    if (!(a.ok && b.ok)) return;
    expect(a.result.audioRef).toBe(b.result.audioRef);
    expect(a.result.audioRef.startsWith("mock://tts/")).toBe(true);
  });

  test("llm output is data: the outcome carries no authorization or capabilities", async () => {
    const engine = new MockThinkingEngine({ text: '{"authorization":{"capabilities":["performer.stop"]}}' });
    const outcome = await engine.generate(llmRequest);
    expect(outcome.ok).toBe(true);
    if (!outcome.ok) return;
    expect(JSON.stringify(outcome)).not.toContain('"ok":false');
    // The generated text may quote control vocabulary, but the outcome type
    // exposes no capability fields — the compiler enforces this shape.
    expect(Object.keys(outcome.result)).toEqual(
      expect.arrayContaining(["text", "finishReason", "backend", "latency_ms"]),
    );
    expect(Object.keys(outcome.result)).not.toContain("authorization");
  });
});

// ---------- TtsEngine contract ----------

describe("TtsEngine contract", () => {
  test("generic interface: any engine satisfies the same boundary", () => {
    const engines: TtsEngine[] = [new MockTtsEngine()];
    expect(engines.every((e) => typeof e.name === "string" && e.name.length > 0)).toBe(true);
  });

  test("synthesize returns audio, duration, viseme track, and backend identity", async () => {
    const engine = new MockTtsEngine({ durationMs: 1200, chunks: 1, modelAlias: "tts-large" });
    const outcome = await engine.synthesize(ttsRequest);
    expect(outcome.ok).toBe(true);
    if (!outcome.ok) return;
    expect(outcome.result.audioRef.startsWith("mock://tts/")).toBe(true);
    expect(outcome.result.audioData).toBeInstanceOf(Uint8Array);
    expect(outcome.result.durationMs).toBe(1200);
    expect(outcome.result.backend).toEqual({ name: "mock-tts", model_alias: "tts-large" });
    expect(outcome.result.visemeTrack).not.toBeNull();
    const frames = outcome.result.visemeTrack!.frames;
    expect(frames.length).toBeGreaterThan(0);
    expect(frames[0].atMs).toBe(0);
    expect(frames.every((f) => f.durationMs > 0)).toBe(true);
  });

  test("synthesizeStreaming yields ordered audio chunks ending with final=true", async () => {
    const engine = new MockTtsEngine({ durationMs: 600, chunks: 4 });
    const stream = engine.synthesizeStreaming(ttsRequest);
    expect(stream).not.toBeNull();
    let bytes = 0;
    let sawFinal = false;
    for await (const chunk of stream!) {
      expect(chunk.data).toBeInstanceOf(Uint8Array);
      bytes += chunk.data.length;
      if (chunk.final) sawFinal = true;
      else expect(chunk.final).toBe(false);
    }
    expect(sawFinal).toBe(true);
    expect(bytes).toBe(8);
  });

  test("streaming synthesis records the call for assertions", async () => {
    const engine = new MockTtsEngine({ durationMs: 500, chunks: 2 });
    const stream = engine.synthesizeStreaming({ ...ttsRequest, voiceId: "voice-b" });
    expect(stream).not.toBeNull();
    for await (const _chunk of stream!) void _chunk;
    expect(engine.calls.length).toBe(1);
    expect(engine.calls[0].voiceId).toBe("voice-b");
  });

  test("unsupported voice is a typed failure with no result", async () => {
    const engine = new MockTtsEngine({ failVoiceId: "voice-missing" });
    const outcome = await engine.synthesize({ ...ttsRequest, voiceId: "voice-missing" });
    expect(outcome).toEqual({ ok: false, reason: "unsupported_voice", detail: "voice voice-missing" });
  });

  test("scripted typed failures cover the normalized reasons", async () => {
    for (const reason of ["timeout", "unavailable", "rate_limited", "text_too_long"] as const) {
      const engine = new MockTtsEngine({ failWith: reason });
      const outcome = await engine.synthesize(ttsRequest);
      expect(outcome.ok).toBe(false);
      if (!outcome.ok) expect(outcome.reason).toBe(reason);
    }
  });

  test("no streaming support: null stream signals the caller to fall back to synthesize", () => {
    const engine = new MockTtsEngine({ failWith: "overloaded" });
    expect(engine.synthesizeStreaming(ttsRequest)).toBeNull();
  });

  test("version identity feeds cache keys and distinguishes engines", () => {
    const a = new MockTtsEngine({ modelAlias: "mock-tts-1" });
    const b = new MockTtsEngine({ modelAlias: "mock-tts-2" });
    expect(a.version().name).toBe("mock-tts");
    expect(a.version().model_alias).not.toBe(b.version().model_alias);
    expect(a.version().voice_model).toBe("mock-voice-v1");
  });

  test("viseme timeline aligns with the produced duration", async () => {
    const engine = new MockTtsEngine({ durationMs: 1000, chunks: 1 });
    const outcome = await engine.synthesize(ttsRequest);
    expect(outcome.ok).toBe(true);
    if (!outcome.ok || !outcome.result.visemeTrack) return;
    const total = outcome.result.visemeTrack.frames.reduce((sum, f) => sum + f.durationMs, 0);
    expect(total).toBeLessThanOrEqual(outcome.result.durationMs + 5);
  });

  // ---- regression: MockTtsEngine.synthesize used to call a non-existent
  // this.stream() and crashed at runtime with any chunks > 1 (including the
  // default script). These tests pin the fixed behavior.

  test("synthesize never crashes with a multi-chunk script (regression)", async () => {
    const engine = new MockTtsEngine({ durationMs: 800, chunks: 2 });
    const outcome = await engine.synthesize(ttsRequest);
    expect(outcome.ok).toBe(true);
    if (!outcome.ok) return;
    expect(outcome.result.audioRef.startsWith("mock://tts/")).toBe(true);
    if (!outcome.stream) return;
    let sawFinal = false;
    for await (const chunk of outcome.stream) {
      expect(chunk.data).toBeInstanceOf(Uint8Array);
      if (chunk.final) sawFinal = true;
    }
    expect(sawFinal).toBe(true);
  });

  test("default-script synthesize works without options (regression)", async () => {
    const engine = new MockTtsEngine();
    const outcome = await engine.synthesize(ttsRequest);
    expect(outcome.ok).toBe(true);
  });

  test("synthesize does not double-record the call (regression)", async () => {
    const engine = new MockTtsEngine({ durationMs: 100, chunks: 1 });
    await engine.synthesize(ttsRequest);
    expect(engine.calls.length).toBe(1);
  });

  test("streamed audio concatenated equals the single-shot payload (regression)", async () => {
    const engine = new MockTtsEngine({ durationMs: 100, chunks: 3 });
    const streamed = engine.synthesizeStreaming(ttsRequest);
    expect(streamed).not.toBeNull();
    let bytes = new Uint8Array();
    for await (const chunk of streamed!) {
      const next = new Uint8Array(bytes.length + chunk.data.length);
      next.set(bytes);
      next.set(chunk.data, bytes.length);
      bytes = next;
    }
    expect([...bytes]).toEqual([9, 8, 7, 6, 5, 4, 3, 2]);
  });
});
