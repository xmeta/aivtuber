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
  return { ...(v as PanelState), status: { ...v.status, thinking_model: thinkingModel } };
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
