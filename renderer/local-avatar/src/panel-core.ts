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
    /** The performer is speaking now (#440); absent from an older host. Display only. */
    speaking?: boolean;
    /** The local Thinking model selector (#357); absent unless the app composed it. */
    thinking_model?: ThinkingModelState | null;
    /** The voice settings (#360); absent unless the app composed them. */
    voice?: VoiceState | null;
    /** The stream-chat connections (#361); absent unless the app composed them. */
    chat_ingress?: ChatIngressState | null;
    /** The VRM avatar choice (#362); absent unless the app composed it. */
    avatar_model?: AvatarState | null;
    /** Voice input (#400); absent from an older host, or when no speech sidecar is configured. */
    voice_input?: VoiceInputState | null;
    /** Live character-sheet editing (#401); absent from an older host, or when the app did not compose it. */
    persona_editor?: PersonaEditorState | null;
    /** Live memory-seed editing (#402); absent from an older host, or when the app did not compose it. */
    memory_seed_editor?: MemorySeedEditorState | null;
    /** The idle initiative (#87): the switch and its text-free counters; absent unless the app composed it. */
    initiative?: InitiativeState | null;
    /** The opt-in session conversation log (#463): the switch, where it writes and counters; absent unless composed. */
    session_log?: SessionLogState | null;
    /** Session Thinking tokens/cost and the optional spend cap (#367); absent unless composed. */
    thinking_usage?: ThinkingUsageState | null;
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

/** Mirrors `ThinkingUsageStatus` in crates/app/src/thinking_meter.rs. */
export interface ThinkingUsageState {
  prompt_tokens: number;
  completion_tokens: number;
  /** Summed micro-USD, or null when no reply has reported a cost (unknown, not 0). */
  cost_micro_usd: number | null;
  turns_with_usage: number;
  turns_unknown_usage: number;
  turns_cost_reported: number;
  cap_micro_usd: number | null;
  cap_tripped: boolean;
  suspended_calls: number;
}

export const STATE_SCHEMA = "local-panel/1";
/** Mirrors `MAX_THINKING_MODEL_CHOICES` and the alias shape in crates/domain. */
export const MAX_MODEL_CHOICES = 8;
const MODEL_ALIAS_CHARS = 96;
// `alias`, `vendor/model` or either with `:variant` (issue #365); see
// `is_valid_model_alias` in crates/domain.
const MODEL_ALIAS_RE = /^[a-z0-9][a-z0-9._-]*(\/[a-z0-9][a-z0-9._-]*)?(:[a-z0-9][a-z0-9._-]*)?$/;
const MODEL_ALIAS = { test: (alias: string) => alias.length <= MODEL_ALIAS_CHARS && MODEL_ALIAS_RE.test(alias) };
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
 * Whether a fragment change should reload the page: only a well-formed token
 * fragment. The page scrubs its own fragment with replaceState (no hashchange),
 * and an empty or malformed fragment never reloads, so there is no loop.
 */
export function shouldReloadForHash(hash: string): boolean {
  return tokenFromHash(hash) !== null;
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

const isCount = (value: unknown): value is number => typeof value === "number" && Number.isInteger(value) && value >= 0;

/** The usage state, only if it has exactly the documented shape; otherwise the section is hidden. */
export function parseUsageState(value: unknown): ThinkingUsageState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  const optionalCount = (x: unknown): number | null | undefined => (x === null ? null : isCount(x) ? x : undefined);
  const cost = optionalCount(v.cost_micro_usd);
  const cap = optionalCount(v.cap_micro_usd);
  if (cost === undefined || cap === undefined) return null;
  if (
    !isCount(v.prompt_tokens) ||
    !isCount(v.completion_tokens) ||
    !isCount(v.turns_with_usage) ||
    !isCount(v.turns_unknown_usage) ||
    !isCount(v.turns_cost_reported) ||
    !isCount(v.suspended_calls) ||
    typeof v.cap_tripped !== "boolean"
  ) {
    return null;
  }
  return {
    prompt_tokens: v.prompt_tokens,
    completion_tokens: v.completion_tokens,
    cost_micro_usd: cost,
    turns_with_usage: v.turns_with_usage,
    turns_unknown_usage: v.turns_unknown_usage,
    turns_cost_reported: v.turns_cost_reported,
    cap_micro_usd: cap,
    cap_tripped: v.cap_tripped,
    suspended_calls: v.suspended_calls,
  };
}

/** Micro-USD as `$X.XXXX`. */
export function formatUsd(micro: number): string {
  return `$${(micro / 1_000_000).toFixed(4)}`;
}

export interface UsageView {
  /** "今回の配信: N トークン · $X.XXXX", with "不明" where the provider reported nothing. */
  summary: string;
  /** The spend cap and whether it stopped Thinking; empty when there is no cap. */
  cap: string;
  health: Health;
}

/** What the usage card shows, or `null` when the app has no meter. */
export function usageView(state: PanelState | null): UsageView | null {
  const u = state?.status.thinking_usage ?? null;
  if (u === null) return null;
  const replies = u.turns_with_usage + u.turns_unknown_usage;
  let summary: string;
  if (u.turns_with_usage === 0) {
    summary = replies === 0 ? "今回の配信: まだ応答がありません" : "今回の配信: 不明(プロバイダーが使用量を返していません)";
  } else {
    const tokens = `${(u.prompt_tokens + u.completion_tokens).toLocaleString("en-US")} トークン`;
    if (u.cost_micro_usd === null) {
      summary = `今回の配信: ${tokens} · 金額は不明`;
    } else {
      const unknown = replies - u.turns_cost_reported;
      summary =
        unknown > 0
          ? `今回の配信: ${tokens} · ${formatUsd(u.cost_micro_usd)} 以上(${unknown}回分は金額不明)`
          : `今回の配信: ${tokens} · ${formatUsd(u.cost_micro_usd)}`;
    }
  }
  if (u.cap_micro_usd === null) return { summary, cap: "", health: "ok" };
  const limit = formatUsd(u.cap_micro_usd);
  if (u.cap_tripped) {
    return {
      summary,
      cap: `上限 ${limit} に達したため、応答の生成を停止しています。以降は定型の返しになります(再起動で解除)`,
      health: "warn",
    };
  }
  return { summary, cap: `上限 ${limit}(金額が報告された分だけ数えます。超えると応答の生成を停止します)`, health: "ok" };
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
  const voiceInput = parseVoiceInputState(v.status.voice_input);
  const personaEditor = parsePersonaEditorState(v.status.persona_editor);
  const memorySeedEditor = parseMemorySeedEditorState(v.status.memory_seed_editor);
  const thinkingUsage = parseUsageState(v.status.thinking_usage);
  const initiative = parseInitiativeState(v.status.initiative);
  const sessionLog = parseSessionLogState(v.status.session_log);
  return {
    ...(v as PanelState),
    status: {
      ...v.status,
      speaking: typeof v.status.speaking === "boolean" ? v.status.speaking : undefined,
      thinking_model: thinkingModel,
      voice,
      chat_ingress: chatIngress,
      avatar_model: avatarModel,
      voice_input: voiceInput,
      persona_editor: personaEditor,
      memory_seed_editor: memorySeedEditor,
      thinking_usage: thinkingUsage,
      initiative,
      session_log: sessionLog,
    },
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
  /** Stable id of a row the page updates between full renders. */
  key?: string;
  label: string;
  value: string;
  health: Health;
  /** Raw host text behind a translated value, for support (tooltip). */
  raw?: string;
}

/** The status-card text while the performer is speaking (#440). */
export const SPEAKING_TEXT = "話しています";

/** The rows the status card shows, in order, with Japanese labels. */
export function statusRows(state: PanelState): StatusRow[] {
  const s = state.status;
  const err = (e: string | null): StatusRow["value"] => (e === null ? "正常" : reasonText(e));
  const raw = (e: string | null): { raw?: string } => (e === null ? {} : { raw: e.slice(0, MAX_HOST_TEXT_CHARS) });
  return [
    { label: "ミュート", value: s.muted ? "ミュート中(再起動まで解除不可)" : "いいえ", health: s.muted ? "warn" : "ok" },
    { label: "音声出力", value: err(s.audio_error), health: s.audio_error === null ? "ok" : "warn", ...raw(s.audio_error) },
    { label: "アバター", value: err(s.avatar_error), health: s.avatar_error === null ? "ok" : "warn", ...raw(s.avatar_error) },
    { label: "配信連携", value: err(s.stream_error), health: s.stream_error === null ? "ok" : "warn", ...raw(s.stream_error) },
    { label: "応答生成", value: s.generation_active ? "生成中" : "待機中", health: "ok" },
    ...(s.speaking === undefined ? [] : [{ key: "speaking", label: "アバターの声", value: s.speaking ? SPEAKING_TEXT : "話していません", health: "ok" as Health }]),
    { label: "入力待ち", value: String(s.content_queue), health: "ok" },
    { label: "再生予定", value: String(s.scheduler_items), health: "ok" },
  ];
}

export interface ChatEntry {
  /** Event id the runtime gave the line (`panel-chat-N`), once accepted. */
  id: string | null;
  text: string;
  /** Page-local send time (epoch ms), display only; never sent to the host. */
  sentAt: number;
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

/** Replies that answer a panel line, with their seq (for the first-seen time). */
export function replyRecordsFor(state: PanelState | null, id: string | null): { seq: number; text: string }[] {
  if (state === null || id === null) return [];
  return state.replies.filter((r) => r.event_id === id).map(({ seq, text }) => ({ seq, text }));
}

/** Reply first-seen times kept in the page; bounded like the chat log. */
export const MAX_SEEN_REPLIES = 200;

/** When this page first saw reply `seq` (stable across re-renders/polls). Display only. */
export function firstSeen(seen: Map<number, number>, seq: number, now: number): number {
  const known = seen.get(seq);
  if (known !== undefined) return known;
  seen.set(seq, now);
  if (seen.size > MAX_SEEN_REPLIES) seen.delete(seen.keys().next().value as number);
  return now;
}

/** Local HH:MM of an epoch-ms instant. */
export function formatClock(ms: number): string {
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

/** Speaker label and time shown above a bubble. */
export function bubbleMeta(who: "operator" | "ai", ms: number): { label: string; time: string } {
  return { label: who === "operator" ? "オペレーター" : "AI", time: formatClock(ms) };
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

// ---- voice input (#400) ----

/** Mirrors `MAX_SPEECH_INPUT_REASON_CHARS` (crates/app/src/speech_input.rs). */
export const MAX_VOICE_INPUT_REASON_CHARS = 400;
/** Mirrors `MAX_SPEECH_DEVICES` and `MAX_SPEECH_DEVICE_NAME_BYTES` (crates/domain/src/speech_input.rs). */
export const MAX_VOICE_INPUT_DEVICES = 32;
export const MAX_VOICE_INPUT_DEVICE_NAME_BYTES = 128;
export type VoiceInputDuplex = "half" | "full";
export type VoiceInputStateName = "off" | "on" | "failed";
const VOICE_INPUT_STATES: readonly string[] = ["off", "on", "failed"];

/** Mirrors `SpeechInputStatus` (crates/app/src/speech_input.rs). */
export interface VoiceInputState {
  /** Whether the launcher composed live switching; false hides the toggle. */
  composed: boolean;
  state: VoiceInputStateName;
  /** Why it failed (host text, shown with textContent only). */
  reason: string | null;
  /** The Thinking endpoint leaves this machine: transcribed speech goes to a cloud model. */
  thinking_remote: boolean;
  /** The capture device the running microphone opened (#396), if known. Host text. */
  device: string | null;
  /** The remembered choice; null is the system default. */
  selected_device: string | null;
  /** Capture devices the microphone reported at its last start. */
  devices: string[];
  /** half: speech during the performer's playback is ignored; full: headset. */
  duplex: VoiceInputDuplex;
  /** Segments dropped for starting during playback (half duplex). */
  dropped_during_playback: number;
  /** Microphone input level 0..100 (#440); null is no signal (not running, or the host heard nothing for a second). */
  level: number | null;
  /** Highest speech probability 0..100 in the same interval; null with `level`. */
  speech_prob: number | null;
}

const deviceName = (value: unknown): value is string =>
  typeof value === "string" &&
  value.length > 0 &&
  new TextEncoder().encode(value).length <= MAX_VOICE_INPUT_DEVICE_NAME_BYTES;

/** A 0..100 integer, else null: meter data is display only, so a bad value is no signal, not a reason to hide the card. */
const percent = (value: unknown): number | null =>
  typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= 100 ? value : null;

/**
 * The voice-input state, only if it has exactly the documented shape. An older
 * host never sends it, and anything malformed is dropped: either way the
 * toggle is hidden rather than offering what the app would refuse.
 */
export function parseVoiceInputState(value: unknown): VoiceInputState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.composed !== "boolean" || typeof v.thinking_remote !== "boolean") return null;
  if (typeof v.state !== "string" || !VOICE_INPUT_STATES.includes(v.state)) return null;
  // The host omits `reason` unless failed; null and absent both mean none.
  const reason = v.reason === undefined ? null : boundedText(v.reason, MAX_VOICE_INPUT_REASON_CHARS);
  if (reason === undefined) return null;
  // The device fields are optional (an older host sends none): absent means
  // unknown / system default / full duplex, as the host's own defaults do.
  const device = v.device === undefined ? null : v.device;
  const selected = v.selected_device === undefined ? null : v.selected_device;
  if ((device !== null && !deviceName(device)) || (selected !== null && !deviceName(selected))) return null;
  const devices = v.devices === undefined ? [] : v.devices;
  if (!Array.isArray(devices) || devices.length > MAX_VOICE_INPUT_DEVICES || !devices.every(deviceName)) return null;
  const duplex = v.duplex === undefined ? "full" : v.duplex;
  if (duplex !== "half" && duplex !== "full") return null;
  const dropped = v.dropped_during_playback === undefined ? 0 : v.dropped_during_playback;
  if (!count(dropped)) return null;
  return {
    composed: v.composed,
    state: v.state as VoiceInputStateName,
    reason,
    thinking_remote: v.thinking_remote,
    device,
    selected_device: selected,
    devices: [...devices],
    duplex,
    dropped_during_playback: dropped,
    level: percent(v.level),
    speech_prob: percent(v.speech_prob),
  };
}

/** Segments in the meter; the lit count is `ceil(level * METER_SEGMENTS / 100)`. */
export const METER_SEGMENTS = 20;
/** Speech probability (percent) at or above which the meter shows "speech detected". */
export const SPEECH_DETECTED_PERCENT = 50;

export interface VoiceMeterView {
  /** Lit segments, 0..METER_SEGMENTS. */
  lit: number;
  /** Speech is detected: the lit segments are highlighted. */
  speech: boolean;
  /** No level reached the panel recently: shown as no signal, not as silence. */
  noSignal: boolean;
  /** The level, for assistive technology and the label. */
  value: number;
  text: string;
}

/** What the meter shows for the host level and speech probability (null: no signal). */
export function voiceMeterView(level: number | null, speechProb: number | null): VoiceMeterView {
  if (level === null) return { lit: 0, speech: false, noSignal: true, value: 0, text: "信号なし(マイクの音が届いていません)" };
  const clamped = Math.min(100, Math.max(0, Math.round(level)));
  const speech = speechProb !== null && speechProb >= SPEECH_DETECTED_PERCENT;
  return {
    lit: Math.ceil((clamped * METER_SEGMENTS) / 100),
    speech,
    noSignal: false,
    value: clamped,
    text: speech ? "話し声を検出" : "入力レベル",
  };
}

export interface VoiceInputView {
  /** The live input meter; null unless voice input is on (#440). */
  meter: VoiceMeterView | null;
  /** オフ / 聞き取り中 / 失敗: …. */
  stateText: string;
  health: Health;
  /** What turning it on does, always shown. */
  explanation: string;
  /** Shown whenever Thinking is remote, so it is read before the first turn-on. */
  cloudNote: string | null;
  canEnable: boolean;
  canDisable: boolean;
  enableLabel: string;
  /** Which microphone is open, or the default; host text, textContent only. */
  deviceText: string;
  /** The dropdown: the system default first, then the reported names (and the remembered one if unlisted). */
  deviceOptions: { value: string; label: string }[];
  /** The option value to select ("" is the system default). */
  deviceSelected: string;
  /** The headset switch (full duplex). */
  headset: boolean;
  headsetLabel: string;
  headsetHint: string;
  /** How many segments half duplex dropped, or null when none. */
  droppedText: string | null;
}

export const VOICE_INPUT_HEADSET_LABEL = "ヘッドセット使用(話し中に割り込み可)";
export const VOICE_INPUT_HEADSET_HINT_HALF = "オフの間は、アバターが話している間に始まった音声は聞き取らず、声での割り込みもしません(スピーカー使用時の自己反響を防ぎます)";
export const VOICE_INPUT_HEADSET_HINT_FULL = "オンの間は、アバターが話している間でも聞き取り、声で割り込めます。スピーカーでは自分の声を聞き取ることがあります";
export const VOICE_INPUT_DEFAULT_DEVICE = "システムの既定のデバイス";

export const VOICE_INPUT_EXPLANATION = "オンにすると、マイクで聞き取った音声に返事をします";
export const VOICE_INPUT_CLOUD_NOTE = "聞き取った音声の文字起こしは、クラウドの応答モデルに送信されます";

/** What the voice-input card shows, or `null` when it must stay hidden (absent or not composed). */
export function voiceInputView(state: PanelState | null): VoiceInputView | null {
  const v = state?.status.voice_input ?? null;
  if (v === null || !v.composed) return null;
  const names = [...v.devices];
  if (v.selected_device !== null && !names.includes(v.selected_device)) names.push(v.selected_device);
  const base = {
    explanation: VOICE_INPUT_EXPLANATION,
    cloudNote: v.thinking_remote ? VOICE_INPUT_CLOUD_NOTE : null,
    deviceText:
      v.state === "on" && v.device !== null
        ? `使用中のマイク: ${v.device}`
        : v.selected_device === null
          ? `マイク: ${VOICE_INPUT_DEFAULT_DEVICE}`
          : `マイク: ${v.selected_device}`,
    deviceOptions: [{ value: "", label: VOICE_INPUT_DEFAULT_DEVICE }, ...names.map((n) => ({ value: n, label: n }))],
    deviceSelected: v.selected_device ?? "",
    headset: v.duplex === "full",
    headsetLabel: VOICE_INPUT_HEADSET_LABEL,
    headsetHint: v.duplex === "full" ? VOICE_INPUT_HEADSET_HINT_FULL : VOICE_INPUT_HEADSET_HINT_HALF,
    meter: v.state === "on" ? voiceMeterView(v.level, v.speech_prob) : null,
    droppedText: v.dropped_during_playback > 0 ? `話し中に始まったため無視した音声: ${v.dropped_during_playback} 件` : null,
  };
  switch (v.state) {
    case "on":
      return { ...base, stateText: "聞き取り中", health: "ok", canEnable: false, canDisable: true, enableLabel: "オンにする" };
    case "failed":
      return {
        ...base,
        stateText: `失敗${v.reason === null || v.reason === "" ? "" : `: ${v.reason}`}`,
        health: "warn",
        canEnable: true,
        canDisable: true,
        enableLabel: "もう一度オンにする",
      };
    default:
      return { ...base, stateText: "オフ", health: "ok", canEnable: true, canDisable: false, enableLabel: "オンにする" };
  }
}

/** The body of `POST /api/speech-input`: a boolean and nothing else (the app refuses anything more). */
export function voiceInputBody(enabled: boolean): { enabled: boolean } {
  return { enabled };
}

/** The body of `POST /api/speech-input/device`: a name, or null for the system default. */
export function voiceInputDeviceBody(device: string): { device: string | null } {
  return { device: device === "" ? null : device };
}

/** The body of `POST /api/speech-input/duplex`. */
export function voiceInputDuplexBody(headset: boolean): { duplex: VoiceInputDuplex } {
  return { duplex: headset ? "full" : "half" };
}

/** The line shown after a device or duplex request (the host's text is only a short tail). */
export function voiceInputChoiceResultText(what: "マイク" | "ヘッドセット設定", ok: boolean, detail: unknown, status: number): string {
  const raw = typeof detail === "string" ? bounded(detail) : "";
  if (ok && status === 200) return `${what}を変更しました`;
  return `${what}を変更できませんでした${raw === "" ? ` (${status})` : `: ${raw}`}`;
}

/** The line shown after an on/off request; the host's own text is only a short tail. */
export function voiceInputResultText(enabled: boolean, ok: boolean, detail: unknown, status: number): string {
  const raw = typeof detail === "string" ? bounded(detail) : "";
  const name = enabled ? "オンにする" : "オフにする";
  if (ok && status === 200) return enabled ? "音声入力をオンにしました" : "音声入力をオフにしました";
  return `${name}ことができませんでした${raw === "" ? ` (${status})` : `: ${raw}`}`;
}

// ---- idle initiative (#87) ----

/** Mirrors `InitiativeStatus` (crates/app/src/initiative_host.rs). Counters only, never text. */
export interface InitiativeState {
  enabled: boolean;
  idle_secs: number;
  pending: boolean;
  fired: number;
  completed: number;
  failed: number;
  preempted_chat: number;
  preempted_speech: number;
  preempted_stop: number;
  suppressed_total: number;
  suppressed: [string, number][];
  /** The operator topic list (#87 S2): counts only; absent unless live topic editing is composed. */
  topics: TopicsState | null;
}

/** Mirrors `TopicsStatus` (crates/app/src/topics.rs). Counts and two facts, never the topics. */
export interface TopicsState {
  count: number;
  /** A save is written to the topics file and survives a restart. */
  persisted: boolean;
  /** The Thinking endpoint leaves this machine: a topic goes to a cloud model with its initiative. */
  thinking_remote: boolean;
  used: number;
  without_topic: number;
}

/** The topics object only if it has exactly the documented shape; anything else hides the topic card, not the switch. */
export function parseTopicsState(value: unknown): TopicsState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.persisted !== "boolean" || typeof v.thinking_remote !== "boolean") return null;
  if (!count(v.count) || !count(v.used) || !count(v.without_topic)) return null;
  return {
    count: v.count,
    persisted: v.persisted,
    thinking_remote: v.thinking_remote,
    used: v.used,
    without_topic: v.without_topic,
  };
}

const MAX_INITIATIVE_REASONS = 16;

/** An absent or malformed state is dropped (the card stays hidden); the rest of the page still works. */
export function parseInitiativeState(value: unknown): InitiativeState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.enabled !== "boolean" || typeof v.pending !== "boolean") return null;
  const numbers = [
    v.idle_secs,
    v.fired,
    v.completed,
    v.failed,
    v.preempted_chat,
    v.preempted_speech,
    v.preempted_stop,
    v.suppressed_total,
  ];
  if (!numbers.every(count)) return null;
  const reasons = v.suppressed;
  if (!Array.isArray(reasons) || reasons.length > MAX_INITIATIVE_REASONS) return null;
  const suppressed: [string, number][] = [];
  for (const entry of reasons) {
    if (!Array.isArray(entry) || entry.length !== 2) return null;
    const [name, n] = entry as [unknown, unknown];
    if (typeof name !== "string" || !/^[a-z_]{1,32}$/.test(name) || !count(n)) return null;
    suppressed.push([name, n]);
  }
  return {
    enabled: v.enabled,
    idle_secs: v.idle_secs as number,
    pending: v.pending,
    fired: v.fired as number,
    completed: v.completed as number,
    failed: v.failed as number,
    preempted_chat: v.preempted_chat as number,
    preempted_speech: v.preempted_speech as number,
    preempted_stop: v.preempted_stop as number,
    suppressed_total: v.suppressed_total as number,
    suppressed,
    // Absent from an older host: the card stays hidden, the switch still works.
    topics: parseTopicsState(v.topics),
  };
}

export const INITIATIVE_LABEL = "沈黙時に自分から話す";

const INITIATIVE_REASON_TEXT: Record<string, string> = {
  muted: "ミュート中",
  operator_stopped: "停止直後",
  floor_held: "相手が話し中",
  speaking: "話している最中",
  generating: "返答を作っている最中",
  content_queued: "コメント処理待ち",
  deferred_pending: "返信待ちのコメントあり",
  settling: "発話直後",
  budget_gap: "間隔の上限",
  budget_window: "回数の上限",
};

export interface InitiativeView {
  label: string;
  checked: boolean;
  hint: string;
  /** One line of counters; reasons are the closed vocabulary the host names, never free text. */
  summary: string;
}

/** What the initiative card shows, or `null` when it must stay hidden (absent: not composed). */
export function initiativeView(state: PanelState | null): InitiativeView | null {
  const v = state?.status.initiative ?? null;
  if (v === null) return null;
  const reasons = v.suppressed
    .map(([name, n]) => `${INITIATIVE_REASON_TEXT[name] ?? "その他"} ${n}`)
    .join("、");
  const preempted = v.preempted_chat + v.preempted_speech + v.preempted_stop;
  const parts = [`話しかけた回数 ${v.fired}`, `失敗 ${v.failed}`, `譲った回数 ${preempted}`];
  if (v.suppressed_total > 0) parts.push(`見送り ${v.suppressed_total}(${reasons})`);
  return {
    label: INITIATIVE_LABEL,
    checked: v.enabled,
    hint: `コメントも音声も ${v.idle_secs} 秒ない間だけ、自分から短く話しかけます。コメントや音声が来たらすぐやめて、そちらを優先します`,
    summary: parts.join(" · "),
  };
}

/** The body of `POST /api/initiative`: a boolean and nothing else (the app refuses anything more). */
export function initiativeBody(enabled: boolean): { enabled: boolean } {
  return { enabled };
}

/** The line shown after an on/off request; the host's own text is only a short tail. */
export function initiativeResultText(enabled: boolean, ok: boolean, detail: unknown, status: number): string {
  const raw = typeof detail === "string" ? bounded(detail) : "";
  if (ok && status === 200) return enabled ? "自分から話すようにしました" : "自分からは話さないようにしました";
  return `切り替えできませんでした${raw === "" ? ` (${status})` : `: ${raw}`}`;
}

// ---- operator topic list (#87 S2) ----

/** Mirrors `MAX_TOPICS` and `MAX_TOPIC_CHARS` (crates/domain/src/topics.rs). */
export const MAX_TOPICS = 32;
export const MAX_TOPIC_CHARS = 80;
/** Mirrors `MAX_TOPICS_FILE_BYTES`: the longest text the page will accept back from the host. */
const MAX_TOPICS_TEXT_BYTES = 16 * 1024;

/**
 * The topics the host will keep for this text (`parse_topics`): LF line endings,
 * each line trimmed, blank lines dropped, a repeated line kept once.
 */
export function topicLines(text: string): string[] {
  const lines: string[] = [];
  for (const raw of text.replace(/\r\n?/g, "\n").split("\n")) {
    const line = raw.trim();
    if (line !== "" && !lines.includes(line)) lines.push(line);
  }
  return lines;
}

/** Characters as the host counts them (code points, not UTF-16 units). */
export function topicChars(line: string): number {
  return Array.from(line).length;
}

export const TOPICS_LABEL = "雑談ネタ";
export const TOPICS_EXPLANATION =
  "沈黙時に自分から話すときの話題を、1行に1つずつ書きます(最大 32 個、1個 80 文字まで)。順番に1つずつ使い、直前に使った話題は飛ばします。空のときは話題なしで話します。保存すると次に話すときから反映されます";
export const TOPICS_CLOUD_NOTE =
  "話題は、それを使う発話のたびにクラウドのモデルへ送信されます / sent to the cloud model with the initiative that uses it";
export const TOPICS_MEMORY_ONLY_NOTE =
  "保存先のファイルが設定されていないため、この内容はアプリを再起動すると元に戻ります";

export interface TopicsView {
  /** `3 / 32 個`. */
  counterText: string;
  /** warn once a bound is exceeded. */
  health: Health;
  /** The first problem the host would refuse the text for, or null. */
  problem: string | null;
  explanation: string;
  cloudNote: string | null;
  memoryOnlyNote: string | null;
  canSave: boolean;
  /** One line of counters, never a topic. */
  summary: string;
}

/** What the topic card shows for the text in the editor, or `null` when it stays hidden. */
export function topicsView(state: PanelState | null, text: string): TopicsView | null {
  const topics = state?.status.initiative?.topics ?? null;
  if (topics === null) return null;
  const lines = topicLines(text);
  const tooMany = lines.length > MAX_TOPICS;
  const tooLong = lines.some((line) => topicChars(line) > MAX_TOPIC_CHARS);
  const problem = tooMany
    ? `話題は ${MAX_TOPICS} 個までです`
    : tooLong
      ? `1つの話題は ${MAX_TOPIC_CHARS} 文字までです`
      : null;
  return {
    counterText: `${lines.length} / ${MAX_TOPICS} 個`,
    health: problem === null ? "ok" : "warn",
    problem,
    explanation: TOPICS_EXPLANATION,
    cloudNote: topics.thinking_remote ? TOPICS_CLOUD_NOTE : null,
    memoryOnlyNote: topics.persisted ? null : TOPICS_MEMORY_ONLY_NOTE,
    canSave: problem === null,
    summary: `話題を使った回数 ${topics.used} · 話題なしで話した回数 ${topics.without_topic}`,
  };
}

/** The body of `POST /api/topics/set`: the whole text and nothing else. */
export function topicsSetBody(text: string): { text: string } {
  return { text };
}

/** The body of `POST /api/topics/get`: an empty object. */
export function topicsGetBody(): Record<string, never> {
  return {};
}

/** The text a `topics/get` answer carries, only if it is a string within the file bound. */
export function topicsTextFrom(json: unknown): string | null {
  if (typeof json !== "object" || json === null) return null;
  const text = (json as Record<string, unknown>).text;
  if (typeof text !== "string" || new TextEncoder().encode(text).length > MAX_TOPICS_TEXT_BYTES) return null;
  return text;
}

/** The line shown after a save or a reload; the host's own text is only a short tail. */
export function topicsResultText(kind: "save" | "reload", ok: boolean, detail: unknown, status: number): string {
  const raw = typeof detail === "string" ? bounded(detail) : "";
  if (ok && status === 200) return kind === "save" ? "保存しました。次に話すときから反映されます" : "読み込みました";
  const name = kind === "save" ? "保存" : "読み込み";
  return `${name}できませんでした${raw === "" ? ` (${status})` : `: ${raw}`}`;
}

// ---- session conversation log (#463) ----

/** Mirrors `SessionLogStatus` (crates/app/src/session_log.rs). Counters and the location, never logged text. */
export interface SessionLogState {
  enabled: boolean;
  location: string | null;
  max_file_bytes: number;
  retain_sessions: number;
  files_kept: number;
  lines_written: number;
  bytes_written: number;
  dropped_queue_full: number;
  dropped_file_full: number;
  write_errors: number;
  capped: boolean;
  settings_save_failed: boolean;
}

const MAX_LOG_LOCATION_CHARS = 512;

/** An absent or malformed state is dropped (the card stays hidden); the rest of the page still works. */
export function parseSessionLogState(value: unknown): SessionLogState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.enabled !== "boolean" || typeof v.capped !== "boolean" || typeof v.settings_save_failed !== "boolean") {
    return null;
  }
  const numbers = [
    v.max_file_bytes,
    v.retain_sessions,
    v.files_kept,
    v.lines_written,
    v.bytes_written,
    v.dropped_queue_full,
    v.dropped_file_full,
    v.write_errors,
  ];
  if (!numbers.every(count)) return null;
  const location = v.location;
  if (location !== undefined && location !== null && (typeof location !== "string" || location.length > MAX_LOG_LOCATION_CHARS)) {
    return null;
  }
  return {
    enabled: v.enabled,
    location: typeof location === "string" ? location : null,
    max_file_bytes: v.max_file_bytes as number,
    retain_sessions: v.retain_sessions as number,
    files_kept: v.files_kept as number,
    lines_written: v.lines_written as number,
    bytes_written: v.bytes_written as number,
    dropped_queue_full: v.dropped_queue_full as number,
    dropped_file_full: v.dropped_file_full as number,
    write_errors: v.write_errors as number,
    capped: v.capped,
    settings_save_failed: v.settings_save_failed,
  };
}

export const SESSION_LOG_LABEL = "配信ログを保存";

export interface SessionLogView {
  label: string;
  checked: boolean;
  /** Says plainly what is stored and where; the location is the host's own path text, shown as text. */
  hint: string;
  /** One line of counters. */
  summary: string;
  /** Problems the operator should know about (empty when none). */
  warning: string;
  /** Delete-all is offered only when there is something saved. */
  canDelete: boolean;
}

const megabytes = (bytes: number): string => `${Math.max(1, Math.round(bytes / (1024 * 1024)))} MB`;

/** What the session-log card shows, or `null` when it must stay hidden (absent: not composed). */
export function sessionLogView(state: PanelState | null): SessionLogView | null {
  const v = state?.status.session_log ?? null;
  if (v === null) return null;
  const where = v.location === null ? "このPCの中" : v.location;
  const warnings: string[] = [];
  if (v.capped) warnings.push(`このファイルが上限(${megabytes(v.max_file_bytes)})に達したため、これ以降は保存していません`);
  if (v.write_errors > 0) warnings.push(`書き込みに失敗した回数 ${v.write_errors}`);
  if (v.dropped_queue_full > 0) warnings.push(`保存が追いつかず見送った行 ${v.dropped_queue_full}`);
  if (v.settings_save_failed) warnings.push("オンオフの設定を保存できませんでした(次回起動時は元に戻ります)");
  return {
    label: SESSION_LOG_LABEL,
    checked: v.enabled,
    hint:
      `オンの間、コメントや音声の文字起こし(視聴者名は「viewer-01」のような仮名に置き換え)と、話した内容、` +
      `返答の種類(キャッシュ/生成)、繰り返し判定を、このPCの ${where} にファイルで保存します。` +
      `音声・キーやトークン・キャラクター設定や記憶の本文は保存しません。外部には送りません。` +
      `コメントは視聴者のものなので、取り扱いは配信者の責任です。` +
      `直近 ${v.retain_sessions} 回分だけ残り、古いものは自動で消えます。`,
    summary: `保存済みファイル ${v.files_kept} · 書き込んだ行 ${v.lines_written}`,
    warning: warnings.join("。"),
    canDelete: v.files_kept > 0,
  };
}

/** The body of `POST /api/session-log`: a boolean and nothing else (the app refuses anything more). */
export function sessionLogBody(enabled: boolean): { enabled: boolean } {
  return { enabled };
}

/** The body of `POST /api/session-log/delete`: an empty object (the app refuses anything more). */
export function sessionLogDeleteBody(): Record<string, never> {
  return {};
}

export const SESSION_LOG_DELETE_CONFIRM_TEXT =
  "保存した配信ログをすべて削除します。元に戻せません。よろしいですか?";

/** The line shown after an on/off request; the host's own text is only a short tail. */
export function sessionLogResultText(enabled: boolean, ok: boolean, detail: unknown, status: number): string {
  const raw = typeof detail === "string" ? bounded(detail) : "";
  if (ok && status === 200) return enabled ? "配信ログを保存するようにしました" : "配信ログを保存しないようにしました";
  return `切り替えできませんでした${raw === "" ? ` (${status})` : `: ${raw}`}`;
}

/** The line shown after delete-all. */
export function sessionLogDeleteResultText(ok: boolean, detail: unknown, status: number): string {
  const raw = typeof detail === "string" ? bounded(detail) : "";
  if (ok && status === 200) return "保存した配信ログを削除しました";
  return `削除できませんでした${raw === "" ? ` (${status})` : `: ${raw}`}`;
}

// ---- character sheet (#401) ----

/** Mirrors `MAX_PERSONA_BYTES` (crates/domain/src/persona.rs). */
export const MAX_PERSONA_BYTES = 3000;

/** Mirrors `PersonaEditorStatus` (crates/app/src/persona.rs). */
export interface PersonaEditorState {
  /** A save is written to the persona file and survives a restart. */
  persisted: boolean;
  /** The Thinking endpoint leaves this machine: the sheet goes to a cloud model. */
  thinking_remote: boolean;
}

/** The editor state only if it has exactly the documented shape; anything else hides the editor. */
export function parsePersonaEditorState(value: unknown): PersonaEditorState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.persisted !== "boolean" || typeof v.thinking_remote !== "boolean") return null;
  return { persisted: v.persisted, thinking_remote: v.thinking_remote };
}

/**
 * The size the host will measure: UTF-8 bytes after line endings become LF and
 * trailing whitespace is trimmed (the host's normalisation, `parse_persona`).
 */
export function personaBytes(text: string): number {
  const normalised = text.replace(/\r\n?/g, "\n").trimEnd();
  return new TextEncoder().encode(normalised).length;
}

export const PERSONA_CLOUD_NOTE =
  "この内容は返答ごとにクラウドのモデルへ送信されます / sent to the cloud model with every reply";
export const PERSONA_MEMORY_ONLY_NOTE =
  "保存先のファイルが設定されていないため、この内容はアプリを再起動すると元に戻ります";
export const PERSONA_EXPLANATION =
  "キャラクターの名前・一人称・口調などを書きます。保存すると次の返答から反映されます(すでに生成中の返答は元の内容のままです)。空にして保存すると解除されます";

export interface PersonaView {
  /** `1,234 / 3,000 バイト`. */
  counterText: string;
  /** warn once the text is over the limit. */
  health: Health;
  explanation: string;
  /** Shown whenever Thinking is remote, so it is read before the first save. */
  cloudNote: string | null;
  /** Shown when a save is not written to a file. */
  memoryOnlyNote: string | null;
  canSave: boolean;
}

/** What the character-sheet card shows for the text in the editor, or `null` when it stays hidden. */
export function personaView(state: PanelState | null, text: string): PersonaView | null {
  const editor = state?.status.persona_editor ?? null;
  if (editor === null) return null;
  const bytes = personaBytes(text);
  return {
    counterText: `${bytes.toLocaleString("en-US")} / ${MAX_PERSONA_BYTES.toLocaleString("en-US")} バイト`,
    health: bytes > MAX_PERSONA_BYTES ? "warn" : "ok",
    explanation: PERSONA_EXPLANATION,
    cloudNote: editor.thinking_remote ? PERSONA_CLOUD_NOTE : null,
    memoryOnlyNote: editor.persisted ? null : PERSONA_MEMORY_ONLY_NOTE,
    canSave: bytes <= MAX_PERSONA_BYTES,
  };
}

/** The body of `POST /api/persona/set`: the whole text and nothing else. */
export function personaSetBody(text: string): { text: string } {
  return { text };
}

/** The body of `POST /api/persona/get`: an empty object. */
export function personaGetBody(): Record<string, never> {
  return {};
}

/** The text a `persona/get` answer carries, only if it is a string within the file bound. */
export function personaTextFrom(json: unknown): string | null {
  if (typeof json !== "object" || json === null) return null;
  const text = (json as Record<string, unknown>).text;
  if (typeof text !== "string" || personaBytes(text) > MAX_PERSONA_BYTES) return null;
  return text;
}

/** The line shown after a save or a reload; the host's own text is only a short tail. */
export function personaResultText(kind: "save" | "reload", ok: boolean, detail: unknown, status: number): string {
  const raw = typeof detail === "string" ? bounded(detail) : "";
  if (ok && status === 200) return kind === "save" ? "保存しました。次の返答から反映されます" : "読み込みました";
  const name = kind === "save" ? "保存" : "読み込み";
  return `${name}できませんでした${raw === "" ? ` (${status})` : `: ${raw}`}`;
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

// ---- First-run wording (#384) ----
// The host sends English diagnostics (EngineError Display = "<Kind>: <message>",
// operator stop/mute detail tokens). The page shows short Japanese text for the
// stable, common ones and keeps the raw string for support. Everything here is
// total: no input can make it throw, and unknown text passes through unchanged
// (bounded, so a hostile or runaway string cannot flood the page).

/** Longest host string the page will show or match against. */
export const MAX_HOST_TEXT_CHARS = 300;

function bounded(raw: string): string {
  return raw.length > MAX_HOST_TEXT_CHARS ? `${raw.slice(0, MAX_HOST_TEXT_CHARS)}…` : raw;
}

export type AvatarLink = "none" | "disconnected" | "handshaking" | "degraded";

/**
 * Mirrors LocalAvatarOutput::connect (crates/app/src/outputs.rs):
 * `local avatar renderer is {Disconnected|Handshaking|Degraded}`.
 */
export function avatarLinkState(raw: string | null | undefined): AvatarLink {
  if (typeof raw !== "string") return "none";
  const m = /local avatar renderer is (Disconnected|Handshaking|Degraded)/.exec(raw.slice(0, MAX_HOST_TEXT_CHARS));
  if (m === null) return "none";
  return m[1] === "Disconnected" ? "disconnected" : m[1] === "Handshaking" ? "handshaking" : "degraded";
}

/** A status-row error (audio/avatar/stream) in Japanese; unknown text is kept. */
export function reasonText(raw: string | null | undefined): string {
  if (typeof raw !== "string" || raw === "") return "";
  switch (avatarLinkState(raw)) {
    case "disconnected":
      return "アバター画面が開かれていません";
    case "handshaking":
      return "アバター画面と接続しています";
    case "degraded":
      return "アバター画面の応答が遅れています";
    case "none":
      break;
  }
  const text = raw.slice(0, MAX_HOST_TEXT_CHARS);
  // crates/app/src/audio_stop.rs (AudioChannel::execute)
  if (text === "Unavailable: audio output busy ending playback") return "音声を止めている最中のため再生できませんでした";
  // crates/app/src/lib.rs (stop_started_audio)
  if (text === "audio stop did not complete within its bound") return "音声の停止を確認できませんでした(音が残っている可能性があります)";
  return bounded(raw);
}

const STOP_TOKENS: Record<string, string> = {
  nothing_playing: "再生中の音声はありませんでした",
  hard_stopped: "音声を止めました",
  unsupported: "この音声出力は停止に対応していません(音が残る可能性があります)",
  failed: "音声を止められませんでした",
  unconfirmed: "音声が止まったか確認できませんでした",
};

function audioStopText(token: string): string | null {
  const known = Object.hasOwn(STOP_TOKENS, token) ? STOP_TOKENS[token] : undefined;
  if (known !== undefined) return known;
  return /^faded_\d{1,6}ms$/.test(token) ? "音声をフェードアウトして止めました" : null;
}

export interface ControlResultText {
  /** Shown line. */
  text: string;
  /** Raw host detail for the tooltip; empty when there was none. */
  raw: string;
}

/**
 * Result line of an emergency stop / mute. Detail formats come from
 * crates/app/src/operator_control.rs (`stopped N items` | `muted` |
 * `authenticated`, then `; audio_stop=<token> avatar_reset=ok|failed` from
 * crates/app/src/audio_stop.rs) and `rejected: ...` for a refusal.
 */
export function controlResultText(action: "stop" | "mute", ok: boolean, detail: unknown, status: number): ControlResultText {
  const raw = typeof detail === "string" ? bounded(detail) : "";
  const name = action === "stop" ? "停止" : "ミュート";
  if (!ok || status !== 200) {
    return { text: `失敗しました${raw === "" ? ` (${status})` : `: ${raw}`}`, raw };
  }
  const m = /^(?:stopped (\d{1,9}) items?|muted|authenticated)(?:; (.*))?$/.exec(raw);
  if (m === null) return { text: raw === "" ? `${name}しました` : `${name}しました: ${raw}`, raw };
  const parts: string[] = [];
  if (m[1] !== undefined) parts.push(Number(m[1]) === 0 ? "止める発話はありませんでした" : `${m[1]}件の発話を止めました`);
  const tail = m[2] ?? "";
  const audio = /(?:^| )audio_stop=(\S+)/.exec(tail);
  const reset = /(?:^| )avatar_reset=(ok|failed)(?: |$)/.exec(tail);
  if (audio !== null) parts.push(audioStopText(audio[1] ?? "") ?? `音声: ${audio[1]}`);
  if (reset !== null) parts.push(reset[1] === "ok" ? "アバターを戻しました" : "アバターを戻せませんでした");
  return { text: parts.length === 0 ? `${name}しました` : `${name}しました(${parts.join("、")})`, raw };
}

/** Confirm text before ending the app (#449). */
export const SHUTDOWN_CONFIRM_TEXT = "配信中なら止まります。アプリを終了しますか?";

/** Shown once the host accepted `app.shutdown`; polling stops, so nothing says "reconnecting". */
export const SHUTDOWN_ENDED_TEXT = "終了しました。もう一度使うには aivtuber-setup start を実行してください";

/**
 * Result of `POST /api/shutdown`. Accepted: the ended message (the host answers
 * before it exits, with `shutting down; stopped N items; ...`). Refused: the
 * app is still running, so say so and keep the page live.
 */
export function shutdownResultText(ok: boolean, detail: unknown, status: number): { ended: boolean; text: string; raw: string } {
  const raw = typeof detail === "string" ? bounded(detail) : "";
  if (ok && status === 200) return { ended: true, text: SHUTDOWN_ENDED_TEXT, raw };
  return { ended: false, text: `終了できませんでした${raw === "" ? ` (${status})` : `: ${raw}`}`, raw };
}

export type ConnectionKind =
  | { kind: "connecting" }
  | { kind: "ended" }
  | { kind: "connected" }
  | { kind: "reconnecting" }
  | { kind: "lost" }
  | { kind: "no-token" }
  | { kind: "mismatch" }
  | { kind: "http-error"; status: number };

/** The connection line: connecting, connected and reconnecting stay distinct. */
export function connectionText(c: ConnectionKind): string {
  switch (c.kind) {
    case "connecting":
      return "接続しています…";
    case "connected":
      return "接続済み";
    case "ended":
      return "アプリは終了しました";
    case "reconnecting":
      return "再接続中…(アプリが終了した可能性があります)";
    case "lost":
      return "切断されました(アプリを再起動して、aivtuber-setup start が表示した新しいURLを開いてください)";
    case "no-token":
      return "切断されました(トークンがありません。aivtuber-setup start が表示したURLをそのまま開いてください)";
    case "mismatch":
      return "この画面とバージョンが合いません(ページを再読み込みしてください)";
    case "http-error":
      return `再接続中…(応答エラー ${Number.isFinite(c.status) ? Math.trunc(c.status) : 0})`;
  }
}

// ---- memory seed (#402) ----

/** Mirror `MAX_SEED_ENTRIES` / `MAX_SEED_CLAIM_BYTES` / `MAX_SEED_FILE_BYTES` (crates/app/src/memory_seed.rs). */
export const MAX_SEED_ENTRIES = 128;
export const MAX_SEED_CLAIM_BYTES = 240;
export const MAX_SEED_FILE_BYTES = 64 * 1024;
/** The only `schema_version` the host's seed parser accepts. */
export const SEED_SCHEMA_VERSION = "0.1.0";

export type SeedKind = "persona-fact" | "running-joke" | "commitment" | "segment-summary";

/** The four kinds of `schemas/memory-seed.schema.json`, with the label the card shows. */
export const SEED_KINDS: readonly { kind: SeedKind; label: string }[] = [
  { kind: "persona-fact", label: "人物設定" },
  { kind: "running-joke", label: "お決まりのネタ" },
  { kind: "commitment", label: "約束" },
  { kind: "segment-summary", label: "配信のまとめ" },
];

export interface SeedEntry {
  kind: SeedKind;
  claim: string;
}

/** The whole seed document (`schemas/memory-seed.schema.json`): Save sends all of it. */
export interface SeedDocument {
  schema_version: string;
  seed_id: string;
  revision: string;
  entries: SeedEntry[];
}

/** Mirrors `MemorySeedEditorStatus` (crates/app/src/memory_seed.rs). Counts and a hash, never a claim. */
export interface MemorySeedEditorState {
  /** A save is written to the seed file and survives a restart. */
  persisted: boolean;
  /** The Thinking endpoint leaves this machine: relevant entries go to a cloud model. */
  thinking_remote: boolean;
  entries: number;
  content_hash: string;
}

/** The editor state only if it has exactly the documented shape; anything else hides the card. */
export function parseMemorySeedEditorState(value: unknown): MemorySeedEditorState | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.persisted !== "boolean" || typeof v.thinking_remote !== "boolean") return null;
  if (typeof v.entries !== "number" || !Number.isInteger(v.entries) || v.entries < 0) return null;
  if (typeof v.content_hash !== "string") return null;
  return {
    persisted: v.persisted,
    thinking_remote: v.thinking_remote,
    entries: v.entries,
    content_hash: v.content_hash,
  };
}

const SEED_KIND_SET: ReadonlySet<string> = new Set(SEED_KINDS.map((k) => k.kind));

/**
 * The document a `memory-seed/get` answer carries (its `text`), only if it is
 * JSON of exactly the seed shape within the bounds. Anything else is `null`, so
 * the card never edits a document it cannot send back.
 */
export function parseSeedDocument(text: unknown): SeedDocument | null {
  if (typeof text !== "string" || new TextEncoder().encode(text).length > MAX_SEED_FILE_BYTES) return null;
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const v = value as Record<string, unknown>;
  if (v.schema_version !== SEED_SCHEMA_VERSION) return null;
  if (typeof v.seed_id !== "string" || typeof v.revision !== "string") return null;
  if (!Array.isArray(v.entries) || v.entries.length > MAX_SEED_ENTRIES) return null;
  const entries: SeedEntry[] = [];
  for (const raw of v.entries) {
    if (typeof raw !== "object" || raw === null) return null;
    const e = raw as Record<string, unknown>;
    if (typeof e.kind !== "string" || !SEED_KIND_SET.has(e.kind) || typeof e.claim !== "string") return null;
    entries.push({ kind: e.kind as SeedKind, claim: e.claim });
  }
  return { schema_version: v.schema_version, seed_id: v.seed_id, revision: v.revision, entries };
}

/** The size the host measures for one claim: UTF-8 bytes of the claim with its edges trimmed. */
export function seedClaimBytes(claim: string): number {
  return new TextEncoder().encode(claim.trim()).length;
}

/** The host's duplicate rule (`fold`): ignoring case and the amount of white space. */
function seedFold(claim: string): string {
  return claim.split(/\s+/).filter((part) => part !== "").join(" ").toLowerCase();
}

// eslint-disable-next-line no-control-regex
const CONTROL_CHARS = /[\u0000-\u001f\u007f-\u009f]/;

/** Why a claim cannot be saved, in Japanese, or `null`. The host checks again. */
export function seedClaimProblem(claim: string, earlier: readonly string[]): string | null {
  const trimmed = claim.trim();
  if (trimmed === "") return "内容が空です。入力するか、この項目を削除してください";
  if (CONTROL_CHARS.test(trimmed)) return "改行や制御文字は使えません";
  if (seedClaimBytes(claim) > MAX_SEED_CLAIM_BYTES) return `${MAX_SEED_CLAIM_BYTES} バイトを超えています`;
  const folded = seedFold(trimmed);
  if (earlier.some((other) => seedFold(other) === folded)) return "同じ内容の項目が前にあります";
  return null;
}

export const SEED_CLOUD_NOTE =
  "関連する項目は返答のたびにクラウドのモデルへ送信されます / relevant entries are sent to the cloud model";
export const SEED_MEMORY_ONLY_NOTE =
  "保存先のファイルが設定されていないため、この内容はアプリを再起動すると元に戻ります";
export const SEED_EXPLANATION =
  "配信者として覚えておくことを1行ずつ書きます。保存すると次の返答から反映されます。返答ごとに、話題に関係のある項目だけがモデルに渡されます";

export interface SeedRowView {
  kindLabel: string;
  /** `12 / 240 バイト`. */
  bytesText: string;
  health: Health;
  /** Why this row blocks saving, or `null`. */
  problem: string | null;
}

export interface SeedView {
  /** `3 / 128 件`. */
  countText: string;
  health: Health;
  explanation: string;
  /** Shown whenever Thinking is remote, so it is read before the first save. */
  cloudNote: string | null;
  /** Shown when a save is not written to a file. */
  memoryOnlyNote: string | null;
  rows: SeedRowView[];
  canSave: boolean;
  canAdd: boolean;
}

/** What the memory card shows for the entries in the editor, or `null` when it stays hidden. */
export function seedView(state: PanelState | null, entries: readonly SeedEntry[]): SeedView | null {
  const editor = state?.status.memory_seed_editor ?? null;
  if (editor === null) return null;
  const rows = entries.map((entry, index): SeedRowView => {
    const bytes = seedClaimBytes(entry.claim);
    const problem = seedClaimProblem(
      entry.claim,
      entries.slice(0, index).map((e) => e.claim),
    );
    return {
      kindLabel: SEED_KINDS.find((k) => k.kind === entry.kind)?.label ?? entry.kind,
      bytesText: `${bytes} / ${MAX_SEED_CLAIM_BYTES} バイト`,
      health: problem === null ? "ok" : "warn",
      problem,
    };
  });
  const tooMany = entries.length > MAX_SEED_ENTRIES;
  return {
    countText: `${entries.length} / ${MAX_SEED_ENTRIES} 件`,
    health: tooMany ? "warn" : "ok",
    explanation: SEED_EXPLANATION,
    cloudNote: editor.thinking_remote ? SEED_CLOUD_NOTE : null,
    memoryOnlyNote: editor.persisted ? null : SEED_MEMORY_ONLY_NOTE,
    rows,
    canSave: !tooMany && rows.every((row) => row.problem === null),
    canAdd: entries.length < MAX_SEED_ENTRIES,
  };
}

/** Longest claim (in characters) a chip shows before it is cut with an ellipsis. */
export const SEED_CHIP_MAX_CHARS = 40;
export const SEED_EMPTY_CHIP_TEXT = "(空の項目)";

export interface SeedChipView {
  /** The kind label, shown small before the claim. */
  kindLabel: string;
  /** The claim cut to one short line, or the placeholder for an empty claim. */
  text: string;
  /** The whole claim for the title attribute; empty for an empty claim. */
  title: string;
  /** Names the 1-based index and kind for assistive tech. */
  ariaLabel: string;
}

/** What a collapsed entry shows as a chip. Cuts by code point so a surrogate pair is never split. */
export function seedChipView(entry: SeedEntry, index: number): SeedChipView {
  const kindLabel = SEED_KINDS.find((k) => k.kind === entry.kind)?.label ?? entry.kind;
  const claim = entry.claim.trim();
  const points = Array.from(claim);
  const text =
    claim === ""
      ? SEED_EMPTY_CHIP_TEXT
      : points.length > SEED_CHIP_MAX_CHARS
        ? `${points.slice(0, SEED_CHIP_MAX_CHARS).join("")}…`
        : claim;
  return {
    kindLabel,
    text,
    title: claim,
    ariaLabel: `項目 ${index + 1}(${kindLabel}): ${text} - 編集`,
  };
}

/** The open editor after an add: the new last entry. */
export function seedOpenAfterAdd(length: number): number {
  return length - 1;
}

/** The open editor after `removed` is deleted: closed if it was that one, shifted if it was after it. */
export function seedOpenAfterDelete(open: number | null, removed: number): number | null {
  if (open === null || open === removed) return null;
  return open > removed ? open - 1 : open;
}

/** Clicking a chip opens it; at most one editor is open, so this replaces any other. */
export function seedOpenAfterOpen(open: number | null, index: number, length: number): number | null {
  return index >= 0 && index < length ? index : open;
}

/** The body of `POST /api/memory-seed/set`: the whole document, claims trimmed, nothing else. */
export function seedSetBody(seed: Pick<SeedDocument, "seed_id" | "revision">, entries: readonly SeedEntry[]): SeedDocument {
  return {
    schema_version: SEED_SCHEMA_VERSION,
    seed_id: seed.seed_id,
    revision: seed.revision,
    entries: entries.map((entry) => ({ kind: entry.kind, claim: entry.claim.trim() })),
  };
}

/** The body of `POST /api/memory-seed/get`: an empty object. */
export function seedGetBody(): Record<string, never> {
  return {};
}

/** The text a `memory-seed/get` answer carries, if it is a string; `parseSeedDocument` checks the rest. */
export function seedTextFrom(json: unknown): string | null {
  if (typeof json !== "object" || json === null) return null;
  const text = (json as Record<string, unknown>).text;
  return typeof text === "string" ? text : null;
}

/** The line shown after a save or a reload; the host's own text is only a short tail. */
export function seedResultText(kind: "save" | "reload", ok: boolean, detail: unknown, status: number): string {
  const raw = typeof detail === "string" ? bounded(detail) : "";
  if (ok && status === 200) return kind === "save" ? "保存しました。次の返答から反映されます" : "読み込みました";
  const name = kind === "save" ? "保存" : "読み込み";
  return `${name}できませんでした${raw === "" ? ` (${status})` : `: ${raw}`}`;
}
