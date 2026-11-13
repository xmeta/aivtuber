// Pure logic of the local operation panel (issue #335): no DOM, no network, so
// bun can test it. The panel page (panel.ts) is a thin shell around this.

/** The state document `GET /api/state` serves (crates/app/src/panel.rs). */
export interface PanelState {
  schema: string;
  uptime_ms: number;
  status: {
    muted: boolean;
    audio_error: string | null;
    avatar_error: string | null;
    stream_error: string | null;
    content_queue: number;
    scheduler_items: number;
    generation_active: boolean;
    /** The local Thinking model selector (#357); absent unless the app composed it. */
    thinking_model?: ThinkingModelState | null;
    /** The voice settings (#360); absent unless the app composed them. */
    voice?: VoiceState | null;
    /** The stream-chat connections (#361); absent unless the app composed them. */
    chat_ingress?: ChatIngressState | null;
    /** The VRM avatar choice (#362); absent unless the app composed it. */
    avatar_model?: AvatarState | null;
  };
  replies: { seq: number; event_id: string; text: string }[];
  avatar_url: string | null;
}

export interface ModelOption {
  alias: string;
  version: string;
}

export interface ThinkingModelState {
  active: string;
  active_version: string;
  state: "ready" | "switching" | "failed";
  /** The model being loaded, or the one that failed to load. */
  target: string | null;
  available: ModelOption[];
}

export const STATE_SCHEMA = "local-panel/1";
/** Mirrors `MAX_THINKING_MODEL_CHOICES` and the alias shape in crates/domain. */
export const MAX_MODEL_CHOICES = 8;
const MODEL_ALIAS = /^[a-z0-9][a-z0-9._-]{0,63}$/;
const MODEL_VERSION = /^[A-Za-z0-9._-]{1,32}$/;
export const MAX_CHAT_CHARS = 500;
/** Chat lines kept in the page. Bounded like everything on the Rust side. */
export const MAX_CHAT_LOG = 30;

/**
 * Reads the per-launch secret from a URL fragment (`#token=...`). The fragment
 * is never sent to the server, so the secret stays out of server logs and the
 * Referer header. Only the exact 64-hex form the server mints is accepted.
 */
export function tokenFromHash(hash: string): string | null {
  const match = /^#token=([0-9a-f]{64})$/.exec(hash);
  return match?.[1] ?? null;
}

/**
 * The selector state, only if it has exactly the documented shape. Anything
 * else (a newer or older app, a malformed document) yields `null`, which hides
 * the selector rather than offering a model the app would refuse.
 */
export function parseModelState(value: unknown): ThinkingModelState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  const { active, active_version, state, target, available } = v;
  if (typeof active !== "string" || !MODEL_ALIAS.test(active)) return null;
  if (typeof active_version !== "string" || !MODEL_VERSION.test(active_version)) return null;
  if (state !== "ready" && state !== "switching" && state !== "failed") return null;
  if (target !== null && (typeof target !== "string" || !MODEL_ALIAS.test(target))) return null;
  if (!Array.isArray(available) || available.length === 0 || available.length > MAX_MODEL_CHOICES) return null;
  const options: ModelOption[] = [];
  for (const entry of available) {
    if (typeof entry !== "object" || entry === null) return null;
    const { alias, version } = entry as Record<string, unknown>;
    if (typeof alias !== "string" || !MODEL_ALIAS.test(alias)) return null;
    if (typeof version !== "string" || !MODEL_VERSION.test(version)) return null;
    if (options.some((o) => o.alias === alias)) return null;
    options.push({ alias, version });
  }
  return { active, active_version, state, target: target ?? null, available: options };
}

export interface ModelView {
  options: { alias: string; label: string }[];
  /** Alias to show as chosen: the one being loaded while switching. */
  selected: string;
  /** Switching is in progress, or there is nothing to switch to. */
  disabled: boolean;
  message: string;
  health: Health;
}

/** What the model selector shows, or `null` when the app has no selector. */
export function modelView(state: PanelState | null): ModelView | null {
  const m = state?.status.thinking_model ?? null;
  if (m === null) return null;
  const options = m.available.map((o) => ({ alias: o.alias, label: `${o.alias} (${o.version})` }));
  if (m.state === "switching") {
    return {
      options,
      selected: m.target ?? m.active,
      disabled: true,
      message: `切り替え中: ${m.target ?? m.active} を読み込んでいます。数秒から1分ほどかかり、その間の返答は定型の返しになります`,
      health: "warn",
    };
  }
  if (m.state === "failed") {
    return {
      options,
      selected: m.active,
      disabled: options.length < 2,
      message: `${m.target ?? "モデル"} への切り替えに失敗しました。${m.active} のままです`,
      health: "warn",
    };
  }
  return {
    options,
    selected: m.active,
    disabled: options.length < 2,
    message:
      options.length < 2
        ? `使用中: ${m.active}(モデルは1つだけインストールされています。追加するには aivtuber-setup local --with-model qwen3-4b)`
        : `使用中: ${m.active}`,
    health: "ok",
  };
}

/** A state document is only trusted if it has the shape this page was built for. */
export function parseState(value: unknown): PanelState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Partial<PanelState>;
  if (v.schema !== STATE_SCHEMA || typeof v.status !== "object" || v.status === null) return null;
  if (!Array.isArray(v.replies) || typeof v.uptime_ms !== "number") return null;
  if (typeof v.status.muted !== "boolean") return null;
  // An absent or malformed selector is dropped, not fatal: the rest still works.
  const thinkingModel = parseModelState(v.status.thinking_model);
  const voice = parseVoiceState(v.status.voice);
  const chatIngress = parseChatIngressState(v.status.chat_ingress);
  const avatarModel = parseAvatarState(v.status.avatar_model);
  return {
    ...(v as PanelState),
    status: { ...v.status, thinking_model: thinkingModel, voice, chat_ingress: chatIngress, avatar_model: avatarModel },
  };
}

/** Only an http(s) link to the loopback avatar page is offered as a link. */
export function safeAvatarUrl(url: string | null | undefined): string | null {
  if (!url) return null;
  try {
    const u = new URL(url);
    if (u.protocol === "http:" && (u.hostname === "127.0.0.1" || u.hostname === "localhost")) return u.href;
  } catch {
    // fall through
  }
  return null;
}

export type Health = "ok" | "warn";

export interface StatusRow {
  label: string;
  value: string;
  health: Health;
}

/** The rows the status card shows, in order, with Japanese labels. */
export function statusRows(state: PanelState): StatusRow[] {
  const s = state.status;
  const err = (e: string | null): StatusRow["value"] => (e === null ? "正常" : e);
  return [
    { label: "ミュート", value: s.muted ? "ミュート中(再起動まで解除不可)" : "いいえ", health: s.muted ? "warn" : "ok" },
    { label: "音声出力", value: err(s.audio_error), health: s.audio_error === null ? "ok" : "warn" },
    { label: "アバター", value: err(s.avatar_error), health: s.avatar_error === null ? "ok" : "warn" },
    { label: "配信連携", value: err(s.stream_error), health: s.stream_error === null ? "ok" : "warn" },
    { label: "応答生成", value: s.generation_active ? "生成中" : "待機中", health: "ok" },
    { label: "入力待ち", value: String(s.content_queue), health: "ok" },
    { label: "再生予定", value: String(s.scheduler_items), health: "ok" },
  ];
}

export interface ChatEntry {
  /** Event id the runtime gave the line (`panel-chat-N`), once accepted. */
  id: string | null;
  text: string;
  /** Pending until the runtime answers, then the admission result. */
  note: string;
}

/** Adds an entry, keeping only the newest `MAX_CHAT_LOG`. */
export function pushChat(log: ChatEntry[], entry: ChatEntry): ChatEntry[] {
  const next = [...log, entry];
  return next.length > MAX_CHAT_LOG ? next.slice(next.length - MAX_CHAT_LOG) : next;
}

/** Replies that answer a given panel line (matched by event id). */
export function repliesFor(state: PanelState | null, id: string | null): string[] {
  if (state === null || id === null) return [];
  return state.replies.filter((r) => r.event_id === id).map((r) => r.text);
}

/** Replies that answer something other than a panel line (stdin, speech...). */
export function otherReplies(state: PanelState | null): { seq: number; text: string }[] {
  if (state === null) return [];
  return state.replies.filter((r) => !r.event_id.startsWith("panel-chat-")).map(({ seq, text }) => ({ seq, text }));
}

/** Text to send, or an explanation of why it cannot be sent. */
export function checkChat(raw: string): { ok: true; text: string } | { ok: false; reason: string } {
  const text = raw.trim();
  if (text === "") return { ok: false, reason: "メッセージが空です" };
  if ([...text].length > MAX_CHAT_CHARS) return { ok: false, reason: `${MAX_CHAT_CHARS}文字までです` };
  return { ok: true, text };
}

// ---- Voice settings (#360) ----

/** Mirrors crates/domain/src/voice_settings.rs. */
export const SPEAKING_RATE_RANGE = [0.5, 2.0] as const;
export const PITCH_RANGE = [-1.0, 1.0] as const;
export const VOLUME_RANGE = [0.0, 2.0] as const;
export const EMOTIONAL_INTENSITY_RANGE = [0.0, 2.0] as const;
export const MAX_STYLE_ID = 31;
export const MAX_CREDIT_CHARS = 200;
export const MAX_KEY_CHARS = 256;
export const MAX_KEYWORD_CHARS = 64;
/** The search page size the app asks for (the API's own default). */
export const SEARCH_PAGE_SIZE = 24;
const UUID = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/;

export type KeyCheck = "unchecked" | "checking" | "valid" | "rejected" | "network_error" | "unexpected";
const KEY_CHECKS: readonly string[] = ["unchecked", "checking", "valid", "rejected", "network_error", "unexpected"];

export interface Tuning {
  speaking_rate: number;
  pitch: number;
  volume: number;
  emotional_intensity: number;
}

export interface VoiceState {
  /** The voice that speaks now. */
  backend: "local" | "aivis";
  model_uuid: string | null;
  model_name: string | null;
  speaker_uuid: string | null;
  style_id: number | null;
  tuning: Tuning;
  credit: string | null;
  key: { set: boolean; source: "environment" | "file" | null; check: KeyCheck };
  last_error: string | null;
  save_error: string | null;
  preview_text: string;
}

const inRange = (value: unknown, [low, high]: readonly [number, number]): value is number =>
  typeof value === "number" && Number.isFinite(value) && value >= low && value <= high;

const optionalText = (value: unknown, max: number): string | null | undefined => {
  if (value === null) return null;
  return typeof value === "string" && [...value].length <= max ? value : undefined;
};

/**
 * The voice state, only if it has exactly the documented shape. Anything else
 * yields `null`, which hides the voice section instead of offering controls the
 * app would refuse.
 */
export function parseVoiceState(value: unknown): VoiceState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (v.backend !== "local" && v.backend !== "aivis") return null;
  const modelUuid = v.model_uuid === null ? null : typeof v.model_uuid === "string" && UUID.test(v.model_uuid) ? v.model_uuid : undefined;
  const speakerUuid = v.speaker_uuid === null ? null : typeof v.speaker_uuid === "string" && UUID.test(v.speaker_uuid) ? v.speaker_uuid : undefined;
  if (modelUuid === undefined || speakerUuid === undefined) return null;
  const styleId =
    v.style_id === null ? null : Number.isInteger(v.style_id) && (v.style_id as number) >= 0 && (v.style_id as number) <= MAX_STYLE_ID ? (v.style_id as number) : undefined;
  if (styleId === undefined) return null;
  const modelName = optionalText(v.model_name, 80);
  const credit = optionalText(v.credit, MAX_CREDIT_CHARS);
  const lastError = optionalText(v.last_error, 300);
  const saveError = optionalText(v.save_error, 300);
  if (modelName === undefined || credit === undefined || lastError === undefined || saveError === undefined) return null;
  const t = v.tuning as Record<string, unknown> | null;
  if (typeof t !== "object" || t === null) return null;
  if (
    !inRange(t.speaking_rate, SPEAKING_RATE_RANGE) ||
    !inRange(t.pitch, PITCH_RANGE) ||
    !inRange(t.volume, VOLUME_RANGE) ||
    !inRange(t.emotional_intensity, EMOTIONAL_INTENSITY_RANGE)
  ) {
    return null;
  }
  const k = v.key as Record<string, unknown> | null;
  if (typeof k !== "object" || k === null || typeof k.set !== "boolean") return null;
  if (k.source !== null && k.source !== "environment" && k.source !== "file") return null;
  if (typeof k.check !== "string" || !KEY_CHECKS.includes(k.check)) return null;
  if (typeof v.preview_text !== "string" || [...v.preview_text].length > 200) return null;
  return {
    backend: v.backend,
    model_uuid: modelUuid,
    model_name: modelName,
    speaker_uuid: speakerUuid,
    style_id: styleId,
    tuning: {
      speaking_rate: t.speaking_rate,
      pitch: t.pitch,
      volume: t.volume,
      emotional_intensity: t.emotional_intensity,
    },
    credit,
    key: { set: k.set, source: k.source, check: k.check as KeyCheck },
    last_error: lastError,
    save_error: saveError,
    preview_text: v.preview_text,
  };
}

export interface VoiceStyle {
  name: string;
  style_id: number;
}
export interface VoiceSpeaker {
  uuid: string;
  name: string;
  styles: VoiceStyle[];
}
export interface VoiceModel {
  uuid: string;
  name: string;
  description: string;
  category: string;
  voice_timbre: string;
  licenses: string[];
  speakers: VoiceSpeaker[];
}
export interface VoiceSearchPage {
  total: number;
  page: number;
  models: VoiceModel[];
}

const shortText = (value: unknown, max: number): value is string => typeof value === "string" && [...value].length <= max;

/**
 * The search answer, only the documented typed shape. A model that does not
 * match it is dropped (not the page), the lists are re-bounded here as well as
 * in the app, and nothing is interpreted: every string is shown as text.
 */
export function parseSearchPage(value: unknown): VoiceSearchPage | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  const page = v.page as Record<string, unknown> | null;
  if (typeof page !== "object" || page === null) return null;
  if (!Number.isInteger(page.total) || (page.total as number) < 0) return null;
  if (!Number.isInteger(page.page) || (page.page as number) < 1) return null;
  if (!Array.isArray(page.models)) return null;
  const models: VoiceModel[] = [];
  for (const raw of page.models.slice(0, SEARCH_PAGE_SIZE)) {
    const m = raw as Record<string, unknown> | null;
    if (typeof m !== "object" || m === null) continue;
    if (typeof m.uuid !== "string" || !UUID.test(m.uuid)) continue;
    if (!shortText(m.name, 80) || m.name.trim() === "") continue;
    if (!shortText(m.description, 400) || !shortText(m.category, 40) || !shortText(m.voice_timbre, 40)) continue;
    if (!Array.isArray(m.licenses) || !m.licenses.slice(0, 3).every((l) => shortText(l, 40))) continue;
    if (!Array.isArray(m.speakers)) continue;
    const speakers: VoiceSpeaker[] = [];
    for (const rawSpeaker of m.speakers.slice(0, 8)) {
      const s = rawSpeaker as Record<string, unknown> | null;
      if (typeof s !== "object" || s === null) continue;
      if (typeof s.uuid !== "string" || !UUID.test(s.uuid) || !shortText(s.name, 80) || !Array.isArray(s.styles)) continue;
      const styles: VoiceStyle[] = [];
      for (const rawStyle of s.styles.slice(0, 32)) {
        const st = rawStyle as Record<string, unknown> | null;
        if (typeof st !== "object" || st === null) continue;
        if (!shortText(st.name, 80) || !Number.isInteger(st.style_id)) continue;
        if ((st.style_id as number) < 0 || (st.style_id as number) > MAX_STYLE_ID) continue;
        styles.push({ name: st.name, style_id: st.style_id as number });
      }
      speakers.push({ uuid: s.uuid, name: s.name, styles });
    }
    models.push({
      uuid: m.uuid,
      name: m.name,
      description: m.description,
      category: m.category,
      voice_timbre: m.voice_timbre,
      licenses: (m.licenses as string[]).slice(0, 3),
      speakers,
    });
  }
  return { total: page.total as number, page: page.page as number, models };
}

/** Pages available for `total` results at the app's page size. */
export function searchPageCount(total: number): number {
  return Math.max(1, Math.ceil(total / SEARCH_PAGE_SIZE));
}

/** The credit line prefilled from a model name: `AivisSpeech: <name>`, bounded. */
export function creditFor(modelName: string): string {
  return [...`AivisSpeech: ${modelName.trim()}`].slice(0, MAX_CREDIT_CHARS).join("");
}

/** A credit line to send, or why it cannot be. Plain text, bounded. */
export function checkCredit(raw: string): { ok: true; text: string } | { ok: false; reason: string } {
  const text = raw.trim();
  if (text === "") return { ok: false, reason: "クレジット表記を入力してください(モデルのライセンスが求める表記です)" };
  if ([...text].length > MAX_CREDIT_CHARS) return { ok: false, reason: `クレジット表記は${MAX_CREDIT_CHARS}文字までです` };
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f-\u009f]/.test(text)) return { ok: false, reason: "クレジット表記に制御文字は使えません" };
  return { ok: true, text };
}

/** A key to send, or why it cannot be. It is never echoed back by the app. */
export function checkKey(raw: string): { ok: true; key: string } | { ok: false; reason: string } {
  const key = raw.trim();
  if (key === "") return { ok: false, reason: "APIキーが空です" };
  if ([...key].length > MAX_KEY_CHARS) return { ok: false, reason: "APIキーが長すぎます" };
  if (/[\s\u0000-\u001f\u007f-\u009f]/.test(key)) return { ok: false, reason: "APIキーに空白や制御文字は使えません" };
  return { ok: true, key };
}

/** A search keyword to send, or why it cannot be. Empty is allowed (all models). */
export function checkKeyword(raw: string): { ok: true; keyword: string } | { ok: false; reason: string } {
  const keyword = raw.trim();
  if ([...keyword].length > MAX_KEYWORD_CHARS) return { ok: false, reason: `検索語は${MAX_KEYWORD_CHARS}文字までです` };
  if (/[\u0000-\u001f\u007f-\u009f]/.test(keyword)) return { ok: false, reason: "検索語に制御文字は使えません" };
  return { ok: true, keyword };
}

export type TuningKey = keyof Tuning;

export const TUNING_RANGES: Record<TuningKey, readonly [number, number]> = {
  speaking_rate: SPEAKING_RATE_RANGE,
  pitch: PITCH_RANGE,
  volume: VOLUME_RANGE,
  emotional_intensity: EMOTIONAL_INTENSITY_RANGE,
};

/** A slider value kept inside the API's bounds, with the noise of a float slider rounded away. */
export function clampTuning(key: TuningKey, value: number): number {
  const [low, high] = TUNING_RANGES[key];
  if (!Number.isFinite(value)) return key === "pitch" ? 0 : 1;
  return Math.round(Math.min(high, Math.max(low, value)) * 100) / 100;
}

/** The patch that changes one tuning value (applies to the next utterance). */
export function tuningPatch(key: TuningKey, value: number): Record<string, number> {
  return { [key]: clampTuning(key, value) };
}

export interface VoiceChoice {
  model: VoiceModel;
  speakerUuid: string | null;
  styleId: number | null;
  credit: string;
  /** The operator confirmed that reply text is sent to Aivis Cloud. */
  confirmEgress: boolean;
}

/** The patch that switches to Aivis Cloud with a chosen model, speaker and style. */
export function enablePatch(choice: VoiceChoice): Record<string, unknown> {
  return {
    backend: "aivis",
    model_uuid: choice.model.uuid,
    model_name: [...choice.model.name].slice(0, 80).join(""),
    speaker_uuid: choice.speakerUuid,
    style_id: choice.styleId,
    credit: choice.credit,
    confirm_egress: choice.confirmEgress,
  };
}

/** The first speaker and its first style, the defaults for a model just picked. */
export function defaultSelection(model: VoiceModel): { speakerUuid: string | null; styleId: number | null } {
  const speaker = model.speakers[0];
  if (speaker === undefined) return { speakerUuid: null, styleId: null };
  return { speakerUuid: speaker.uuid, styleId: speaker.styles[0]?.style_id ?? null };
}

export interface VoiceView {
  /** One line: which voice speaks, with what. */
  summary: string;
  health: Health;
  keyText: string;
  keyHealth: Health;
  /** Aivis can only be switched on with a key. */
  canEnable: boolean;
  /** A change that is live-applied to the running Aivis voice. */
  live: boolean;
  error: string | null;
}

/** Japanese label of a key check outcome. */
export function keyCheckLabel(check: KeyCheck): string {
  switch (check) {
    case "valid":
      return "有効";
    case "rejected":
      return "拒否されました(キーが違うか、無効です)";
    case "network_error":
      return "確認できませんでした(ネットワークエラー)";
    case "unexpected":
      return "確認できませんでした(サービス側の応答が想定外です)";
    case "checking":
      return "確認中…";
    default:
      return "未確認";
  }
}

/** What the voice section shows, or `null` when the app has no voice settings. */
export function voiceView(state: PanelState | null): VoiceView | null {
  const v = state?.status.voice ?? null;
  if (v === null) return null;
  const name = v.model_name ?? v.model_uuid ?? "(未選択)";
  const style = v.style_id === null ? "" : ` / スタイル ${v.style_id}`;
  const keyText =
    !v.key.set
      ? "未設定"
      : `設定済み(${v.key.source === "environment" ? "環境変数 AIVIS_API" : "ファイル"}): ${keyCheckLabel(v.key.check)}`;
  const failure = v.last_error ?? v.save_error;
  return {
    summary:
      v.backend === "aivis"
        ? `使用中: Aivis Cloud — ${name}${style}(返信テキストはインターネット上のサービスに送信されます)`
        : "使用中: ローカルの声(テキストは外部に送信されません)",
    health: failure === null ? "ok" : "warn",
    keyText,
    keyHealth: v.key.set && (v.key.check === "rejected" || v.key.check === "network_error" || v.key.check === "unexpected") ? "warn" : "ok",
    canEnable: v.key.set,
    live: v.backend === "aivis",
    error:
      failure === null
        ? null
        : v.last_error !== null
          ? `最後の合成に失敗しました: ${v.last_error}(返答は定型の返しになります)`
          : `設定を保存できませんでした: ${v.save_error}`,
  };
}

// ---- Stream chat connections (#361) ----

export type ChatPlatform = "twitch" | "youtube";
export const CHAT_PLATFORMS: readonly ChatPlatform[] = ["twitch", "youtube"];
export type ChatConnectionState = "disconnected" | "connecting" | "connected" | "backoff" | "failed";
const CHAT_STATES: readonly string[] = ["disconnected", "connecting", "connected", "backoff", "failed"];
/** Longest text the page sends as a target (the app refuses more). */
export const MAX_CHAT_TARGET_CHARS = 256;

export interface ChatPlatformState {
  state: ChatConnectionState;
  target: string | null;
  saved_target: string | null;
  sidecar_found: boolean;
  messages_accepted: number;
  messages_dropped: number;
  dropped_queue_full: number;
  dropped_refused: number;
  messages_per_minute: number;
  restarts: number;
  last_error: string | null;
}

export interface ChatIngressState {
  twitch: ChatPlatformState;
  youtube: ChatPlatformState;
  youtube_key: { set: boolean; source: "environment" | "file" | null };
  save_error: string | null;
}

const count = (value: unknown): value is number =>
  typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= Number.MAX_SAFE_INTEGER;

const boundedText = (value: unknown, max: number): string | null | undefined => {
  if (value === null) return null;
  return typeof value === "string" && [...value].length <= max ? value : undefined;
};

function parseChatPlatform(value: unknown): ChatPlatformState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.state !== "string" || !CHAT_STATES.includes(v.state)) return null;
  const target = boundedText(v.target, MAX_CHAT_TARGET_CHARS);
  const savedTarget = boundedText(v.saved_target, MAX_CHAT_TARGET_CHARS);
  const lastError = boundedText(v.last_error, 400);
  if (target === undefined || savedTarget === undefined || lastError === undefined) return null;
  if (typeof v.sidecar_found !== "boolean") return null;
  for (const key of [
    "messages_accepted",
    "messages_dropped",
    "dropped_queue_full",
    "dropped_refused",
    "messages_per_minute",
    "restarts",
  ]) {
    if (!count(v[key])) return null;
  }
  return {
    state: v.state as ChatConnectionState,
    target,
    saved_target: savedTarget,
    sidecar_found: v.sidecar_found,
    messages_accepted: v.messages_accepted as number,
    messages_dropped: v.messages_dropped as number,
    dropped_queue_full: v.dropped_queue_full as number,
    dropped_refused: v.dropped_refused as number,
    messages_per_minute: v.messages_per_minute as number,
    restarts: v.restarts as number,
    last_error: lastError,
  };
}

/**
 * The chat connection state, only if it has exactly the documented shape;
 * anything else hides the card rather than offering what the app would refuse.
 */
export function parseChatIngressState(value: unknown): ChatIngressState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  const twitch = parseChatPlatform(v.twitch);
  const youtube = parseChatPlatform(v.youtube);
  if (twitch === null || youtube === null) return null;
  const key = v.youtube_key as Record<string, unknown> | null;
  if (typeof key !== "object" || key === null || typeof key.set !== "boolean") return null;
  if (key.source !== null && key.source !== "environment" && key.source !== "file") return null;
  const saveError = boundedText(v.save_error, 400);
  if (saveError === undefined) return null;
  return { twitch, youtube, youtube_key: { set: key.set, source: key.source }, save_error: saveError };
}

/** Mirrors `parse_twitch_login` (crates/adapters/src/chat_target.rs). */
export function checkTwitchChannel(raw: string): { ok: true; target: string } | { ok: false; reason: string } {
  const target = raw.trim().toLowerCase();
  if (!/^[a-z0-9_]{3,25}$/.test(target)) {
    return { ok: false, reason: "Twitchのチャンネル名は、英小文字・数字・アンダースコアの3〜25文字です(# や URL は不要です)" };
  }
  return { ok: true, target };
}

const VIDEO_ID = /^[A-Za-z0-9_-]{11}$/;

/** Mirrors `parse_youtube_video`: an 11-character id or a watch/live/youtu.be link. */
export function checkYoutubeVideo(raw: string): { ok: true; target: string } | { ok: false; reason: string } {
  const refused = { ok: false as const, reason: "動画IDか、youtube.com の視聴・ライブ配信URL、youtu.be の短縮URLを入力してください" };
  const text = raw.trim();
  if ([...text].length > MAX_CHAT_TARGET_CHARS) return { ok: false, reason: "長すぎます" };
  if (VIDEO_ID.test(text)) return { ok: true, target: text };
  let url: URL;
  try {
    url = new URL(text);
  } catch {
    return refused;
  }
  if ((url.protocol !== "https:" && url.protocol !== "http:") || url.username !== "" || url.password !== "" || url.port !== "") {
    return refused;
  }
  const segments = url.pathname.split("/").slice(1);
  let id: string | null = null;
  if (["youtube.com", "www.youtube.com", "m.youtube.com"].includes(url.hostname)) {
    if (segments[0] === "watch" && (segments.length === 1 || (segments.length === 2 && segments[1] === ""))) {
      id = url.searchParams.get("v");
    } else if (segments[0] === "live" && segments.length >= 2) {
      id = segments[1] ?? null;
    }
  } else if (url.hostname === "youtu.be") {
    id = segments[0] ?? null;
  }
  return id !== null && VIDEO_ID.test(id) ? { ok: true, target: id } : refused;
}

/** The body of `POST /api/ingress/connect`, or why the input cannot be sent. */
export function connectBody(
  platform: ChatPlatform,
  raw: string,
): { ok: true; body: { platform: ChatPlatform; target: string } } | { ok: false; reason: string } {
  const checked = platform === "twitch" ? checkTwitchChannel(raw) : checkYoutubeVideo(raw);
  return checked.ok ? { ok: true, body: { platform, target: checked.target } } : checked;
}

export interface ChatPlatformView {
  label: string;
  stateText: string;
  health: Health;
  /** Target and counters on one line, or empty when there is nothing to show. */
  detail: string;
  error: string | null;
  /** Value to put in an empty input (the last choice; never connected by itself). */
  prefill: string;
  canConnect: boolean;
  canDisconnect: boolean;
  /** Reason the connect button is off, when it is not the connection state. */
  blocked: string | null;
}

export interface ChatIngressView {
  twitch: ChatPlatformView;
  youtube: ChatPlatformView;
  keyText: string;
  keyHealth: Health;
  saveError: string | null;
}

const STATE_TEXT: Record<ChatConnectionState, string> = {
  disconnected: "未接続",
  connecting: "接続中…",
  connected: "接続済み",
  backoff: "再接続待ち",
  failed: "失敗",
};

function platformView(platform: ChatPlatform, s: ChatPlatformState, keySet: boolean): ChatPlatformView {
  const label = platform === "twitch" ? "Twitch" : "YouTube";
  const active = s.state === "connecting" || s.state === "connected" || s.state === "backoff";
  const shown = s.target === null ? "" : platform === "twitch" ? `#${s.target}` : s.target;
  const counters =
    s.state === "disconnected" && s.messages_accepted === 0 && s.messages_dropped === 0
      ? ""
      : `受理 ${s.messages_accepted} 件 / 破棄 ${s.messages_dropped} 件 / 直近1分 ${s.messages_per_minute} 件`;
  const detail = [shown, counters].filter((t) => t !== "").join(" — ");
  let blocked: string | null = null;
  if (!s.sidecar_found) {
    blocked = `${platform === "twitch" ? "twitch" : "youtube"}-chat-ingress がアプリと同じフォルダにありません`;
  } else if (platform === "youtube" && !keySet) {
    blocked = "先にYouTube Data APIキーを保存してください";
  }
  return {
    label,
    stateText: STATE_TEXT[s.state] + (s.restarts > 0 ? `(再起動 ${s.restarts} 回)` : ""),
    health: s.state === "failed" || s.state === "backoff" ? "warn" : "ok",
    detail,
    error: s.last_error,
    prefill: s.saved_target ?? "",
    canConnect: blocked === null,
    canDisconnect: active || s.state === "failed",
    blocked,
  };
}

/** What the chat connection card shows, or `null` when the app has none. */
export function chatIngressView(state: PanelState | null): ChatIngressView | null {
  const c = state?.status.chat_ingress ?? null;
  if (c === null) return null;
  const key = c.youtube_key;
  return {
    twitch: platformView("twitch", c.twitch, key.set),
    youtube: platformView("youtube", c.youtube, key.set),
    keyText: key.set ? `設定済み(${key.source === "environment" ? "環境変数 YOUTUBE_API_KEY" : "ファイル"})` : "未設定",
    keyHealth: "ok",
    saveError: c.save_error,
  };
}

// ---- VRM avatar choice (#362) ----

/** Mirrors `MAX_VRM_BYTES` in crates/adapters/src/local_avatar/vrm.rs. */
export const MAX_VRM_BYTES = 64 * 1024 * 1024;
/** Mirrors `MAX_STORED_MODELS` (crates/app/src/avatar_control.rs). */
export const MAX_AVATAR_MODELS = 5;
const MAX_META_TEXT = 300;
const MAX_META_AUTHORS = 8;
const HASH64 = /^[0-9a-f]{64}$/;

export type AvatarWarning =
  | "author_only"
  | "licensed_persons_only"
  | "non_commercial_only"
  | "personal_commercial_only"
  | "credit_required"
  | "licence_not_stated";
const AVATAR_WARNINGS: readonly string[] = [
  "author_only",
  "licensed_persons_only",
  "non_commercial_only",
  "personal_commercial_only",
  "credit_required",
  "licence_not_stated",
];

/** The licence summary the app read from the model file. All text is untrusted. */
export interface AvatarMeta {
  spec: "vrm1" | "vrm0";
  name: string | null;
  version: string | null;
  authors: string[];
  copyright: string | null;
  contact: string | null;
  license_url: string | null;
  other_license_url: string | null;
  license_name: string | null;
  avatar_permission: string | null;
  commercial_usage: string | null;
  allow_redistribution: boolean | null;
  modification: string | null;
  credit_notation: string | null;
  allow_violent_usage: boolean | null;
  allow_sexual_usage: boolean | null;
  allow_political_or_religious_usage: boolean | null;
  allow_antisocial_or_hate_usage: boolean | null;
  warnings: AvatarWarning[];
  credit: string | null;
}

export interface AvatarModel {
  id: string;
  size: number;
  name: string | null;
  warnings: AvatarWarning[];
  meta: AvatarMeta | null;
}

export interface AvatarState {
  active: string;
  active_id: string | null;
  credit: string | null;
  sample: AvatarModel | null;
  models: AvatarModel[];
  pending: AvatarModel | null;
  max_models: number;
  max_upload_bytes: number;
  renderer_model: string | null;
  last_error: string | null;
  save_error: string | null;
}

const optText = (value: unknown, max = MAX_META_TEXT): string | null | undefined =>
  boundedText(value, max);

const optBool = (value: unknown): boolean | null | undefined =>
  value === null || typeof value === "boolean" ? value : undefined;

function parseWarnings(value: unknown): AvatarWarning[] | null {
  if (!Array.isArray(value) || value.length > AVATAR_WARNINGS.length) return null;
  if (!value.every((w) => typeof w === "string" && AVATAR_WARNINGS.includes(w))) return null;
  return value as AvatarWarning[];
}

function parseAvatarMeta(value: unknown): AvatarMeta | null | undefined {
  if (value === null) return null;
  if (typeof value !== "object") return undefined;
  const v = value as Record<string, unknown>;
  if (v.spec !== "vrm1" && v.spec !== "vrm0") return undefined;
  const authors = v.authors;
  if (!Array.isArray(authors) || authors.length > MAX_META_AUTHORS) return undefined;
  if (!authors.every((a) => typeof a === "string" && [...a].length <= MAX_META_TEXT)) return undefined;
  const warnings = parseWarnings(v.warnings);
  if (warnings === null) return undefined;
  const texts: Record<string, string | null> = {};
  for (const key of [
    "name",
    "version",
    "copyright",
    "contact",
    "license_url",
    "other_license_url",
    "license_name",
    "avatar_permission",
    "commercial_usage",
    "modification",
    "credit_notation",
    "credit",
  ]) {
    const text = optText(v[key]);
    if (text === undefined) return undefined;
    texts[key] = text;
  }
  const bools: Record<string, boolean | null> = {};
  for (const key of [
    "allow_redistribution",
    "allow_violent_usage",
    "allow_sexual_usage",
    "allow_political_or_religious_usage",
    "allow_antisocial_or_hate_usage",
  ]) {
    const b = optBool(v[key]);
    if (b === undefined) return undefined;
    bools[key] = b;
  }
  return {
    spec: v.spec,
    authors: authors as string[],
    warnings,
    name: texts.name ?? null,
    version: texts.version ?? null,
    copyright: texts.copyright ?? null,
    contact: texts.contact ?? null,
    license_url: texts.license_url ?? null,
    other_license_url: texts.other_license_url ?? null,
    license_name: texts.license_name ?? null,
    avatar_permission: texts.avatar_permission ?? null,
    commercial_usage: texts.commercial_usage ?? null,
    modification: texts.modification ?? null,
    credit_notation: texts.credit_notation ?? null,
    credit: texts.credit ?? null,
    allow_redistribution: bools.allow_redistribution ?? null,
    allow_violent_usage: bools.allow_violent_usage ?? null,
    allow_sexual_usage: bools.allow_sexual_usage ?? null,
    allow_political_or_religious_usage: bools.allow_political_or_religious_usage ?? null,
    allow_antisocial_or_hate_usage: bools.allow_antisocial_or_hate_usage ?? null,
  };
}

function parseAvatarModel(value: unknown): AvatarModel | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.id !== "string" || !HASH64.test(v.id)) return null;
  if (!count(v.size) || v.size > MAX_VRM_BYTES) return null;
  const name = optText(v.name, 200);
  const warnings = parseWarnings(v.warnings);
  const meta = parseAvatarMeta(v.meta);
  if (name === undefined || warnings === null || meta === undefined) return null;
  return { id: v.id, size: v.size, name, warnings, meta };
}

/**
 * The avatar state, only if it has exactly the documented shape; anything else
 * hides the card rather than offering what the app would refuse.
 */
export function parseAvatarState(value: unknown): AvatarState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.active !== "string" || (v.active !== "sample" && !HASH64.test(v.active))) return null;
  for (const key of ["active_id", "renderer_model"]) {
    const id = v[key];
    if (id !== null && (typeof id !== "string" || !HASH64.test(id))) return null;
  }
  const credit = optText(v.credit, 200);
  const lastError = optText(v.last_error, 400);
  const saveError = optText(v.save_error, 400);
  if (credit === undefined || lastError === undefined || saveError === undefined) return null;
  if (!count(v.max_models) || v.max_models > 64 || !count(v.max_upload_bytes)) return null;
  let sample: AvatarModel | null = null;
  if (v.sample !== null) {
    sample = parseAvatarModel(v.sample);
    if (sample === null) return null;
  }
  let pending: AvatarModel | null = null;
  if (v.pending !== null) {
    pending = parseAvatarModel(v.pending);
    if (pending === null) return null;
  }
  if (!Array.isArray(v.models) || v.models.length > 64) return null;
  const models: AvatarModel[] = [];
  for (const entry of v.models) {
    const model = parseAvatarModel(entry);
    if (model === null || models.some((m) => m.id === model.id)) return null;
    models.push(model);
  }
  return {
    active: v.active,
    active_id: (v.active_id as string | null) ?? null,
    credit,
    sample,
    models,
    pending,
    max_models: v.max_models,
    max_upload_bytes: v.max_upload_bytes,
    renderer_model: (v.renderer_model as string | null) ?? null,
    last_error: lastError,
    save_error: saveError,
  };
}

/** Why a file cannot be sent, or `ok`. The app checks everything again. */
export function checkVrmFile(
  file: { name: string; size: number },
  head: Uint8Array,
): { ok: true } | { ok: false; reason: string } {
  if (file.size === 0) return { ok: false, reason: "空のファイルです" };
  if (file.size > MAX_VRM_BYTES) {
    return { ok: false, reason: `ファイルが大きすぎます(上限 ${MAX_VRM_BYTES / (1024 * 1024)} MiB)` };
  }
  // glTF binary starts with the four bytes "glTF"; a renamed image or archive
  // does not. The app inspects the whole file; this only saves the upload.
  const magic = [0x67, 0x6c, 0x54, 0x46];
  if (head.length < 4 || !magic.every((b, i) => head[i] === b)) {
    return { ok: false, reason: "VRMファイル(glTFバイナリ)ではありません。拡張子だけ .vrm に変えたファイルは使えません" };
  }
  return { ok: true };
}

/** Warning text, in the order the app reports them. */
export const AVATAR_WARNING_TEXT: Record<AvatarWarning, string> = {
  author_only: "このモデルは作者本人だけが使えます。配信に使うには作者の許可が必要です",
  licensed_persons_only: "このモデルは作者が個別に許可した人だけが使えます。許可を受けているか確認してください",
  non_commercial_only: "営利目的の利用が許可されていません。収益化している配信では使えない可能性があります",
  personal_commercial_only: "営利利用は個人に限られます。法人や事務所の配信では使えない可能性があります",
  credit_required: "クレジット表記が必要です。配信の概要欄などに表示してください",
  licence_not_stated: "利用者や営利利用の条件が書かれていません。配布元の規約を確認してください",
};

/** Warnings that say the use may not be allowed (as opposed to a duty to credit). */
const RESTRICTIVE: readonly AvatarWarning[] = [
  "author_only",
  "licensed_persons_only",
  "non_commercial_only",
  "personal_commercial_only",
  "licence_not_stated",
];

export function isRestrictive(warnings: readonly AvatarWarning[]): boolean {
  return warnings.some((w) => RESTRICTIVE.includes(w));
}

const LABELS: Record<string, Record<string, string>> = {
  avatar_permission: {
    onlyAuthor: "作者のみ",
    OnlyAuthor: "作者のみ",
    onlySeparatelyLicensedPerson: "作者が個別に許可した人のみ",
    ExplicitlyLicensedPerson: "作者が個別に許可した人のみ",
    everyone: "全員",
    Everyone: "全員",
  },
  commercial_usage: {
    personalNonProfit: "個人・非営利のみ",
    personalProfit: "個人なら営利利用可",
    corporation: "法人も営利利用可",
    Disallow: "営利利用不可",
    Allow: "営利利用可",
  },
  modification: {
    prohibited: "改変不可",
    allowModification: "改変可(再配布は不可)",
    allowModificationRedistribution: "改変・再配布可",
  },
  credit_notation: { required: "必要", unnecessary: "不要" },
};

/** The Japanese label of a known value, else the file's own text. */
export function valueLabel(field: keyof typeof LABELS, raw: string | null): string {
  if (raw === null) return "(記載なし)";
  const label = LABELS[field]?.[raw];
  return label === undefined ? raw : `${label}(${raw})`;
}

const yesNo = (value: boolean | null): string => (value === null ? "(記載なし)" : value ? "許可" : "不許可");

/** The licence rows to show, label then value. Every value is shown as text. */
export function licenceRows(meta: AvatarMeta): [string, string][] {
  const rows: [string, string][] = [
    ["名前", meta.name ?? "(記載なし)"],
    ["規格", meta.spec === "vrm1" ? "VRM 1.0" : "VRM 0.x"],
    ["バージョン", meta.version ?? "(記載なし)"],
    ["作者", meta.authors.length === 0 ? "(記載なし)" : meta.authors.join("、")],
    ["アバターを使える人", valueLabel("avatar_permission", meta.avatar_permission)],
    ["営利利用", valueLabel("commercial_usage", meta.commercial_usage)],
    ["クレジット表記", valueLabel("credit_notation", meta.credit_notation)],
  ];
  if (meta.license_name !== null) rows.push(["ライセンス名", meta.license_name]);
  rows.push(
    ["改変", valueLabel("modification", meta.modification)],
    ["再配布", yesNo(meta.allow_redistribution)],
    ["過度に暴力的な表現", yesNo(meta.allow_violent_usage)],
    ["過度に性的な表現", yesNo(meta.allow_sexual_usage)],
    ["政治・宗教目的", yesNo(meta.allow_political_or_religious_usage)],
    ["反社会的・ヘイト目的", yesNo(meta.allow_antisocial_or_hate_usage)],
    ["ライセンスURL", meta.license_url ?? "(記載なし)"],
  );
  if (meta.other_license_url !== null) rows.push(["その他のライセンスURL", meta.other_license_url]);
  if (meta.copyright !== null) rows.push(["著作権表示", meta.copyright]);
  if (meta.contact !== null) rows.push(["連絡先", meta.contact]);
  return rows;
}

export interface AvatarChoice {
  /** `sample` or the model hash: what the select/delete request carries. */
  id: string;
  label: string;
  active: boolean;
  deletable: boolean;
  warnings: string[];
}

export interface AvatarPendingView {
  id: string;
  title: string;
  rows: [string, string][];
  warnings: string[];
  /** The licence says the use may not be allowed: confirm needs a second look. */
  restrictive: boolean;
  credit: string | null;
  /** The text of the confirmation question when the licence is restrictive. */
  question: string;
}

export interface AvatarView {
  summary: string;
  health: Health;
  credit: string | null;
  choices: AvatarChoice[];
  pending: AvatarPendingView | null;
  activeRows: [string, string][];
  activeWarnings: string[];
  error: string | null;
  /** The connected avatar page shows another model than the active one. */
  mismatch: boolean;
  canAddMore: boolean;
}

export function formatMiB(bytes: number): string {
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

function modelLabel(model: AvatarModel, fallback: string): string {
  return `${model.name ?? fallback}(${model.id.slice(0, 8)}, ${formatMiB(model.size)})`;
}

/** What the avatar card shows, or `null` when the app has no avatar store. */
export function avatarView(state: PanelState | null): AvatarView | null {
  const a = state?.status.avatar_model ?? null;
  if (a === null) return null;
  const choices: AvatarChoice[] = [];
  if (a.sample !== null) {
    choices.push({
      id: "sample",
      label: `サンプル — ${modelLabel(a.sample, "(名前なし)")}`,
      active: a.active === "sample",
      deletable: false,
      warnings: a.sample.warnings.map((w) => AVATAR_WARNING_TEXT[w]),
    });
  }
  for (const m of a.models) {
    choices.push({
      id: m.id,
      label: modelLabel(m, "(名前なし)"),
      active: a.active === m.id,
      deletable: a.active !== m.id,
      warnings: m.warnings.map((w) => AVATAR_WARNING_TEXT[w]),
    });
  }
  const activeModel = a.active === "sample" ? a.sample : (a.models.find((m) => m.id === a.active) ?? null);
  const pending: AvatarPendingView | null =
    a.pending === null
      ? null
      : {
          id: a.pending.id,
          title: a.pending.name ?? "(名前なし)",
          rows: a.pending.meta === null ? [] : licenceRows(a.pending.meta),
          warnings: a.pending.warnings.map((w) => AVATAR_WARNING_TEXT[w]),
          restrictive: isRestrictive(a.pending.warnings),
          credit: a.pending.meta?.credit ?? null,
          question:
            "このモデルのライセンスには、配信で使えない可能性がある条件があります(上の警告を確認してください)。それでも使いますか?(最終的な判断はあなたに任されています)",
        };
  const failure = a.last_error ?? a.save_error;
  const mismatch = a.renderer_model !== null && a.active_id !== null && a.renderer_model !== a.active_id;
  return {
    summary:
      activeModel === null
        ? "使用中: (モデルがありません。VRMファイルを追加してください)"
        : `使用中: ${a.active === "sample" ? "サンプル — " : ""}${activeModel.name ?? "(名前なし)"}`,
    health: failure === null && !isRestrictive(activeModel?.warnings ?? []) ? "ok" : "warn",
    credit: a.credit,
    choices,
    pending,
    activeRows: activeModel?.meta == null ? [] : licenceRows(activeModel.meta),
    activeWarnings: (activeModel?.warnings ?? []).map((w) => AVATAR_WARNING_TEXT[w]),
    error:
      failure === null
        ? null
        : a.last_error !== null
          ? a.last_error
          : `選択を保存できませんでした: ${a.save_error}`,
    mismatch,
    canAddMore: true,
  };
}
