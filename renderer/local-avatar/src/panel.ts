// Local operation panel page (issue #335). Plain DOM, no framework. It talks
// to the same origin only, holds the per-launch secret in memory (received once
// in the URL fragment), and renders every server string with textContent.
import {
  type ChatEntry,
  type PanelState,
  checkChat,
  modelView,
  otherReplies,
  parseState,
  pushChat,
  repliesFor,
  safeAvatarUrl,
  statusRows,
  tokenFromHash,
} from "./panel-core";

const POLL_MS = 1000;
const REQUEST_TIMEOUT_MS = 6000;

// The secret lives only in this variable: not in storage, not in a cookie, and
// the fragment is removed from the address bar right away.
const token = tokenFromHash(location.hash);
history.replaceState(null, "", location.pathname);

let state: PanelState | null = null;
let chat: ChatEntry[] = [];
let connection = "接続中…";
let connectionLost = false;
let pollTimer: ReturnType<typeof setInterval> | undefined;

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (el === null) throw new Error(`missing #${id}`);
  return el as T;
};

async function api(path: string, init?: { method: "POST"; body?: string }): Promise<{ status: number; json: unknown }> {
  if (token === null) throw new Error("no token");
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  try {
    const response = await fetch(path, {
      method: init?.method ?? "GET",
      headers: { "X-Panel-Token": token, ...(init ? { "Content-Type": "application/json" } : {}) },
      body: init?.body,
      cache: "no-store",
      credentials: "omit",
      referrerPolicy: "no-referrer",
      signal: controller.signal,
    });
    let json: unknown = null;
    try {
      json = await response.json();
    } catch {
      // an empty body (401, 403) carries no JSON
    }
    return { status: response.status, json };
  } finally {
    clearTimeout(timer);
  }
}

function render(): void {
  $("connection").textContent = connection;
  const rows = $("status");
  rows.replaceChildren();
  if (state !== null) {
    for (const row of statusRows(state)) {
      const item = document.createElement("div");
      item.className = `row ${row.health}`;
      const label = document.createElement("span");
      label.className = "label";
      label.textContent = row.label;
      const value = document.createElement("span");
      value.textContent = row.value;
      item.append(label, value);
      rows.append(item);
    }
  }
  renderModel();
  const link = $<HTMLAnchorElement>("avatar-link");
  const url = safeAvatarUrl(state?.avatar_url);
  link.hidden = url === null;
  if (url !== null) link.href = url;

  const log = $("chat-log");
  log.replaceChildren();
  for (const entry of chat) {
    const mine = document.createElement("div");
    mine.className = "bubble mine";
    mine.textContent = entry.text;
    log.append(mine);
    const replies = repliesFor(state, entry.id);
    if (replies.length === 0) {
      const note = document.createElement("div");
      note.className = "note";
      note.textContent = entry.note;
      log.append(note);
    }
    for (const text of replies) {
      const bubble = document.createElement("div");
      bubble.className = "bubble reply";
      bubble.textContent = text;
      log.append(bubble);
    }
  }
  const others = $("other-replies");
  others.replaceChildren();
  for (const r of otherReplies(state)) {
    const li = document.createElement("li");
    li.textContent = r.text;
    others.append(li);
  }
  $("other-wrap").hidden = others.childElementCount === 0;
}

let modelKey = "";

/** The model selector. Rebuilt only when the option list changes, so a poll
 * never closes a dropdown the streamer has open. */
function renderModel(): void {
  const view = modelView(state);
  $("model-section").hidden = view === null;
  if (view === null) return;
  const select = $<HTMLSelectElement>("model-select");
  const key = view.options.map((o) => o.alias).join("|");
  if (key !== modelKey) {
    modelKey = key;
    select.replaceChildren(
      ...view.options.map((o) => {
        const option = document.createElement("option");
        option.value = o.alias;
        option.textContent = o.label;
        return option;
      }),
    );
  }
  if (document.activeElement !== select || view.disabled) select.value = view.selected;
  select.disabled = view.disabled;
  $<HTMLButtonElement>("model-apply").disabled = view.disabled || connectionLost;
  const message = $("model-state");
  message.textContent = view.message;
  message.className = view.health === "warn" ? "error" : "";
}

async function selectModel(): Promise<void> {
  const alias = $<HTMLSelectElement>("model-select").value;
  if (state?.status.thinking_model?.active === alias) return;
  $<HTMLButtonElement>("model-apply").disabled = true;
  try {
    const { status, json } = await api("/api/model", { method: "POST", body: JSON.stringify({ model: alias }) });
    const body = (json ?? {}) as { ok?: boolean; detail?: string };
    $("model-state").textContent =
      status === 200 && body.ok ? `${body.detail}` : `切り替えできませんでした: ${body.detail ?? status}`;
  } catch {
    $("model-state").textContent = "切り替えできませんでした(アプリに接続できません)";
  } finally {
    void poll();
  }
}

async function poll(): Promise<void> {
  try {
    const { status, json } = await api("/api/state");
    if (status === 401) {
      // The secret is per launch, so a 401 never heals: this page belongs to an
      // earlier run. Stop polling instead of filling the app log with refusals.
      clearInterval(pollTimer);
      connectionLost = true;
      setBusy(true);
      connection = "トークンが無効です。aivtuber-setup start が表示したURLを開き直してください";
    } else if (status === 200) {
      const next = parseState(json);
      if (next === null) {
        connection = "この画面とバージョンが合いません(ページを再読み込みしてください)";
      } else {
        state = next;
        connection = "接続中";
      }
    } else {
      connection = `応答エラー (${status})`;
    }
  } catch {
    connection = "アプリに接続できません(終了した可能性があります)";
  }
  render();
}

function setBusy(busy: boolean): void {
  for (const id of ["send", "stop", "mute"]) $<HTMLButtonElement>(id).disabled = busy;
}

async function send(): Promise<void> {
  const input = $<HTMLTextAreaElement>("chat-input");
  const checked = checkChat(input.value);
  if (!checked.ok) {
    $("chat-error").textContent = checked.reason;
    return;
  }
  $("chat-error").textContent = "";
  setBusy(true);
  try {
    const { status, json } = await api("/api/chat", { method: "POST", body: JSON.stringify({ text: checked.text }) });
    const body = (json ?? {}) as { ok?: boolean; detail?: string; id?: string | null };
    if (status === 200 && body.ok) {
      chat = pushChat(chat, { id: body.id ?? null, text: checked.text, note: "返答を待っています…" });
      input.value = "";
    } else {
      $("chat-error").textContent = `送信できませんでした: ${body.detail ?? status}`;
    }
  } catch {
    $("chat-error").textContent = "送信できませんでした(アプリに接続できません)";
  } finally {
    setBusy(false);
    render();
  }
}

async function control(action: "stop" | "mute"): Promise<void> {
  if (action === "mute" && !window.confirm("ミュートすると、アプリを再起動するまで解除できません。ミュートしますか?")) return;
  setBusy(true);
  try {
    const { status, json } = await api(`/api/${action}`, { method: "POST", body: "{}" });
    const body = (json ?? {}) as { ok?: boolean; detail?: string };
    $("control-result").textContent =
      status === 200 && body.ok ? `${action === "stop" ? "停止" : "ミュート"}しました: ${body.detail}` : `失敗しました: ${body.detail ?? status}`;
  } catch {
    $("control-result").textContent = "失敗しました(アプリに接続できません)";
  } finally {
    setBusy(false);
    void poll();
  }
}

$("send").addEventListener("click", () => void send());
$("chat-input").addEventListener("keydown", (event) => {
  const e = event as KeyboardEvent;
  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    void send();
  }
});
$("model-apply").addEventListener("click", () => void selectModel());
$("stop").addEventListener("click", () => void control("stop"));
$("mute").addEventListener("click", () => void control("mute"));

if (token === null) {
  connection = "トークンがありません。aivtuber-setup start が表示したURLをそのまま開いてください";
  connectionLost = true;
  setBusy(true);
  render();
} else {
  render();
  void poll();
  pollTimer = setInterval(() => void poll(), POLL_MS);
}
