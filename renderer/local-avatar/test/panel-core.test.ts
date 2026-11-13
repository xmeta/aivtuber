// Unit tests for the operation panel's pure logic (issue #335).
import { describe, expect, test } from "bun:test";
import {
  CHAT_PLATFORMS,
  MAX_CHAT_CHARS,
  MAX_CHAT_LOG,
  MAX_CHAT_TARGET_CHARS,
  MAX_CREDIT_CHARS,
  MAX_KEY_CHARS,
  MAX_MODEL_CHOICES,
  SEARCH_PAGE_SIZE,
  type VoiceModel,
  checkChat,
  checkTwitchChannel,
  checkYoutubeVideo,
  chatIngressView,
  connectBody,
  checkCredit,
  checkKey,
  checkKeyword,
  clampTuning,
  creditFor,
  defaultSelection,
  enablePatch,
  keyCheckLabel,
  modelView,
  otherReplies,
  parseChatIngressState,
  parseModelState,
  parseSearchPage,
  parseState,
  parseVoiceState,
  pushChat,
  repliesFor,
  safeAvatarUrl,
  searchPageCount,
  statusRows,
  tokenFromHash,
  tuningPatch,
  voiceView,
} from "../src/panel-core";

const TOKEN = "ab".repeat(32);

const state = (over: Record<string, unknown> = {}) =>
  parseState({
    schema: "local-panel/1",
    uptime_ms: 5,
    status: {
      muted: false,
      audio_error: null,
      avatar_error: null,
      stream_error: null,
      content_queue: 0,
      scheduler_items: 0,
      generation_active: false,
    },
    replies: [
      { seq: 1, event_id: "panel-chat-1", text: "こんにちは" },
      { seq: 2, event_id: "evt-9", text: "stdin reply" },
    ],
    avatar_url: "http://127.0.0.1:8765/?token=x",
    ...over,
  });

describe("tokenFromHash", () => {
  test("accepts only the exact fragment form the server mints", () => {
    expect(tokenFromHash(`#token=${TOKEN}`)).toBe(TOKEN);
    expect(tokenFromHash("")).toBeNull();
    expect(tokenFromHash("#token=short")).toBeNull();
    expect(tokenFromHash(`#token=${TOKEN}&x=1`)).toBeNull();
    expect(tokenFromHash(`?token=${TOKEN}`)).toBeNull();
    expect(tokenFromHash(`#token=${TOKEN.toUpperCase()}`)).toBeNull();
  });
});

describe("parseState", () => {
  test("rejects another schema or a malformed document", () => {
    expect(state()).not.toBeNull();
    expect(state({ schema: "local-panel/2" })).toBeNull();
    expect(state({ replies: "x" })).toBeNull();
    expect(parseState(null)).toBeNull();
    expect(parseState("{}")).toBeNull();
    expect(state({ status: { muted: "yes" } })).toBeNull();
  });
});

describe("safeAvatarUrl", () => {
  test("offers only an http loopback link", () => {
    expect(safeAvatarUrl("http://127.0.0.1:8765/?token=x")).toBe("http://127.0.0.1:8765/?token=x");
    expect(safeAvatarUrl("https://evil.example/")).toBeNull();
    expect(safeAvatarUrl("javascript:alert(1)")).toBeNull();
    expect(safeAvatarUrl("http://evil.example:8765/")).toBeNull();
    expect(safeAvatarUrl(null)).toBeNull();
  });
});

describe("statusRows", () => {
  test("flags mute and adapter errors", () => {
    const rows = statusRows(state({ status: { ...state()!.status, muted: true, audio_error: "ffplay missing" } })!);
    expect(rows.find((r) => r.label === "ミュート")?.health).toBe("warn");
    const audio = rows.find((r) => r.label === "音声出力");
    expect(audio?.health).toBe("warn");
    expect(audio?.value).toBe("ffplay missing");
    expect(statusRows(state()!).every((r) => r.health === "ok")).toBe(true);
  });
});

describe("chat", () => {
  test("pairs a reply with its line by event id and keeps the rest apart", () => {
    const s = state();
    expect(repliesFor(s, "panel-chat-1")).toEqual(["こんにちは"]);
    expect(repliesFor(s, "panel-chat-7")).toEqual([]);
    expect(repliesFor(s, null)).toEqual([]);
    expect(otherReplies(s)).toEqual([{ seq: 2, text: "stdin reply" }]);
  });

  test("the log is bounded", () => {
    let log: { id: string | null; text: string; note: string }[] = [];
    for (let i = 0; i < MAX_CHAT_LOG + 10; i++) log = pushChat(log, { id: null, text: String(i), note: "" });
    expect(log).toHaveLength(MAX_CHAT_LOG);
    expect(log.at(-1)?.text).toBe(String(MAX_CHAT_LOG + 9));
  });

  test("checkChat trims, refuses empty and over-long text", () => {
    expect(checkChat("  hi ")).toEqual({ ok: true, text: "hi" });
    expect(checkChat("   ").ok).toBe(false);
    expect(checkChat("あ".repeat(MAX_CHAT_CHARS)).ok).toBe(true);
    expect(checkChat("あ".repeat(MAX_CHAT_CHARS + 1)).ok).toBe(false);
  });
});

const model = (over: Record<string, unknown> = {}) => ({
  active: "gemma-4-e4b-it-qat",
  active_version: "UD-Q4_K_XL",
  state: "ready",
  target: null,
  available: [
    { alias: "gemma-4-e4b-it-qat", version: "UD-Q4_K_XL" },
    { alias: "qwen3-4b-instruct-2507", version: "Q4_K_M" },
  ],
  ...over,
});

const withModel = (m: unknown) =>
  state({
    status: {
      muted: false,
      audio_error: null,
      avatar_error: null,
      stream_error: null,
      content_queue: 0,
      scheduler_items: 0,
      generation_active: false,
      thinking_model: m,
    },
  });

describe("model selector state", () => {
  test("a state without the selector has no model view", () => {
    expect(state()!.status.thinking_model).toBeNull();
    expect(modelView(state())).toBeNull();
    expect(modelView(null)).toBeNull();
  });

  test("parseModelState accepts exactly the documented shape", () => {
    expect(parseModelState(model())?.available).toHaveLength(2);
    expect(parseModelState(model({ state: "switching", target: "qwen3-4b-instruct-2507" }))?.state).toBe("switching");
    for (const bad of [
      null,
      "x",
      model({ active: "C:\\evil.gguf" }),
      model({ active: "Gemma" }),
      model({ active_version: "a b" }),
      model({ state: "loading" }),
      model({ target: "../x" }),
      model({ available: [] }),
      model({ available: [{ alias: "a", version: "v" }, { alias: "a", version: "v" }] }),
      model({ available: [{ alias: "a", version: "v", path: "/x" }, { alias: "<b>", version: "v" }] }),
      model({ available: Array.from({ length: MAX_MODEL_CHOICES + 1 }, (_, i) => ({ alias: `m${i}`, version: "v" })) }),
    ]) {
      expect(parseModelState(bad)).toBeNull();
    }
  });

  test("a malformed selector is dropped without rejecting the whole state", () => {
    const s = withModel({ active: "<script>", available: [] });
    expect(s).not.toBeNull();
    expect(s!.status.thinking_model).toBeNull();
    expect(modelView(s)).toBeNull();
  });

  test("ready: two models can be switched between, one cannot", () => {
    const two = modelView(withModel(model()))!;
    expect(two.disabled).toBe(false);
    expect(two.selected).toBe("gemma-4-e4b-it-qat");
    expect(two.options.map((o) => o.label)).toEqual([
      "gemma-4-e4b-it-qat (UD-Q4_K_XL)",
      "qwen3-4b-instruct-2507 (Q4_K_M)",
    ]);
    expect(two.health).toBe("ok");
    const one = modelView(withModel(model({ available: [{ alias: "gemma-4-e4b-it-qat", version: "UD-Q4_K_XL" }] })))!;
    expect(one.options).toHaveLength(1);
    expect(one.disabled).toBe(true);
    expect(one.message).toContain("--with-model");
  });

  test("switching shows the target and locks the selector; failed says what stayed", () => {
    const switching = modelView(
      withModel(model({ active: "qwen3-4b-instruct-2507", active_version: "Q4_K_M", state: "switching", target: "qwen3-4b-instruct-2507" })),
    )!;
    expect(switching.disabled).toBe(true);
    expect(switching.selected).toBe("qwen3-4b-instruct-2507");
    expect(switching.health).toBe("warn");
    const failed = modelView(withModel(model({ state: "failed", target: "qwen3-4b-instruct-2507" })))!;
    expect(failed.selected).toBe("gemma-4-e4b-it-qat");
    expect(failed.disabled).toBe(false);
    expect(failed.message).toContain("失敗");
  });
});

const MODEL = "a59cb814-0083-4369-8542-f51a29e72af7";
const SPEAKER = "e756b8e4-b606-4e15-99b1-3f9c6a1b2317";

const voice = (over: Record<string, unknown> = {}) => ({
  backend: "local",
  model_uuid: null,
  model_name: null,
  speaker_uuid: null,
  style_id: null,
  tuning: { speaking_rate: 1, pitch: 0, volume: 1, emotional_intensity: 1 },
  credit: null,
  key: { set: false, source: null, check: "unchecked" },
  last_error: null,
  save_error: null,
  preview_text: "こんにちは",
  ...over,
});

const withVoice = (v: unknown) =>
  state({
    status: {
      muted: false,
      audio_error: null,
      avatar_error: null,
      stream_error: null,
      content_queue: 0,
      scheduler_items: 0,
      generation_active: false,
      voice: v,
    },
  });

describe("voice state", () => {
  test("a state without voice settings has no voice view", () => {
    expect(state()!.status.voice).toBeNull();
    expect(voiceView(state())).toBeNull();
    expect(voiceView(null)).toBeNull();
  });

  test("parseVoiceState accepts exactly the documented shape", () => {
    expect(parseVoiceState(voice())?.backend).toBe("local");
    expect(
      parseVoiceState(
        voice({ backend: "aivis", model_uuid: MODEL, model_name: "まお", speaker_uuid: SPEAKER, style_id: 2, key: { set: true, source: "file", check: "valid" } }),
      )?.style_id,
    ).toBe(2);
    for (const bad of [
      null,
      "x",
      voice({ backend: "cloud" }),
      voice({ model_uuid: "../x" }),
      voice({ speaker_uuid: "nope" }),
      voice({ style_id: 32 }),
      voice({ style_id: 1.5 }),
      voice({ credit: "x".repeat(MAX_CREDIT_CHARS + 1) }),
      voice({ tuning: { speaking_rate: 2.5, pitch: 0, volume: 1, emotional_intensity: 1 } }),
      voice({ tuning: { speaking_rate: 1, pitch: -1.5, volume: 1, emotional_intensity: 1 } }),
      voice({ tuning: { speaking_rate: 1, pitch: 0, volume: 1, emotional_intensity: Number.NaN } }),
      voice({ key: { set: true, source: "somewhere", check: "valid" } }),
      voice({ key: { set: true, source: "file", check: "great" } }),
      voice({ key: { set: "yes", source: "file", check: "valid" } }),
      voice({ preview_text: "x".repeat(201) }),
    ]) {
      expect(parseVoiceState(bad)).toBeNull();
    }
  });

  test("a malformed voice block is dropped without rejecting the whole state", () => {
    const s = withVoice({ backend: "<script>" });
    expect(s).not.toBeNull();
    expect(s!.status.voice).toBeNull();
    expect(voiceView(s)).toBeNull();
  });

  test("the view says which voice speaks, where the key comes from and what failed", () => {
    const local = voiceView(withVoice(voice()))!;
    expect(local.summary).toContain("ローカル");
    expect(local.keyText).toBe("未設定");
    expect(local.canEnable).toBe(false);
    expect(local.live).toBe(false);
    expect(local.error).toBeNull();
    const aivis = voiceView(
      withVoice(
        voice({ backend: "aivis", model_uuid: MODEL, model_name: "まお", style_id: 2, key: { set: true, source: "environment", check: "rejected" }, last_error: "aivis-cloud: HTTP 401" }),
      ),
    )!;
    expect(aivis.summary).toContain("Aivis Cloud");
    expect(aivis.summary).toContain("まお");
    expect(aivis.summary).toContain("送信");
    expect(aivis.keyText).toContain("AIVIS_API");
    expect(aivis.keyText).toContain("拒否");
    expect(aivis.keyHealth).toBe("warn");
    expect(aivis.health).toBe("warn");
    expect(aivis.canEnable && aivis.live).toBe(true);
    expect(aivis.error).toContain("HTTP 401");
    expect(keyCheckLabel("valid")).toBe("有効");
    expect(keyCheckLabel("network_error")).toContain("ネットワーク");
    expect(keyCheckLabel("unchecked")).toBe("未確認");
  });
});

const wireModel = (over: Record<string, unknown> = {}) => ({
  uuid: MODEL,
  name: "まお",
  description: "いたずらっ子な小悪魔ボイス。\nクレジットは「AivisSpeech: まお」",
  category: "OriginalCharacter",
  voice_timbre: "YouthfulFemale",
  licenses: ["ACML 1.0"],
  speakers: [{ uuid: SPEAKER, name: "まお", styles: [{ name: "ノーマル", style_id: 0 }, { name: "あまあま", style_id: 3 }] }],
  ...over,
});

const searchBody = (models: unknown[], over: Record<string, unknown> = {}) => ({
  ok: true,
  detail: "",
  id: null,
  page: { total: models.length, page: 1, models },
  ...over,
});

describe("model search", () => {
  test("parses the typed page and keeps markup as plain strings", () => {
    const page = parseSearchPage(searchBody([wireModel({ name: "<img src=x onerror=alert(1)>" })]))!;
    expect(page.models).toHaveLength(1);
    expect(page.models[0].name).toBe("<img src=x onerror=alert(1)>");
    expect(page.models[0].speakers[0].styles.map((s) => s.style_id)).toEqual([0, 3]);
    expect(page.models[0].licenses).toEqual(["ACML 1.0"]);
  });

  test("a model that is not the documented shape is dropped, the page is not", () => {
    const page = parseSearchPage(
      searchBody([
        wireModel(),
        wireModel({ uuid: "not-a-uuid" }),
        wireModel({ name: "  " }),
        wireModel({ description: "x".repeat(401) }),
        wireModel({ licenses: "ACML" }),
        wireModel({ speakers: [{ uuid: "bad", name: "x", styles: [] }, { uuid: SPEAKER, name: "ok", styles: [{ name: "s", style_id: 32 }, { name: "t", style_id: 31 }] }] }),
        "x",
        null,
      ]),
    )!;
    expect(page.models).toHaveLength(2);
    expect(page.models[1].speakers.map((s) => s.name)).toEqual(["ok"]);
    expect(page.models[1].speakers[0].styles).toEqual([{ name: "t", style_id: 31 }]);
  });

  test("the page is bounded and a malformed answer is null", () => {
    const many = Array.from({ length: SEARCH_PAGE_SIZE + 10 }, (_, i) => wireModel({ uuid: `00000000-0000-0000-0000-${String(i).padStart(12, "0")}` }));
    expect(parseSearchPage(searchBody(many))!.models).toHaveLength(SEARCH_PAGE_SIZE);
    for (const bad of [
      null,
      "x",
      {},
      { page: null },
      searchBody([], { page: { total: -1, page: 1, models: [] } }),
      searchBody([], { page: { total: 1, page: 0, models: [] } }),
      searchBody([], { page: { total: 1, page: 1, models: "x" } }),
    ]) {
      expect(parseSearchPage(bad)).toBeNull();
    }
    expect(searchPageCount(0)).toBe(1);
    expect(searchPageCount(24)).toBe(1);
    expect(searchPageCount(25)).toBe(2);
    expect(searchPageCount(77)).toBe(4);
  });
});

describe("voice inputs", () => {
  test("the credit line is prefilled from the model name, bounded, plain text", () => {
    expect(creditFor("まお")).toBe("AivisSpeech: まお");
    expect(creditFor("  まお ")).toBe("AivisSpeech: まお");
    expect([...creditFor("あ".repeat(500))]).toHaveLength(MAX_CREDIT_CHARS);
    expect(checkCredit(" AivisSpeech: まお ")).toEqual({ ok: true, text: "AivisSpeech: まお" });
    expect(checkCredit("   ").ok).toBe(false);
    expect(checkCredit("a\nb").ok).toBe(false);
    expect(checkCredit("あ".repeat(MAX_CREDIT_CHARS)).ok).toBe(true);
    expect(checkCredit("あ".repeat(MAX_CREDIT_CHARS + 1)).ok).toBe(false);
  });

  test("the key is trimmed and refuses whitespace, control characters and length", () => {
    expect(checkKey("  sk-abc  ")).toEqual({ ok: true, key: "sk-abc" });
    expect(checkKey("").ok).toBe(false);
    expect(checkKey("two words").ok).toBe(false);
    expect(checkKey("a\u0007b").ok).toBe(false);
    expect(checkKey("k".repeat(MAX_KEY_CHARS)).ok).toBe(true);
    expect(checkKey("k".repeat(MAX_KEY_CHARS + 1)).ok).toBe(false);
  });

  test("the keyword may be empty, not long, not controlled", () => {
    expect(checkKeyword("  ")).toEqual({ ok: true, keyword: "" });
    expect(checkKeyword("ツンデレ")).toEqual({ ok: true, keyword: "ツンデレ" });
    expect(checkKeyword("a".repeat(65)).ok).toBe(false);
    expect(checkKeyword("a\nb").ok).toBe(false);
  });

  test("tuning values are clamped to the API's bounds and rounded", () => {
    expect(clampTuning("speaking_rate", 0.1)).toBe(0.5);
    expect(clampTuning("speaking_rate", 3)).toBe(2);
    expect(clampTuning("pitch", -2)).toBe(-1);
    expect(clampTuning("pitch", 0.30000000000000004)).toBe(0.3);
    expect(clampTuning("volume", -1)).toBe(0);
    expect(clampTuning("emotional_intensity", 9)).toBe(2);
    expect(clampTuning("pitch", Number.NaN)).toBe(0);
    expect(clampTuning("volume", Number.NaN)).toBe(1);
    expect(tuningPatch("pitch", 5)).toEqual({ pitch: 1 });
  });

  test("the enable patch carries the whole choice and the egress confirmation", () => {
    const model: VoiceModel = parseSearchPage(searchBody([wireModel()]))!.models[0];
    expect(defaultSelection(model)).toEqual({ speakerUuid: SPEAKER, styleId: 0 });
    expect(defaultSelection({ ...model, speakers: [] })).toEqual({ speakerUuid: null, styleId: null });
    const patch = enablePatch({ model, speakerUuid: SPEAKER, styleId: 3, credit: "AivisSpeech: まお", confirmEgress: true });
    expect(patch).toEqual({
      backend: "aivis",
      model_uuid: MODEL,
      model_name: "まお",
      speaker_uuid: SPEAKER,
      style_id: 3,
      credit: "AivisSpeech: まお",
      confirm_egress: true,
    });
    // There is no key field in a patch.
    expect(Object.keys(patch).some((k) => /key/i.test(k))).toBe(false);
  });
});

// ---- Stream chat connections (#361) ----

const platformState = (over: Record<string, unknown> = {}) => ({
  state: "disconnected",
  target: null,
  saved_target: null,
  sidecar_found: true,
  messages_accepted: 0,
  messages_dropped: 0,
  dropped_queue_full: 0,
  dropped_refused: 0,
  messages_per_minute: 0,
  restarts: 0,
  last_error: null,
  ...over,
});

const ingress = (over: Record<string, unknown> = {}) => ({
  twitch: platformState(),
  youtube: platformState(),
  youtube_key: { set: false, source: null },
  save_error: null,
  ...over,
});

const withIngress = (value: unknown) =>
  parseState({
    schema: "local-panel/1",
    uptime_ms: 5,
    status: {
      muted: false,
      audio_error: null,
      avatar_error: null,
      stream_error: null,
      content_queue: 0,
      scheduler_items: 0,
      generation_active: false,
      chat_ingress: value,
    },
    replies: [],
    avatar_url: null,
  });

describe("chat connection state", () => {
  test("absent or malformed state hides the card instead of failing the page", () => {
    expect(chatIngressView(state())).toBeNull();
    expect(chatIngressView(null)).toBeNull();
    for (const bad of [
      null,
      "x",
      {},
      ingress({ twitch: platformState({ state: "exploded" }) }),
      ingress({ youtube: platformState({ messages_accepted: -1 }) }),
      ingress({ youtube: platformState({ messages_dropped: 1.5 }) }),
      ingress({ twitch: platformState({ target: 5 }) }),
      ingress({ youtube_key: { set: "yes", source: null } }),
      ingress({ youtube_key: { set: true, source: "registry" } }),
      ingress({ save_error: 7 }),
    ]) {
      expect(chatIngressView(withIngress(bad))).toBeNull();
    }
    expect(parseChatIngressState(ingress())).not.toBeNull();
  });

  test("a connected channel shows its counters and the last choice is only offered", () => {
    const s = withIngress(
      ingress({
        twitch: platformState({ state: "connected", target: "somechannel", saved_target: "somechannel", messages_accepted: 12, messages_dropped: 3, messages_per_minute: 40 }),
        youtube: platformState({ saved_target: "dQw4w9WgXcQ" }),
      }),
    );
    const view = chatIngressView(s)!;
    expect(view.twitch.stateText).toBe("接続済み");
    expect(view.twitch.detail).toBe("#somechannel — 受理 12 件 / 破棄 3 件 / 直近1分 40 件");
    expect(view.twitch.canDisconnect).toBe(true);
    expect(view.twitch.health).toBe("ok");
    // Not connected, but the saved video is offered to prefill (never auto-connected).
    expect(view.youtube.stateText).toBe("未接続");
    expect(view.youtube.prefill).toBe("dQw4w9WgXcQ");
    expect(view.youtube.canDisconnect).toBe(false);
  });

  test("YouTube needs a key and a missing sidecar blocks connecting", () => {
    const noKey = chatIngressView(withIngress(ingress()))!;
    expect(noKey.twitch.canConnect).toBe(true);
    expect(noKey.youtube.canConnect).toBe(false);
    expect(noKey.youtube.blocked).toContain("APIキー");
    expect(noKey.keyText).toBe("未設定");
    const keyed = chatIngressView(withIngress(ingress({ youtube_key: { set: true, source: "file" } })))!;
    expect(keyed.youtube.canConnect).toBe(true);
    expect(keyed.keyText).toContain("ファイル");
    const env = chatIngressView(withIngress(ingress({ youtube_key: { set: true, source: "environment" } })))!;
    expect(env.keyText).toContain("YOUTUBE_API_KEY");
    const missing = chatIngressView(withIngress(ingress({ twitch: platformState({ sidecar_found: false }) })))!;
    expect(missing.twitch.canConnect).toBe(false);
    expect(missing.twitch.blocked).toContain("twitch-chat-ingress");
  });

  test("backoff and failure are warnings that can be disconnected", () => {
    const view = chatIngressView(
      withIngress(
        ingress({
          twitch: platformState({ state: "backoff", target: "abc", restarts: 2, last_error: "the sidecar stopped (exit code 1)" }),
          youtube: platformState({ state: "failed", target: "dQw4w9WgXcQ", last_error: "the key was rejected" }),
        }),
      ),
    )!;
    expect(view.twitch.health).toBe("warn");
    expect(view.twitch.stateText).toContain("再起動 2 回");
    expect(view.twitch.canDisconnect).toBe(true);
    expect(view.youtube.health).toBe("warn");
    expect(view.youtube.error).toBe("the key was rejected");
    expect(view.youtube.canDisconnect).toBe(true);
  });
});

describe("chat connection inputs", () => {
  test("a Twitch channel is lowercased and strict, like the app", () => {
    expect(checkTwitchChannel("  Some_Channel1 ")).toEqual({ ok: true, target: "some_channel1" });
    for (const bad of ["", "ab", "#abc", "a b", "ab-c", "https://twitch.tv/abc", "a".repeat(26), "日本語です"]) {
      expect(checkTwitchChannel(bad).ok).toBe(false);
    }
  });

  test("a YouTube video id or one of the three link shapes is accepted", () => {
    for (const ok of [
      "dQw4w9WgXcQ",
      " dQw4w9WgXcQ ",
      "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
      "https://youtube.com/watch?v=dQw4w9WgXcQ&t=10s",
      "http://m.youtube.com/watch?feature=share&v=dQw4w9WgXcQ",
      "https://www.youtube.com/live/dQw4w9WgXcQ",
      "https://www.youtube.com/live/dQw4w9WgXcQ?feature=share",
      "https://youtu.be/dQw4w9WgXcQ",
    ]) {
      expect(checkYoutubeVideo(ok)).toEqual({ ok: true, target: "dQw4w9WgXcQ" });
    }
    for (const bad of [
      "",
      "short",
      "dQw4w9WgXcQ1",
      "https://example.com/watch?v=dQw4w9WgXcQ",
      "https://youtube.com.evil.example/watch?v=dQw4w9WgXcQ",
      "https://user:pw@www.youtube.com/watch?v=dQw4w9WgXcQ",
      "https://www.youtube.com:8443/watch?v=dQw4w9WgXcQ",
      "javascript:alert(1)",
      "https://www.youtube.com/channel/UC1234567890123456789012",
      "https://www.youtube.com/watch",
      "https://www.youtube.com/watch/extra?v=dQw4w9WgXcQ",
      "https://youtu.be/",
      "x".repeat(MAX_CHAT_TARGET_CHARS + 1),
    ]) {
      expect(checkYoutubeVideo(bad).ok).toBe(false);
    }
  });

  test("the connect body carries a platform and the normalized target only", () => {
    expect(connectBody("twitch", " Abc ")).toEqual({ ok: true, body: { platform: "twitch", target: "abc" } });
    expect(connectBody("youtube", "https://youtu.be/dQw4w9WgXcQ")).toEqual({
      ok: true,
      body: { platform: "youtube", target: "dQw4w9WgXcQ" },
    });
    expect(connectBody("youtube", "nope").ok).toBe(false);
    expect(CHAT_PLATFORMS).toEqual(["twitch", "youtube"]);
  });
});
