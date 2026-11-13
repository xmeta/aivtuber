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
  return { ...(v as PanelState), status: { ...v.status, thinking_model: thinkingModel, voice } };
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
