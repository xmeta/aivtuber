// Unit tests for the operation panel's pure logic (issue #335).
import { describe, expect, test } from "bun:test";
import {
  AVATAR_WARNING_TEXT,
  CHAT_PLATFORMS,
  MAX_VRM_BYTES,
  MAX_CHAT_CHARS,
  MAX_CHAT_LOG,
  MAX_CHAT_TARGET_CHARS,
  MAX_CREDIT_CHARS,
  MAX_KEY_CHARS,
  MAX_MODEL_CHOICES,
  SEARCH_PAGE_SIZE,
  type VoiceModel,
  avatarView,
  parseUsageState,
  usageView,
  checkChat,
  checkVrmFile,
  licenceRows,
  parseAvatarState,
  valueLabel,
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
  parsePersonaEditorState,
  parseVoiceInputState,
  MAX_SEED_ENTRIES,
  MAX_SEED_FILE_BYTES,
  SEED_CLOUD_NOTE,
  SEED_EXPLANATION,
  SEED_KINDS,
  SEED_MEMORY_ONLY_NOTE,
  parseMemorySeedEditorState,
  parseSeedDocument,
  seedClaimBytes,
  seedGetBody,
  seedResultText,
  seedSetBody,
  seedTextFrom,
  seedView,
  type SeedKind,
  personaBytes,
  personaGetBody,
  personaResultText,
  personaSetBody,
  personaTextFrom,
  personaView,
  MAX_PERSONA_BYTES,
  PERSONA_CLOUD_NOTE,
  PERSONA_EXPLANATION,
  PERSONA_MEMORY_ONLY_NOTE,
  parseVoiceState,
  pushChat,
  repliesFor,
  safeAvatarUrl,
  searchPageCount,
  statusRows,
  shouldReloadForHash,
  MAX_HOST_TEXT_CHARS,
  avatarLinkState,
  connectionText,
  controlResultText,
  reasonText,
  tokenFromHash,
  tuningPatch,
  voiceInputBody,
  voiceInputResultText,
  voiceInputView,
  VOICE_INPUT_CLOUD_NOTE,
  VOICE_INPUT_EXPLANATION,
  MAX_VOICE_INPUT_REASON_CHARS,
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

  test("OpenRouter-style ids (vendor/model:variant) are accepted, malformed ones are not", () => {
    const ok = "meta-llama/llama-3.3-70b-instruct:free";
    const parsed = parseModelState(model({ active: ok, available: [{ alias: ok, version: "or-free" }] }));
    expect(parsed?.active).toBe(ok);
    expect(parseModelState(model({ active: "a".repeat(96), available: [{ alias: "a".repeat(96), version: "v" }] }))).not.toBeNull();
    for (const bad of ["a//b", "/a", "a/", "a:b:c", "a:b/c", "a/b/c", "A/b", "a".repeat(97)]) {
      expect(parseModelState(model({ active: bad, available: [{ alias: bad, version: "v" }] }))).toBeNull();
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

// ---- VRM avatar choice (#362) ----

const H1 = "1".repeat(64);
const H2 = "2".repeat(64);
const H3 = "3".repeat(64);

const avMeta = (over: Record<string, unknown> = {}) => ({
  spec: "vrm1",
  name: "Test Girl",
  version: "1.0",
  authors: ["Ann"],
  copyright: null,
  contact: null,
  license_url: "https://vrm.dev/licenses/1.0/",
  other_license_url: null,
  license_name: null,
  avatar_permission: "everyone",
  commercial_usage: "corporation",
  allow_redistribution: false,
  modification: "prohibited",
  credit_notation: "unnecessary",
  allow_violent_usage: false,
  allow_sexual_usage: false,
  allow_political_or_religious_usage: true,
  allow_antisocial_or_hate_usage: false,
  warnings: [],
  credit: null,
  ...over,
});

const avModel = (id: string, over: Record<string, unknown> = {}, metaOver: Record<string, unknown> | null = {}) => ({
  id,
  size: 1024 * 1024,
  name: "Test Girl",
  warnings: [],
  meta: metaOver === null ? null : avMeta(metaOver),
  ...over,
});

const avatar = (over: Record<string, unknown> = {}) => ({
  active: "sample",
  active_id: H1,
  credit: null,
  sample: avModel(H1, { name: "Sample" }, { name: "Sample" }),
  models: [] as unknown[],
  pending: null,
  max_models: 5,
  max_upload_bytes: MAX_VRM_BYTES,
  renderer_model: H1,
  last_error: null,
  save_error: null,
  ...over,
});

const avatarState = (over: Record<string, unknown> = {}) =>
  state({ status: { ...state()!.status, avatar_model: avatar(over) } });

describe("avatar state", () => {
  test("a state without the avatar store has no avatar view", () => {
    expect(avatarView(state())).toBeNull();
    expect(avatarView(null)).toBeNull();
  });

  test("parseAvatarState accepts exactly the documented shape", () => {
    expect(parseAvatarState(avatar())).not.toBeNull();
    for (const bad of [
      null,
      "x",
      avatar({ active: "../model.vrm" }),
      avatar({ active: "ABCDEF" }),
      avatar({ active_id: "nope" }),
      avatar({ renderer_model: 7 }),
      avatar({ models: "x" }),
      avatar({ models: [avModel("short")] }),
      avatar({ models: [avModel(H2), avModel(H2)] }),
      avatar({ pending: avModel(H2, { warnings: ["made_up"] }) }),
      avatar({ pending: avModel(H2, {}, { spec: "vrm2" }) }),
      avatar({ pending: avModel(H2, {}, { authors: "Ann" }) }),
      avatar({ pending: avModel(H2, {}, { authors: Array(9).fill("a") }) }),
      avatar({ pending: avModel(H2, {}, { name: "あ".repeat(301) }) }),
      avatar({ pending: avModel(H2, {}, { allow_redistribution: "yes" }) }),
      avatar({ pending: avModel(H2, { size: -1 }) }),
      avatar({ max_models: -1 }),
      avatar({ last_error: 7 }),
    ]) {
      expect(parseAvatarState(bad)).toBeNull();
    }
  });

  test("a malformed avatar block is dropped without rejecting the whole state", () => {
    const s = state({ status: { ...state()!.status, avatar_model: { active: 7 } } });
    expect(s).not.toBeNull();
    expect(s!.status.avatar_model).toBeNull();
    expect(avatarView(s)).toBeNull();
  });
});

describe("avatar view", () => {
  test("the sample is active at first, with its licence and nothing to delete", () => {
    const view = avatarView(avatarState())!;
    expect(view.summary).toBe("使用中: サンプル — Sample");
    expect(view.health).toBe("ok");
    expect(view.choices).toHaveLength(1);
    expect(view.choices[0]).toMatchObject({ id: "sample", active: true, deletable: false });
    expect(view.pending).toBeNull();
    expect(view.mismatch).toBe(false);
    expect(view.activeRows.find(([l]) => l === "作者")?.[1]).toBe("Ann");
  });

  test("a pending model shows its licence and warns when the use may not be allowed", () => {
    const pending = avModel(
      H2,
      { name: "Restricted", warnings: ["author_only", "non_commercial_only", "credit_required"] },
      {
        name: "Restricted",
        avatar_permission: "onlyAuthor",
        commercial_usage: "personalNonProfit",
        credit_notation: "required",
        credit: "Restricted / Ann",
        warnings: ["author_only", "non_commercial_only", "credit_required"],
      },
    );
    const view = avatarView(avatarState({ pending }))!;
    expect(view.pending).not.toBeNull();
    const p = view.pending!;
    expect(p.id).toBe(H2);
    expect(p.title).toBe("Restricted");
    expect(p.restrictive).toBe(true);
    expect(p.warnings).toEqual([
      AVATAR_WARNING_TEXT.author_only,
      AVATAR_WARNING_TEXT.non_commercial_only,
      AVATAR_WARNING_TEXT.credit_required,
    ]);
    expect(p.credit).toBe("Restricted / Ann");
    expect(p.rows.find(([l]) => l === "アバターを使える人")?.[1]).toBe("作者のみ(onlyAuthor)");
    expect(p.rows.find(([l]) => l === "営利利用")?.[1]).toBe("個人・非営利のみ(personalNonProfit)");
    expect(p.question).toContain("最終的な判断はあなたに任されています");
  });

  test("a credit duty alone is not a restriction", () => {
    const pending = avModel(H2, { warnings: ["credit_required"] }, { warnings: ["credit_required"], credit: "X / Y" });
    expect(avatarView(avatarState({ pending }))!.pending!.restrictive).toBe(false);
  });

  test("stored models can be selected, the active one cannot be deleted", () => {
    const view = avatarView(
      avatarState({
        active: H2,
        active_id: H2,
        models: [avModel(H2, { name: "Two" }), avModel(H3, { name: "Three", warnings: ["licence_not_stated"] }, null)],
      }),
    )!;
    expect(view.summary).toBe("使用中: Two");
    const two = view.choices.find((c) => c.id === H2)!;
    const three = view.choices.find((c) => c.id === H3)!;
    const sample = view.choices.find((c) => c.id === "sample")!;
    expect([two.active, two.deletable]).toEqual([true, false]);
    expect([three.active, three.deletable]).toEqual([false, true]);
    expect(sample.active).toBe(false);
    expect(three.warnings).toEqual([AVATAR_WARNING_TEXT.licence_not_stated]);
    // Hash prefixes in the label let two models of one name be told apart.
    expect(two.label).toContain(H2.slice(0, 8));
  });

  test("a restrictive active model is a warning health", () => {
    const view = avatarView(
      avatarState({
        active: H2,
        active_id: H2,
        models: [avModel(H2, { warnings: ["licensed_persons_only"] }, { warnings: ["licensed_persons_only"] })],
      }),
    )!;
    expect(view.health).toBe("warn");
    expect(view.activeWarnings).toEqual([AVATAR_WARNING_TEXT.licensed_persons_only]);
  });

  test("a page that shows another model than the active one is reported", () => {
    expect(avatarView(avatarState({ renderer_model: H2 }))!.mismatch).toBe(true);
    expect(avatarView(avatarState({ renderer_model: null }))!.mismatch).toBe(false);
    expect(avatarView(avatarState({ active_id: null, renderer_model: H2 }))!.mismatch).toBe(false);
  });

  test("errors are shown, the last request's first", () => {
    expect(avatarView(avatarState({ last_error: "refused: not_glb" }))!.error).toBe("refused: not_glb");
    expect(avatarView(avatarState({ save_error: "disk full" }))!.error).toContain("disk full");
    expect(avatarView(avatarState())!.error).toBeNull();
  });

  test("model text stays plain strings and is never turned into markup or a link", () => {
    const evil = "<img src=x onerror=alert(1)>";
    const view = avatarView(
      avatarState({ pending: avModel(H2, { name: evil }, { name: evil, license_url: "javascript:alert(1)", authors: [evil] }) }),
    )!;
    const p = view.pending!;
    expect(p.title).toBe(evil);
    expect(p.rows.find(([l]) => l === "ライセンスURL")?.[1]).toBe("javascript:alert(1)");
    expect(p.rows.find(([l]) => l === "作者")?.[1]).toBe(evil);
  });
});

describe("licence rows", () => {
  test("VRM 0.x spellings and unknown values are shown as the file wrote them", () => {
    const rows = licenceRows(
      avMeta({
        spec: "vrm0",
        avatar_permission: "OnlyAuthor",
        commercial_usage: "Disallow",
        license_name: "CC_BY_NC",
        modification: null,
        credit_notation: null,
        allow_redistribution: null,
      }) as never,
    );
    expect(rows.find(([l]) => l === "規格")?.[1]).toBe("VRM 0.x");
    expect(rows.find(([l]) => l === "アバターを使える人")?.[1]).toBe("作者のみ(OnlyAuthor)");
    expect(rows.find(([l]) => l === "営利利用")?.[1]).toBe("営利利用不可(Disallow)");
    expect(rows.find(([l]) => l === "ライセンス名")?.[1]).toBe("CC_BY_NC");
    expect(rows.find(([l]) => l === "改変")?.[1]).toBe("(記載なし)");
    expect(valueLabel("commercial_usage", "somethingNew")).toBe("somethingNew");
    expect(valueLabel("commercial_usage", null)).toBe("(記載なし)");
  });
});

describe("avatar file check", () => {
  const glb = new Uint8Array([0x67, 0x6c, 0x54, 0x46, 2, 0, 0, 0, 0, 0, 0, 0]);
  test("a glTF binary of a sane size passes", () => {
    expect(checkVrmFile({ name: "a.vrm", size: 1000 }, glb)).toEqual({ ok: true });
    expect(checkVrmFile({ name: "a.vrm", size: MAX_VRM_BYTES }, glb)).toEqual({ ok: true });
  });

  test("an empty, oversized or renamed file is refused with a reason", () => {
    expect(checkVrmFile({ name: "a.vrm", size: 0 }, glb).ok).toBe(false);
    const big = checkVrmFile({ name: "a.vrm", size: MAX_VRM_BYTES + 1 }, glb);
    expect(big.ok).toBe(false);
    const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0]);
    const renamed = checkVrmFile({ name: "avatar.vrm", size: 5000 }, png);
    expect(renamed.ok).toBe(false);
    expect(checkVrmFile({ name: "a.vrm", size: 5000 }, new Uint8Array([0x67, 0x6c])).ok).toBe(false);
  });
});

const usage = (over: Record<string, unknown> = {}) => ({
  prompt_tokens: 1000,
  completion_tokens: 234,
  cost_micro_usd: 12_340,
  turns_with_usage: 3,
  turns_unknown_usage: 0,
  turns_cost_reported: 3,
  cap_micro_usd: null,
  cap_tripped: false,
  suspended_calls: 0,
  ...over,
});

const withUsage = (u: unknown) =>
  state({
    status: {
      muted: false,
      audio_error: null,
      avatar_error: null,
      stream_error: null,
      content_queue: 0,
      scheduler_items: 0,
      generation_active: false,
      thinking_usage: u,
    },
  });

describe("thinking usage (#367)", () => {
  test("no meter means no usage card", () => {
    expect(usageView(state())).toBeNull();
    expect(usageView(null)).toBeNull();
    expect(usageView(withUsage({ prompt_tokens: "many" }))).toBeNull();
    expect(parseUsageState(usage({ cost_micro_usd: -1 }))).toBeNull();
    expect(parseUsageState(usage({ cap_tripped: "yes" }))).toBeNull();
  });

  test("shows the session tokens and cost", () => {
    const view = usageView(withUsage(usage()))!;
    expect(view.summary).toBe("今回の配信: 1,234 トークン · $0.0123");
    expect(view.cap).toBe("");
    expect(view.health).toBe("ok");
  });

  test("an unreported cost is unknown, never $0", () => {
    const tokensOnly = usageView(withUsage(usage({ cost_micro_usd: null, turns_cost_reported: 0 })))!;
    expect(tokensOnly.summary).toBe("今回の配信: 1,234 トークン · 金額は不明");
    expect(tokensOnly.summary).not.toContain("$0");
    const none = usageView(withUsage(usage({ prompt_tokens: 0, completion_tokens: 0, cost_micro_usd: null, turns_with_usage: 0, turns_unknown_usage: 2, turns_cost_reported: 0 })))!;
    expect(none.summary).toContain("不明");
    expect(none.summary).not.toContain("$");
    const idle = usageView(withUsage(usage({ prompt_tokens: 0, completion_tokens: 0, cost_micro_usd: null, turns_with_usage: 0, turns_cost_reported: 0 })))!;
    expect(idle.summary).toContain("まだ応答がありません");
  });

  test("a partly reported cost is a lower bound", () => {
    const view = usageView(withUsage(usage({ turns_with_usage: 3, turns_unknown_usage: 1, turns_cost_reported: 2 })))!;
    expect(view.summary).toBe("今回の配信: 1,234 トークン · $0.0123 以上(2回分は金額不明)");
  });

  test("shows the cap, and that it tripped", () => {
    const armed = usageView(withUsage(usage({ cap_micro_usd: 5_000_000 })))!;
    expect(armed.cap).toContain("上限 $5.0000");
    expect(armed.health).toBe("ok");
    const tripped = usageView(withUsage(usage({ cap_micro_usd: 12_000, cap_tripped: true, suspended_calls: 2 })))!;
    expect(tripped.cap).toContain("上限 $0.0120 に達したため");
    expect(tripped.cap).toContain("定型の返し");
    expect(tripped.health).toBe("warn");
  });
});

// ---- First-run wording (#384) ----
describe("reasonText", () => {
  test("maps the avatar renderer states the host reports", () => {
    expect(reasonText("Unavailable: local avatar renderer is Disconnected")).toBe("アバター画面が開かれていません");
    expect(reasonText("Unavailable: local avatar renderer is Handshaking")).toContain("接続しています");
    expect(reasonText("Unavailable: local avatar renderer is Degraded")).toContain("遅れ");
    expect(avatarLinkState("Unavailable: local avatar renderer is Disconnected")).toBe("disconnected");
    expect(avatarLinkState("Unavailable: local avatar renderer is Ready")).toBe("none");
    expect(avatarLinkState(null)).toBe("none");
  });
  test("maps the stable audio messages", () => {
    expect(reasonText("Unavailable: audio output busy ending playback")).toContain("音声を止めている");
    expect(reasonText("audio stop did not complete within its bound")).toContain("確認できません");
  });
  test("unknown text passes through, empty stays empty, long text is bounded", () => {
    expect(reasonText("ffplay missing")).toBe("ffplay missing");
    expect(reasonText("")).toBe("");
    expect(reasonText(null)).toBe("");
    const long = reasonText("x".repeat(50_000));
    expect(long.length).toBeLessThanOrEqual(MAX_HOST_TEXT_CHARS + 1);
    expect(avatarLinkState("y".repeat(50_000) + "local avatar renderer is Disconnected")).toBe("none");
  });
  test("statusRows translates the avatar row and keeps the raw text", () => {
    const row = statusRows(
      state({ status: { ...state()!.status, avatar_error: "Unavailable: local avatar renderer is Disconnected" } })!,
    ).find((r) => r.label === "アバター");
    expect(row?.value).toBe("アバター画面が開かれていません");
    expect(row?.raw).toBe("Unavailable: local avatar renderer is Disconnected");
    expect(row?.health).toBe("warn");
  });
});

describe("controlResultText", () => {
  test("stop with audio and avatar detail", () => {
    const r = controlResultText("stop", true, "stopped 2 items; audio_stop=hard_stopped avatar_reset=ok", 200);
    expect(r.text).toBe("停止しました(2件の発話を止めました、音声を止めました、アバターを戻しました)");
    expect(r.raw).toBe("stopped 2 items; audio_stop=hard_stopped avatar_reset=ok");
  });
  test("each audio token and a failed reset", () => {
    expect(controlResultText("stop", true, "stopped 0 items; audio_stop=nothing_playing avatar_reset=failed", 200).text).toBe(
      "停止しました(止める発話はありませんでした、再生中の音声はありませんでした、アバターを戻せませんでした)",
    );
    expect(controlResultText("stop", true, "stopped 1 items; audio_stop=faded_40ms avatar_reset=ok", 200).text).toContain("フェード");
    expect(controlResultText("stop", true, "stopped 1 items; audio_stop=unsupported avatar_reset=ok", 200).text).toContain("対応していません");
    expect(controlResultText("stop", true, "stopped 1 items; audio_stop=unconfirmed avatar_reset=ok", 200).text).toContain("確認できません");
  });
  test("mute, unknown token and unknown detail", () => {
    expect(controlResultText("mute", true, "muted; audio_stop=hard_stopped avatar_reset=ok", 200).text).toBe(
      "ミュートしました(音声を止めました、アバターを戻しました)",
    );
    expect(controlResultText("mute", true, "muted", 200).text).toBe("ミュートしました");
    expect(controlResultText("stop", true, "stopped 1 items; audio_stop=weird avatar_reset=ok", 200).text).toContain("音声: weird");
    expect(controlResultText("stop", true, "something new", 200).text).toBe("停止しました: something new");
    expect(controlResultText("stop", true, "stopped 1 items; audio_stop=__proto__ avatar_reset=ok", 200).text).toContain("音声: __proto__");
  });
  test("failure, missing and oversized detail never throw", () => {
    expect(controlResultText("stop", false, "rejected: nope", 200).text).toBe("失敗しました: rejected: nope");
    expect(controlResultText("stop", false, undefined, 500).text).toBe("失敗しました (500)");
    expect(controlResultText("stop", true, 42, 200).text).toBe("停止しました");
    expect(controlResultText("stop", true, "z".repeat(100_000), 200).text.length).toBeLessThan(MAX_HOST_TEXT_CHARS + 20);
  });
});

describe("connectionText", () => {
  test("connected, connecting and lost are distinct", () => {
    const texts = (["connecting", "connected", "reconnecting", "lost", "no-token", "mismatch"] as const).map((kind) => connectionText({ kind }));
    expect(new Set(texts).size).toBe(texts.length);
    expect(connectionText({ kind: "connected" })).toBe("接続済み");
    expect(connectionText({ kind: "reconnecting" })).toContain("再接続中");
    expect(connectionText({ kind: "lost" })).toContain("切断されました");
    expect(connectionText({ kind: "http-error", status: 502 })).toContain("502");
    expect(connectionText({ kind: "http-error", status: Number.NaN })).toContain("0");
  });
});

describe("shouldReloadForHash", () => {
  test("reloads only for a well-formed token fragment", () => {
    expect(shouldReloadForHash("#token=" + "cd".repeat(32))).toBe(true);
    expect(shouldReloadForHash("")).toBe(false);
    expect(shouldReloadForHash("#")).toBe(false);
    expect(shouldReloadForHash("#token=")).toBe(false);
    expect(shouldReloadForHash("#token=short")).toBe(false);
    expect(shouldReloadForHash("#x=1&token=" + "cd".repeat(32))).toBe(false);
  });
});

// ---- voice input (#400) ----

const voiceInput = (over: Record<string, unknown> = {}) => ({
  composed: true,
  state: "off",
  thinking_remote: false,
  ...over,
});

const withVoiceInput = (value: unknown) =>
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
      ...(value === undefined ? {} : { voice_input: value }),
    },
    replies: [],
    avatar_url: null,
  });

describe("voice input state", () => {
  test("a host that does not send the field hides the toggle", () => {
    // An older app: no `voice_input` key at all.
    const older = withVoiceInput(undefined);
    expect(older).not.toBeNull();
    expect(older!.status.voice_input ?? null).toBeNull();
    expect(voiceInputView(older)).toBeNull();
    expect(voiceInputView(null)).toBeNull();
    expect(voiceInputView(state())).toBeNull();
  });

  test("anything but the documented shape is dropped, not fatal", () => {
    for (const bad of [
      null,
      "on",
      7,
      [],
      {},
      voiceInput({ composed: "yes" }),
      voiceInput({ composed: undefined }),
      voiceInput({ state: "listening" }),
      voiceInput({ state: 1 }),
      voiceInput({ thinking_remote: "no" }),
      voiceInput({ thinking_remote: undefined }),
      voiceInput({ state: "failed", reason: 3 }),
      voiceInput({ state: "failed", reason: "x".repeat(MAX_VOICE_INPUT_REASON_CHARS + 1) }),
    ]) {
      expect(parseVoiceInputState(bad)).toBeNull();
      const parsed = withVoiceInput(bad);
      expect(parsed).not.toBeNull();
      expect(voiceInputView(parsed)).toBeNull();
    }
    expect(parseVoiceInputState(voiceInput())).toEqual({
      composed: true,
      state: "off",
      reason: null,
      thinking_remote: false,
    });
    // The host omits `reason` unless failed; null and the longest reason are fine.
    expect(parseVoiceInputState(voiceInput({ state: "failed", reason: null }))!.reason).toBeNull();
    const longest = "あ".repeat(MAX_VOICE_INPUT_REASON_CHARS);
    expect(parseVoiceInputState(voiceInput({ state: "failed", reason: longest }))!.reason).toBe(longest);
  });

  test("a state that is not composed stays hidden", () => {
    expect(voiceInputView(withVoiceInput(voiceInput({ composed: false })))).toBeNull();
    expect(voiceInputView(withVoiceInput(voiceInput({ composed: false, state: "on" })))).toBeNull();
  });

  test("the three states in Japanese, with the buttons each one allows", () => {
    const off = voiceInputView(withVoiceInput(voiceInput()))!;
    expect(off.stateText).toBe("オフ");
    expect(off.health).toBe("ok");
    expect(off.explanation).toBe(VOICE_INPUT_EXPLANATION);
    expect(off.explanation).toBe("オンにすると、マイクで聞き取った音声に返事をします");
    expect([off.canEnable, off.canDisable, off.enableLabel]).toEqual([true, false, "オンにする"]);

    const on = voiceInputView(withVoiceInput(voiceInput({ state: "on" })))!;
    expect(on.stateText).toBe("聞き取り中");
    expect(on.health).toBe("ok");
    expect([on.canEnable, on.canDisable]).toEqual([false, true]);

    const failed = voiceInputView(
      withVoiceInput(voiceInput({ state: "failed", reason: "sidecar exited (exit code: 3)" })),
    )!;
    expect(failed.stateText).toBe("失敗: sidecar exited (exit code: 3)");
    expect(failed.health).toBe("warn");
    expect([failed.canEnable, failed.canDisable, failed.enableLabel]).toEqual([true, true, "もう一度オンにする"]);
    const noReason = voiceInputView(withVoiceInput(voiceInput({ state: "failed" })))!;
    expect(noReason.stateText).toBe("失敗");
  });

  test("the cloud sentence shows in every state exactly when Thinking is remote", () => {
    for (const s of ["off", "on", "failed"]) {
      const local = voiceInputView(withVoiceInput(voiceInput({ state: s, thinking_remote: false })))!;
      expect(local.cloudNote).toBeNull();
      const remote = voiceInputView(withVoiceInput(voiceInput({ state: s, thinking_remote: true })))!;
      expect(remote.cloudNote).toBe(VOICE_INPUT_CLOUD_NOTE);
    }
    // Before the first turn-on, while off.
    const before = voiceInputView(withVoiceInput(voiceInput({ thinking_remote: true })))!;
    expect(before.canEnable && before.cloudNote !== null).toBe(true);
    expect(VOICE_INPUT_CLOUD_NOTE).toContain("クラウド");
  });

  test("the host's failure text is data: carried as is for textContent, never interpreted", () => {
    const hostile = "<img src=x onerror=alert(1)> &amp; `x`";
    const view = voiceInputView(withVoiceInput(voiceInput({ state: "failed", reason: hostile })))!;
    expect(view.stateText).toBe(`失敗: ${hostile}`);
  });
});

describe("voice input request", () => {
  test("the body is a boolean and nothing else", () => {
    expect(voiceInputBody(true)).toEqual({ enabled: true });
    expect(voiceInputBody(false)).toEqual({ enabled: false });
    expect(Object.keys(voiceInputBody(true))).toEqual(["enabled"]);
    expect(JSON.stringify(voiceInputBody(false))).toBe('{"enabled":false}');
  });

  test("the result line", () => {
    expect(voiceInputResultText(true, true, "voice input is on", 200)).toBe("音声入力をオンにしました");
    expect(voiceInputResultText(false, true, "voice input is off", 200)).toBe("音声入力をオフにしました");
    expect(voiceInputResultText(true, false, "voice input could not start: entity not found", 200)).toBe(
      "オンにすることができませんでした: voice input could not start: entity not found",
    );
    expect(voiceInputResultText(false, false, undefined, 503)).toBe("オフにすることができませんでした (503)");
    // A 200 whose body says not ok is a failure; so is an ok body with a bad status.
    expect(voiceInputResultText(true, true, "x", 500)).toContain("できませんでした");
    // Host text stays bounded.
    expect(voiceInputResultText(true, false, "x".repeat(5000), 200).length).toBeLessThan(MAX_HOST_TEXT_CHARS + 60);
  });
});

// ---- character sheet (#401) ----

const personaEditor = (over: Record<string, unknown> = {}) => ({
  persisted: true,
  thinking_remote: false,
  ...over,
});

const withPersonaEditor = (value: unknown) =>
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
      ...(value === undefined ? {} : { persona_editor: value }),
    },
    replies: [],
    avatar_url: null,
  });

describe("character sheet editor state", () => {
  test("a host that does not send the field hides the editor", () => {
    const older = withPersonaEditor(undefined);
    expect(older).not.toBeNull();
    expect(older!.status.persona_editor ?? null).toBeNull();
    expect(personaView(older, "")).toBeNull();
    expect(personaView(null, "")).toBeNull();
    expect(personaView(state(), "")).toBeNull();
  });

  test("anything but the documented shape is dropped, not fatal", () => {
    for (const bad of [
      null,
      "on",
      7,
      [],
      {},
      personaEditor({ persisted: "yes" }),
      personaEditor({ persisted: undefined }),
      personaEditor({ thinking_remote: 1 }),
      personaEditor({ thinking_remote: undefined }),
    ]) {
      expect(parsePersonaEditorState(bad)).toBeNull();
      const parsed = withPersonaEditor(bad);
      expect(parsed).not.toBeNull();
      expect(personaView(parsed, "")).toBeNull();
    }
    expect(parsePersonaEditorState(personaEditor())).toEqual({ persisted: true, thinking_remote: false });
  });
});

describe("character sheet view", () => {
  test("the counter measures UTF-8 bytes the way the host normalises", () => {
    expect(personaBytes("")).toBe(0);
    expect(personaBytes("abc")).toBe(3);
    expect(personaBytes("ミコト")).toBe(9);
    // CRLF and lone CR count as one byte, trailing whitespace is not counted.
    expect(personaBytes("a\r\nb\rc\n\n  ")).toBe(5);
    expect(personaBytes("あ".repeat(1000))).toBe(MAX_PERSONA_BYTES);
    const view = personaView(withPersonaEditor(personaEditor()), "ミコト")!;
    expect(view.counterText).toBe("9 / 3,000 バイト");
    expect(view.health).toBe("ok");
    expect(view.canSave).toBe(true);
  });

  test("over the limit warns and cannot be saved; exactly at the limit can", () => {
    const state = withPersonaEditor(personaEditor());
    const at = personaView(state, "あ".repeat(1000))!;
    expect([at.health, at.canSave]).toEqual(["ok", true]);
    const over = personaView(state, "あ".repeat(1000) + "a")!;
    expect([over.health, over.canSave]).toEqual(["warn", false]);
    expect(over.counterText).toBe("3,001 / 3,000 バイト");
    // Empty is allowed: it clears the sheet.
    expect(personaView(state, "")!.canSave).toBe(true);
  });

  test("the cloud sentence shows exactly when Thinking is remote", () => {
    const local = personaView(withPersonaEditor(personaEditor({ thinking_remote: false })), "x")!;
    expect(local.cloudNote).toBeNull();
    const remote = personaView(withPersonaEditor(personaEditor({ thinking_remote: true })), "x")!;
    expect(remote.cloudNote).toBe(PERSONA_CLOUD_NOTE);
    expect(PERSONA_CLOUD_NOTE).toContain("クラウドのモデルへ送信されます");
    expect(PERSONA_CLOUD_NOTE).toContain("sent to the cloud model with every reply");
  });

  test("a save that is not written to a file says it will be lost on restart", () => {
    const kept = personaView(withPersonaEditor(personaEditor({ persisted: true })), "x")!;
    expect(kept.memoryOnlyNote).toBeNull();
    const memory = personaView(withPersonaEditor(personaEditor({ persisted: false })), "x")!;
    expect(memory.memoryOnlyNote).toBe(PERSONA_MEMORY_ONLY_NOTE);
    expect(kept.explanation).toBe(PERSONA_EXPLANATION);
  });
});

describe("character sheet requests", () => {
  test("the set body is the text and nothing else; get is an empty object", () => {
    expect(personaSetBody("名前: ミコト\n")).toEqual({ text: "名前: ミコト\n" });
    expect(Object.keys(personaSetBody("x"))).toEqual(["text"]);
    expect(JSON.stringify(personaSetBody("a\"b\\c\n"))).toBe('{"text":"a\\"b\\\\c\\n"}');
    expect(JSON.stringify(personaGetBody())).toBe("{}");
  });

  test("the text of a get answer is taken only when it is a string within the bound", () => {
    expect(personaTextFrom({ ok: true, text: "名前: ミコト" })).toBe("名前: ミコト");
    expect(personaTextFrom({ ok: true, text: "" })).toBe("");
    for (const bad of [null, "x", 7, {}, { text: 3 }, { text: null }, { text: "あ".repeat(1001) }]) {
      expect(personaTextFrom(bad)).toBeNull();
    }
  });

  test("the result lines", () => {
    expect(personaResultText("save", true, "x", 200)).toBe("保存しました。次の返答から反映されます");
    expect(personaResultText("reload", true, "x", 200)).toBe("読み込みました");
    expect(personaResultText("save", false, "persona not changed: larger than 3000 bytes", 409)).toBe(
      "保存できませんでした: persona not changed: larger than 3000 bytes",
    );
    expect(personaResultText("reload", false, undefined, 503)).toBe("読み込みできませんでした (503)");
    expect(personaResultText("save", true, "x", 500)).toContain("できませんでした");
    expect(personaResultText("save", false, "x".repeat(5000), 409).length).toBeLessThan(MAX_HOST_TEXT_CHARS + 60);
  });
});

// ---- memory seed (#402) ----

const seedEditor = (over: Record<string, unknown> = {}) => ({
  persisted: true,
  thinking_remote: false,
  entries: 2,
  content_hash: "ab".repeat(32),
  ...over,
});

const withSeedEditor = (value: unknown) =>
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
      ...(value === undefined ? {} : { memory_seed_editor: value }),
    },
    replies: [],
    avatar_url: null,
  });

const seedEntry = (claim: string, kind: SeedKind = "persona-fact") => ({ kind, claim });

describe("memory seed editor state", () => {
  test("a host that does not send the field hides the card", () => {
    const older = withSeedEditor(undefined);
    expect(older).not.toBeNull();
    expect(older!.status.memory_seed_editor ?? null).toBeNull();
    expect(seedView(older, [])).toBeNull();
    expect(seedView(null, [])).toBeNull();
  });

  test("anything but the documented shape is dropped, not fatal", () => {
    for (const bad of [
      null,
      "on",
      7,
      [],
      {},
      seedEditor({ persisted: "yes" }),
      seedEditor({ thinking_remote: undefined }),
      seedEditor({ entries: -1 }),
      seedEditor({ entries: 1.5 }),
      seedEditor({ entries: "2" }),
      seedEditor({ content_hash: 3 }),
    ]) {
      expect(parseMemorySeedEditorState(bad)).toBeNull();
      const parsed = withSeedEditor(bad);
      expect(parsed).not.toBeNull();
      expect(seedView(parsed, [])).toBeNull();
    }
    expect(parseMemorySeedEditorState(seedEditor())).toEqual(seedEditor());
  });
});

describe("memory seed document", () => {
  const doc = (over: Record<string, unknown> = {}) =>
    JSON.stringify({
      schema_version: "0.1.0",
      seed_id: "s",
      revision: "1",
      entries: [seedEntry("ミコトは猫が好き"), seedEntry("冷蔵庫はいつも空", "running-joke")],
      ...over,
    });

  test("a get answer is taken only when it is exactly the seed shape within the bounds", () => {
    const parsed = parseSeedDocument(doc());
    expect(parsed?.entries.length).toBe(2);
    expect(parsed?.seed_id).toBe("s");
    for (const bad of [
      null,
      7,
      "",
      "not json",
      "[]",
      "null",
      doc({ schema_version: "9.9.9" }),
      doc({ seed_id: 3 }),
      doc({ entries: "x" }),
      doc({ entries: [seedEntry("a", "viewer-fact" as SeedKind)] }),
      doc({ entries: [{ kind: "persona-fact" }] }),
      doc({ entries: Array.from({ length: MAX_SEED_ENTRIES + 1 }, (_, i) => seedEntry(`c${i}`)) }),
      doc({ entries: [seedEntry("a".repeat(MAX_SEED_FILE_BYTES))] }),
    ]) {
      expect(parseSeedDocument(bad)).toBeNull();
    }
    expect(seedTextFrom({ ok: true, text: "x" })).toBe("x");
    for (const bad of [null, "x", 7, {}, { text: 3 }, { text: null }]) expect(seedTextFrom(bad)).toBeNull();
  });

  test("the set body is the whole document with trimmed claims and nothing else", () => {
    const body = seedSetBody({ seed_id: "s", revision: "1" }, [seedEntry("  ミコト  "), seedEntry("b", "commitment")]);
    expect(Object.keys(body)).toEqual(["schema_version", "seed_id", "revision", "entries"]);
    expect(body).toEqual({
      schema_version: "0.1.0",
      seed_id: "s",
      revision: "1",
      entries: [seedEntry("ミコト"), seedEntry("b", "commitment")],
    });
    expect(Object.keys(body.entries[0]!)).toEqual(["kind", "claim"]);
    expect(JSON.stringify(seedGetBody())).toBe("{}");
    // What the card sends is what the card accepts back.
    expect(parseSeedDocument(JSON.stringify(body))).toEqual(body);
  });
});

describe("memory seed view", () => {
  const state = (over: Record<string, unknown> = {}) => withSeedEditor(seedEditor(over));

  test("the claim counter measures UTF-8 bytes of the trimmed claim", () => {
    expect(seedClaimBytes("")).toBe(0);
    expect(seedClaimBytes("abc")).toBe(3);
    expect(seedClaimBytes("ミコト")).toBe(9);
    expect(seedClaimBytes("  ミコト \t")).toBe(9);
    const view = seedView(state(), [seedEntry("ミコト")])!;
    expect(view.rows[0]!.bytesText).toBe("9 / 240 バイト");
    expect(view.rows[0]!.kindLabel).toBe("人物設定");
    expect(view.countText).toBe("1 / 128 件");
    expect(view.canSave && view.canAdd).toBe(true);
  });

  test("a claim at exactly 240 bytes saves; one byte more does not", () => {
    const at = seedView(state(), [seedEntry("あ".repeat(80))])!;
    expect([at.rows[0]!.health, at.canSave]).toEqual(["ok", true]);
    const over = seedView(state(), [seedEntry("あ".repeat(80) + "a")])!;
    expect([over.rows[0]!.health, over.canSave]).toEqual(["warn", false]);
    expect(over.rows[0]!.problem).toContain("240");
    expect(over.rows[0]!.bytesText).toBe("241 / 240 バイト");
  });

  test("empty, multi-line and repeated claims block saving, with the reason on the row", () => {
    expect(seedView(state(), [seedEntry("  ")])!.rows[0]!.problem).toContain("空");
    expect(seedView(state(), [seedEntry("a\nb")])!.rows[0]!.problem).toContain("改行");
    expect(seedView(state(), [seedEntry("a\u0000b")])!.canSave).toBe(false);
    const repeated = seedView(state(), [seedEntry("Same  claim"), seedEntry("same CLAIM", "commitment")])!;
    expect(repeated.rows[0]!.problem).toBeNull();
    expect(repeated.rows[1]!.problem).toContain("同じ内容");
    expect(repeated.canSave).toBe(false);
    // The empty seed is a valid document (it clears the memory).
    expect(seedView(state(), [])!.canSave).toBe(true);
  });

  test("the entry limit stops adding and saving", () => {
    const full = Array.from({ length: MAX_SEED_ENTRIES }, (_, i) => seedEntry(`claim ${i}`));
    const atLimit = seedView(state(), full)!;
    expect([atLimit.canAdd, atLimit.canSave, atLimit.health]).toEqual([false, true, "ok"]);
    const over = seedView(state(), [...full, seedEntry("one more")])!;
    expect([over.canSave, over.health]).toEqual([false, "warn"]);
    expect(over.countText).toBe("129 / 128 件");
  });

  test("the cloud sentence shows exactly when Thinking is remote", () => {
    expect(seedView(state({ thinking_remote: false }), [])!.cloudNote).toBeNull();
    const remote = seedView(state({ thinking_remote: true }), [])!;
    expect(remote.cloudNote).toBe(SEED_CLOUD_NOTE);
    expect(SEED_CLOUD_NOTE).toContain("関連する項目は返答のたびにクラウドのモデルへ送信されます");
    expect(SEED_CLOUD_NOTE).toContain("relevant entries are sent to the cloud model");
  });

  test("a save that is not written to a file says it will be lost on restart", () => {
    expect(seedView(state({ persisted: true }), [])!.memoryOnlyNote).toBeNull();
    expect(seedView(state({ persisted: false }), [])!.memoryOnlyNote).toBe(SEED_MEMORY_ONLY_NOTE);
    expect(seedView(state(), [])!.explanation).toBe(SEED_EXPLANATION);
  });

  test("the kind labels cover exactly the four schema kinds", () => {
    expect(SEED_KINDS.map((k) => k.kind)).toEqual(["persona-fact", "running-joke", "commitment", "segment-summary"]);
  });

  test("the result lines", () => {
    expect(seedResultText("save", true, "x", 200)).toBe("保存しました。次の返答から反映されます");
    expect(seedResultText("reload", true, "x", 200)).toBe("読み込みました");
    expect(seedResultText("save", false, "memory seed not changed: shorten it", 409)).toBe(
      "保存できませんでした: memory seed not changed: shorten it",
    );
    expect(seedResultText("reload", false, undefined, 503)).toBe("読み込みできませんでした (503)");
    expect(seedResultText("save", true, "x", 500)).toContain("できませんでした");
    expect(seedResultText("save", false, "x".repeat(5000), 409).length).toBeLessThan(MAX_HOST_TEXT_CHARS + 60);
  });
});
