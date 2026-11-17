// Local operation panel page (issue #335). Plain DOM, no framework. It talks
// to the same origin only, holds the per-launch secret in memory (received once
// in the URL fragment), and renders every server string with textContent.
import {
  type ChatEntry,
  type ChatPlatform,
  type ChatPlatformView,
  type PanelState,
  type TuningKey,
  type VoiceModel,
  type VoiceSearchPage,
  TUNING_RANGES,
  avatarView,
  checkChat,
  checkVrmFile,
  type ConnectionKind,
  avatarLinkState,
  connectionText,
  controlResultText,
  chatIngressView,
  connectBody,
  checkCredit,
  checkKey,
  checkKeyword,
  clampTuning,
  creditFor,
  defaultSelection,
  enablePatch,
  modelView,
  usageView,
  personaGetBody,
  personaResultText,
  personaSetBody,
  personaTextFrom,
  personaView,
  seedGetBody,
  seedResultText,
  seedSetBody,
  seedTextFrom,
  seedView,
  parseSeedDocument,
  SEED_KINDS,
  type SeedDocument,
  type SeedEntry,
  type SeedKind,
  voiceInputBody,
  voiceInputChoiceResultText,
  voiceInputDeviceBody,
  voiceInputDuplexBody,
  voiceInputResultText,
  voiceInputView,
  otherReplies,
  parseSearchPage,
  parseState,
  pushChat,
  repliesFor,
  safeAvatarUrl,
  searchPageCount,
  shouldReloadForHash,
  statusRows,
  tokenFromHash,
  tuningPatch,
  voiceView,
} from "./panel-core";

const POLL_MS = 1000;
const REQUEST_TIMEOUT_MS = 6000;

// The secret lives only in this variable: not in storage, not in a cookie, and
// the fragment is removed from the address bar right away.
const token = tokenFromHash(location.hash);
history.replaceState(null, "", location.pathname);

let state: PanelState | null = null;
let chat: ChatEntry[] = [];
let connection: ConnectionKind = { kind: "connecting" };
let focusedChat = false;
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

// A settings section that reports a problem or waits for a decision opens once;
// after that the operator's own open/close choice is respected.
const autoOpened = new Set<string>();
function openForAttention(): void {
  const needs: [string, string[]][] = [
    ["avatar-section", ["avatar-error", "avatar-mismatch"]],
    ["voice-section", ["voice-error"]],
    ["ingress-section", ["ingress-error"]],
  ];
  for (const [id, errors] of needs) {
    const section = $<HTMLDetailsElement>(id);
    const attention = errors.some((e) => $(e).textContent !== "") || (id === "avatar-section" && !$("avatar-pending").hidden);
    if (attention && !section.hidden && !autoOpened.has(id)) {
      autoOpened.add(id);
      section.open = true;
    }
  }
}

function render(): void {
  const conn = $("connection");
  conn.textContent = connectionText(connection);
  conn.className = `conn ${connection.kind === "connected" ? "ok" : connection.kind === "connecting" || connection.kind === "reconnecting" || connection.kind === "http-error" ? "warn" : "bad"}`;
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
      value.className = "value";
      value.textContent = row.value;
      if (row.raw !== undefined) value.title = row.raw;
      item.append(label, value);
      rows.append(item);
    }
  }
  renderModel();
  renderUsage();
  renderVoiceInput();
  renderPersona();
  renderMemory();
  renderVoice();
  renderIngress();
  renderAvatar();
  const link = $<HTMLAnchorElement>("avatar-link");
  const url = safeAvatarUrl(state?.avatar_url);
  // Renderer not connected yet: the first step gets a prominent button near the top.
  const needsAvatar = url !== null && avatarLinkState(state?.status.avatar_error) === "disconnected";
  link.hidden = url === null || needsAvatar;
  if (url !== null) link.href = url;
  const cta = $<HTMLAnchorElement>("avatar-cta");
  $("avatar-cta-wrap").hidden = !needsAvatar;
  if (url !== null) cta.href = url;
  openForAttention();
  if (!focusedChat && connection.kind === "connected") {
    // First connected render: the chat box is the first thing a new user needs.
    focusedChat = true;
    $("chat-input").focus();
  }

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

/** Voice input (#400): hidden unless the app composed live switching. Every
 * string, the host's failure reason included, goes in with textContent. */
function renderVoiceInput(): void {
  const view = voiceInputView(state);
  $("voice-input-section").hidden = view === null;
  if (view === null) return;
  const stateLine = $("voice-input-state");
  stateLine.textContent = view.stateText;
  stateLine.className = view.health === "warn" ? "error" : "";
  $("voice-input-explanation").textContent = view.explanation;
  const cloud = $("voice-input-cloud");
  cloud.textContent = view.cloudNote ?? "";
  cloud.hidden = view.cloudNote === null;
  // #396: the open device, the dropdown and the headset switch. Names are
  // host text (operator-local): textContent / option text only, never HTML.
  $("voice-input-device-line").textContent = view.deviceText;
  const select = $<HTMLSelectElement>("voice-input-device");
  const wanted = JSON.stringify([view.deviceOptions, view.deviceSelected]);
  if (select.dataset.shown !== wanted) {
    select.dataset.shown = wanted;
    select.replaceChildren(
      ...view.deviceOptions.map((option) => {
        const el = document.createElement("option");
        el.value = option.value;
        el.textContent = option.label;
        return el;
      }),
    );
    select.value = view.deviceSelected;
  }
  select.disabled = connectionLost || voiceInputBusy;
  const headset = $<HTMLInputElement>("voice-input-headset");
  headset.checked = view.headset;
  headset.disabled = connectionLost || voiceInputBusy;
  $("voice-input-headset-label").textContent = view.headsetLabel;
  $("voice-input-headset-hint").textContent = view.headsetHint;
  const dropped = $("voice-input-dropped");
  dropped.textContent = view.droppedText ?? "";
  dropped.hidden = view.droppedText === null;
  const enable = $<HTMLButtonElement>("voice-input-on");
  enable.textContent = view.enableLabel;
  enable.hidden = !view.canEnable;
  enable.disabled = connectionLost || voiceInputBusy;
  const disable = $<HTMLButtonElement>("voice-input-off");
  disable.hidden = !view.canDisable;
  disable.disabled = connectionLost || voiceInputBusy;
}

let voiceInputBusy = false;

async function setVoiceInput(enabled: boolean): Promise<void> {
  voiceInputBusy = true;
  renderVoiceInput();
  const out = $("voice-input-result");
  try {
    const { status, json } = await api("/api/speech-input", { method: "POST", body: JSON.stringify(voiceInputBody(enabled)) });
    const body = (json ?? {}) as { ok?: boolean; detail?: string };
    out.textContent = voiceInputResultText(enabled, status === 200 && body.ok === true, body.detail, status);
  } catch {
    out.textContent = "失敗しました(アプリに接続できません)";
  } finally {
    voiceInputBusy = false;
    void poll();
  }
}

async function setVoiceInputChoice(path: string, body: unknown, what: "マイク" | "ヘッドセット設定"): Promise<void> {
  voiceInputBusy = true;
  renderVoiceInput();
  const out = $("voice-input-result");
  try {
    const { status, json } = await api(path, { method: "POST", body: JSON.stringify(body) });
    const reply = (json ?? {}) as { ok?: boolean; detail?: string };
    out.textContent = voiceInputChoiceResultText(what, status === 200 && reply.ok === true, reply.detail, status);
  } catch {
    out.textContent = "失敗しました(アプリに接続できません)";
  } finally {
    voiceInputBusy = false;
    void poll();
  }
}

/** Character sheet (#401): hidden unless the app composed live editing. The text
 * is loaded from the host once when the card first shows and again on "reload";
 * a poll never overwrites what is being typed. Every string goes in with
 * textContent or `.value`, never as HTML. */
let personaLoaded = false;
let personaTried = false;
let personaBusy = false;

function renderPersona(): void {
  const field = $<HTMLTextAreaElement>("persona-text");
  const view = personaView(state, field.value);
  $("persona-section").hidden = view === null;
  if (view === null) {
    personaLoaded = false;
    personaTried = false;
    return;
  }
  if (!personaLoaded && !personaBusy && !personaTried) {
    personaTried = true;
    void loadPersona();
  }
  $("persona-explanation").textContent = view.explanation;
  const cloud = $("persona-cloud");
  cloud.textContent = view.cloudNote ?? "";
  cloud.hidden = view.cloudNote === null;
  const memoryOnly = $("persona-memory-only");
  memoryOnly.textContent = view.memoryOnlyNote ?? "";
  memoryOnly.hidden = view.memoryOnlyNote === null;
  const counter = $("persona-counter");
  counter.textContent = view.counterText;
  counter.className = view.health === "warn" ? "error" : "hint";
  $<HTMLButtonElement>("persona-save").disabled = connectionLost || personaBusy || !view.canSave || !personaLoaded;
  $<HTMLButtonElement>("persona-reload").disabled = connectionLost || personaBusy;
}

async function loadPersona(): Promise<void> {
  personaBusy = true;
  const out = $("persona-result");
  try {
    const { status, json } = await api("/api/persona/get", { method: "POST", body: JSON.stringify(personaGetBody()) });
    const body = (json ?? {}) as { ok?: boolean; detail?: string };
    const text = personaTextFrom(json);
    if (status === 200 && body.ok === true && text !== null) {
      $<HTMLTextAreaElement>("persona-text").value = text;
      personaLoaded = true;
      out.textContent = "";
    } else {
      out.textContent = personaResultText("reload", false, body.detail, status);
    }
  } catch {
    out.textContent = "読み込めませんでした(アプリに接続できません)";
  } finally {
    personaBusy = false;
    renderPersona();
  }
}

async function savePersona(): Promise<void> {
  personaBusy = true;
  renderPersona();
  const out = $("persona-result");
  try {
    const text = $<HTMLTextAreaElement>("persona-text").value;
    const { status, json } = await api("/api/persona/set", { method: "POST", body: JSON.stringify(personaSetBody(text)) });
    const body = (json ?? {}) as { ok?: boolean; detail?: string };
    out.textContent = personaResultText("save", status === 200 && body.ok === true, body.detail, status);
  } catch {
    out.textContent = "保存できませんでした(アプリに接続できません)";
  } finally {
    personaBusy = false;
    void poll();
    renderPersona();
  }
}

/** Memory seed (#402): hidden unless the app composed live editing. The document is
 * loaded from the host once when the card first shows and again on "reload"; a
 * poll never rebuilds the rows being typed in. Save sends the whole document.
 * Every string goes in with textContent or `.value`, never as HTML. */
let memoryLoaded = false;
let memoryTried = false;
let memoryBusy = false;
/** The document's identity fields are kept as the host sent them. */
let memoryDocument: Pick<SeedDocument, "seed_id" | "revision"> = { seed_id: "local-memory-seed", revision: "1" };
let memoryEntries: SeedEntry[] = [];

function renderMemory(): void {
  const view = seedView(state, memoryEntries);
  $("memory-section").hidden = view === null;
  if (view === null) {
    memoryLoaded = false;
    memoryTried = false;
    return;
  }
  if (!memoryLoaded && !memoryBusy && !memoryTried) {
    memoryTried = true;
    void loadMemory();
  }
  $("memory-explanation").textContent = view.explanation;
  const cloud = $("memory-cloud");
  cloud.textContent = view.cloudNote ?? "";
  cloud.hidden = view.cloudNote === null;
  const memoryOnly = $("memory-memory-only");
  memoryOnly.textContent = view.memoryOnlyNote ?? "";
  memoryOnly.hidden = view.memoryOnlyNote === null;
  const count = $("memory-count");
  count.textContent = view.countText;
  count.className = view.health === "warn" ? "error" : "hint";
  // Counters and problems follow the rows without rebuilding them.
  const items = $("memory-list").children;
  view.rows.forEach((row, index) => {
    const item = items[index];
    if (!item) return;
    const bytes = item.querySelector(".seed-bytes");
    if (bytes) {
      bytes.textContent = row.bytesText;
      bytes.className = row.health === "warn" ? "seed-bytes error" : "seed-bytes hint";
    }
    const problem = item.querySelector<HTMLElement>(".seed-problem");
    if (problem) {
      problem.textContent = row.problem ?? "";
      problem.hidden = row.problem === null;
    }
  });
  $<HTMLButtonElement>("memory-add").disabled = connectionLost || memoryBusy || !memoryLoaded || !view.canAdd;
  $<HTMLButtonElement>("memory-save").disabled = connectionLost || memoryBusy || !memoryLoaded || !view.canSave;
  $<HTMLButtonElement>("memory-reload").disabled = connectionLost || memoryBusy;
}

/** Rebuild the rows from `memoryEntries` (after a load, an add or a delete). */
function buildMemoryRows(): void {
  const list = $("memory-list");
  list.replaceChildren();
  memoryEntries.forEach((entry, index) => {
    const li = document.createElement("li");
    li.className = "seed-row";
    const kind = document.createElement("select");
    kind.setAttribute("aria-label", `項目 ${index + 1} の種類`);
    for (const { kind: value, label } of SEED_KINDS) {
      const option = document.createElement("option");
      option.value = value;
      option.textContent = label;
      kind.append(option);
    }
    kind.value = entry.kind;
    kind.addEventListener("change", () => {
      entry.kind = kind.value as SeedKind;
      renderMemory();
    });
    const claim = document.createElement("input");
    claim.type = "text";
    claim.className = "seed-claim";
    claim.spellcheck = false;
    claim.setAttribute("aria-label", `項目 ${index + 1} の内容`);
    claim.value = entry.claim;
    claim.addEventListener("input", () => {
      entry.claim = claim.value;
      renderMemory();
    });
    const bytes = document.createElement("span");
    bytes.className = "seed-bytes hint";
    const remove = document.createElement("button");
    remove.type = "button";
    remove.textContent = "削除";
    remove.setAttribute("aria-label", `項目 ${index + 1} を削除`);
    remove.addEventListener("click", () => {
      memoryEntries.splice(index, 1);
      buildMemoryRows();
      renderMemory();
    });
    const problem = document.createElement("p");
    problem.className = "seed-problem error";
    problem.hidden = true;
    li.append(kind, claim, bytes, remove, problem);
    list.append(li);
  });
}

async function loadMemory(): Promise<void> {
  memoryBusy = true;
  const out = $("memory-result");
  try {
    const { status, json } = await api("/api/memory-seed/get", { method: "POST", body: JSON.stringify(seedGetBody()) });
    const body = (json ?? {}) as { ok?: boolean; detail?: string };
    const loaded = parseSeedDocument(seedTextFrom(json));
    if (status === 200 && body.ok === true && loaded !== null) {
      memoryDocument = { seed_id: loaded.seed_id, revision: loaded.revision };
      memoryEntries = loaded.entries;
      memoryLoaded = true;
      buildMemoryRows();
      out.textContent = "";
    } else {
      out.textContent = seedResultText("reload", false, body.detail, status);
    }
  } catch {
    out.textContent = "読み込めませんでした(アプリに接続できません)";
  } finally {
    memoryBusy = false;
    renderMemory();
  }
}

async function saveMemory(): Promise<void> {
  memoryBusy = true;
  renderMemory();
  const out = $("memory-result");
  try {
    const { status, json } = await api("/api/memory-seed/set", {
      method: "POST",
      body: JSON.stringify(seedSetBody(memoryDocument, memoryEntries)),
    });
    const body = (json ?? {}) as { ok?: boolean; detail?: string };
    out.textContent = seedResultText("save", status === 200 && body.ok === true, body.detail, status);
  } catch {
    out.textContent = "保存できませんでした(アプリに接続できません)";
  } finally {
    memoryBusy = false;
    void poll();
    renderMemory();
  }
}

/** Session Thinking tokens/cost and the spend cap (#367). */
function renderUsage(): void {
  const view = usageView(state);
  $("usage-section").hidden = view === null;
  if (view === null) return;
  $("usage-summary").textContent = view.summary;
  const cap = $("usage-cap");
  cap.textContent = view.cap;
  cap.hidden = view.cap === "";
  cap.className = view.health === "warn" ? "error" : "hint";
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

// ---- VRM avatar (#362) ----

const UPLOAD_TIMEOUT_MS = 120_000;
/** The avatar page needs a few seconds to notice a switch; a longer difference is a failure. */
const MISMATCH_GRACE_MS = 8000;
let mismatchSince: number | null = null;

function fillRows(list: HTMLElement, rows: [string, string][]): void {
  list.replaceChildren();
  for (const [label, value] of rows) {
    const term = document.createElement("dt");
    term.textContent = label;
    const detail = document.createElement("dd");
    detail.textContent = value;
    list.append(term, detail);
  }
}

function fillWarnings(list: HTMLElement, warnings: string[]): void {
  list.replaceChildren();
  for (const text of warnings) {
    const item = document.createElement("li");
    item.textContent = text;
    list.append(item);
  }
}

function renderAvatar(): void {
  const view = avatarView(state);
  $("avatar-section").hidden = view === null;
  if (view === null) return;
  const summary = $("avatar-summary");
  summary.textContent = view.summary;
  summary.className = view.health === "warn" ? "error" : "";
  $("avatar-credit").textContent = view.credit === null ? "" : `クレジット表記(配信の概要欄などに表示してください): ${view.credit}`;
  $("avatar-error").textContent = view.error ?? "";
  if (view.mismatch) {
    mismatchSince ??= Date.now();
  } else {
    mismatchSince = null;
  }
  $("avatar-mismatch").textContent =
    mismatchSince !== null && Date.now() - mismatchSince > MISMATCH_GRACE_MS
      ? "アバター画面が、選択中のモデルを読み込めずに別のモデルを表示しています。モデルファイルが壊れているか、対応していない可能性があります"
      : "";
  $<HTMLButtonElement>("avatar-choose").disabled = connectionLost;

  const pending = $("avatar-pending");
  pending.hidden = view.pending === null;
  if (view.pending !== null) {
    $("avatar-pending-name").textContent = view.pending.title;
    fillWarnings($("avatar-pending-warnings"), view.pending.warnings);
    fillRows($("avatar-pending-rows"), view.pending.rows);
    $("avatar-pending-credit").textContent = view.pending.credit === null ? "" : `クレジット表記: ${view.pending.credit}`;
    $<HTMLButtonElement>("avatar-confirm").disabled = connectionLost;
    $<HTMLButtonElement>("avatar-discard").disabled = connectionLost;
  }

  const list = $("avatar-models");
  list.replaceChildren();
  for (const choice of view.choices) {
    const item = document.createElement("li");
    item.className = `model-row${choice.active ? " active" : ""}`;
    const label = document.createElement("span");
    label.textContent = `${choice.active ? "● " : ""}${choice.label}`;
    for (const warning of choice.warnings) {
      const small = document.createElement("small");
      small.textContent = warning;
      label.append(small);
    }
    const use = document.createElement("button");
    use.type = "button";
    use.textContent = choice.active ? "使用中" : "使う";
    use.disabled = choice.active || connectionLost;
    use.addEventListener("click", () => void avatarPost("/api/avatar/select", { id: choice.id }));
    const remove = document.createElement("button");
    remove.type = "button";
    remove.textContent = "削除";
    remove.hidden = choice.id === "sample";
    remove.disabled = !choice.deletable || connectionLost;
    remove.addEventListener("click", () => {
      if (window.confirm("このモデルを保存先から削除します。よろしいですか?")) {
        void avatarPost("/api/avatar/delete", { id: choice.id });
      }
    });
    item.append(label, use, remove);
    list.append(item);
  }
  const max = state?.status.avatar_model?.max_models ?? 0;
  $("avatar-models-hint").textContent = `追加したモデルは最大 ${max} 個まで保存されます。超えると、最も長く使っていないものが自動で消えます(使用中のものは消えません)。`;
  fillWarnings($("avatar-active-warnings"), view.activeWarnings);
  fillRows($("avatar-active-rows"), view.activeRows);
}

async function avatarPost(path: string, body: unknown): Promise<boolean> {
  const out = $("avatar-result");
  try {
    const { status, json } = await api(path, { method: "POST", body: JSON.stringify(body) });
    const reply = (json ?? {}) as { ok?: boolean; detail?: string };
    const ok = status === 200 && reply.ok === true;
    out.textContent = ok ? (reply.detail ?? "") : `できませんでした: ${reply.detail ?? status}`;
    out.className = ok ? "" : "error";
    return ok;
  } catch {
    out.textContent = "できませんでした(アプリに接続できません)";
    out.className = "error";
    return false;
  } finally {
    void poll();
  }
}

/** Sends the chosen file's raw bytes. The page never reads it into memory whole. */
async function uploadAvatar(file: File): Promise<void> {
  const out = $("avatar-upload-state");
  const refuse = (text: string): void => {
    out.textContent = text;
    out.className = "error";
  };
  if (token === null) return refuse("トークンがありません");
  const head = new Uint8Array(await file.slice(0, 12).arrayBuffer());
  const checked = checkVrmFile(file, head);
  if (!checked.ok) return refuse(checked.reason);
  out.textContent = "アップロードして検査しています…";
  out.className = "";
  $<HTMLButtonElement>("avatar-choose").disabled = true;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), UPLOAD_TIMEOUT_MS);
  try {
    const response = await fetch("/api/avatar/upload", {
      method: "POST",
      headers: { "X-Panel-Token": token, "Content-Type": "application/octet-stream" },
      body: file,
      cache: "no-store",
      credentials: "omit",
      referrerPolicy: "no-referrer",
      signal: controller.signal,
    });
    let reply: { ok?: boolean; detail?: string } = {};
    try {
      reply = (await response.json()) as typeof reply;
    } catch {
      // an empty body (401, 403) carries no JSON
    }
    if (response.status === 200 && reply.ok === true) {
      out.textContent = "検査に通りました。下のライセンスを確認してください";
      out.className = "";
    } else {
      refuse(`受け付けられませんでした: ${reply.detail ?? response.status}`);
    }
  } catch {
    refuse("アップロードできませんでした(アプリに接続できないか、時間がかかりすぎました)");
  } finally {
    clearTimeout(timer);
    $<HTMLButtonElement>("avatar-choose").disabled = connectionLost;
    void poll();
  }
}

function confirmAvatar(): void {
  const pending = avatarView(state)?.pending ?? null;
  if (pending === null) return;
  // A licence that may not allow the use asks once more: the operator decides.
  if (pending.restrictive && !window.confirm(`${pending.warnings.join("\n")}\n\n${pending.question}`)) return;
  void avatarPost("/api/avatar/confirm", { id: pending.id });
}

$("avatar-choose").addEventListener("click", () => $<HTMLInputElement>("avatar-file").click());
$("avatar-file").addEventListener("change", () => {
  const input = $<HTMLInputElement>("avatar-file");
  const file = input.files?.[0];
  input.value = "";
  if (file !== undefined) void uploadAvatar(file);
});
$("avatar-confirm").addEventListener("click", confirmAvatar);
$("avatar-discard").addEventListener("click", () => void avatarPost("/api/avatar/discard", {}));

// ---- stream chat connections (#361) ----

const PLATFORMS: ChatPlatform[] = ["twitch", "youtube"];

function renderIngressPlatform(platform: ChatPlatform, view: ChatPlatformView): void {
  const line = $(`ingress-${platform}-state`);
  line.textContent = `${view.label}: ${view.stateText}${view.detail === "" ? "" : ` — ${view.detail}`}${view.error === null ? "" : `(${view.error})`}${view.blocked === null ? "" : ` / ${view.blocked}`}`;
  line.className = view.health === "warn" || view.error !== null ? "error" : "";
  const input = $<HTMLInputElement>(`ingress-${platform}-target`);
  // The last choice is offered, never connected by itself; a field the
  // streamer has typed in (or is typing in) is left alone.
  if (input.value === "" && document.activeElement !== input && view.prefill !== "") input.value = view.prefill;
  $<HTMLButtonElement>(`ingress-${platform}-connect`).disabled = !view.canConnect || connectionLost;
  $<HTMLButtonElement>(`ingress-${platform}-disconnect`).disabled = !view.canDisconnect || connectionLost;
  $<HTMLButtonElement>(`ingress-${platform}-connect`).title = view.blocked ?? "";
}

function renderIngress(): void {
  const view = chatIngressView(state);
  $("ingress-section").hidden = view === null;
  if (view === null) return;
  $("ingress-error").textContent = view.saveError === null ? "" : `選択を保存できませんでした: ${view.saveError}`;
  renderIngressPlatform("twitch", view.twitch);
  renderIngressPlatform("youtube", view.youtube);
  $("ingress-key-state").textContent = `キー: ${view.keyText}`;
  $<HTMLButtonElement>("ingress-key-save").disabled = connectionLost;
  $<HTMLButtonElement>("ingress-key-remove").disabled = !(state?.status.chat_ingress?.youtube_key.set ?? false) || connectionLost;
}

async function ingressPost(platform: ChatPlatform | "key", path: string, body: unknown): Promise<boolean> {
  const out = $(`ingress-${platform === "key" ? "youtube" : platform}-result`);
  try {
    const { status, json } = await api(path, { method: "POST", body: JSON.stringify(body) });
    const reply = (json ?? {}) as { ok?: boolean; detail?: string };
    const ok = status === 200 && reply.ok === true;
    out.textContent = ok ? (reply.detail ?? "") : `できませんでした: ${reply.detail ?? status}`;
    out.className = ok ? "" : "error";
    return ok;
  } catch {
    out.textContent = "できませんでした(アプリに接続できません)";
    out.className = "error";
    return false;
  } finally {
    void poll();
  }
}

function connectPlatform(platform: ChatPlatform): void {
  const checked = connectBody(platform, $<HTMLInputElement>(`ingress-${platform}-target`).value);
  if (!checked.ok) {
    const out = $(`ingress-${platform}-result`);
    out.textContent = checked.reason;
    out.className = "error";
    return;
  }
  void ingressPost(platform, "/api/ingress/connect", checked.body);
}

async function saveIngressKey(): Promise<void> {
  const input = $<HTMLInputElement>("ingress-key");
  const checked = checkKey(input.value);
  if (!checked.ok) {
    const out = $("ingress-youtube-result");
    out.textContent = checked.reason;
    out.className = "error";
    return;
  }
  // The field is emptied before the request is answered and whatever the
  // answer: the page keeps no copy of the key.
  input.value = "";
  await ingressPost("key", "/api/ingress/key", { key: checked.key });
}

for (const platform of PLATFORMS) {
  $(`ingress-${platform}-connect`).addEventListener("click", () => connectPlatform(platform));
  $(`ingress-${platform}-target`).addEventListener("keydown", (event) => {
    if ((event as KeyboardEvent).key === "Enter") connectPlatform(platform);
  });
  $(`ingress-${platform}-disconnect`).addEventListener("click", () => void ingressPost(platform, "/api/ingress/disconnect", { platform }));
}
$("ingress-key-save").addEventListener("click", () => void saveIngressKey());
$("ingress-key-remove").addEventListener("click", () => {
  if (window.confirm("保存したYouTube APIキーを削除します。YouTubeに接続中の場合は切断されます。よろしいですか?")) {
    void ingressPost("key", "/api/ingress/key/remove", {});
  }
});

// ---- voice settings (#360) ----

const TUNING_KEYS: TuningKey[] = ["speaking_rate", "pitch", "volume", "emotional_intensity"];
let results: VoiceSearchPage | null = null;
let selected: VoiceModel | null = null;
let lastSearch = { keyword: "", sort: "download", page: 1 };
let searching = false;

/** The voice section. The sliders and selects are not touched while the
 * streamer is using them, so a poll never fights a drag or an open dropdown. */
function renderVoice(): void {
  const view = voiceView(state);
  $("voice-section").hidden = view === null;
  const voice = state?.status.voice ?? null;
  if (view === null || voice === null) return;
  const summary = $("voice-summary");
  summary.textContent = view.summary;
  summary.className = view.health === "warn" ? "error" : "";
  $("voice-error").textContent = view.error ?? "";
  const keyState = $("voice-key-state");
  keyState.textContent = `キー: ${view.keyText}`;
  keyState.className = view.keyHealth === "warn" ? "error" : "";
  $<HTMLButtonElement>("voice-key-check").disabled = !voice.key.set || connectionLost;
  $<HTMLButtonElement>("voice-key-remove").disabled = !voice.key.set || connectionLost;
  $<HTMLButtonElement>("voice-key-save").disabled = connectionLost;
  $<HTMLButtonElement>("voice-enable").disabled = selected === null || !view.canEnable || connectionLost;
  $<HTMLButtonElement>("voice-local").disabled = voice.backend === "local" || connectionLost;
  $<HTMLButtonElement>("voice-preview").disabled = connectionLost;
  for (const key of TUNING_KEYS) {
    const input = $<HTMLInputElement>(`tune-${key}`);
    if (document.activeElement !== input) input.value = String(voice.tuning[key]);
    $(`tune-${key}-out`).textContent = Number(input.value).toFixed(2);
  }
  // A model already chosen (from a previous run) is shown even before a search.
  if (selected === null && voice.model_uuid !== null && voice.model_name !== null) {
    showChoicePlaceholder(voice.model_name);
  }
}

function showChoicePlaceholder(name: string): void {
  if (!$("voice-choice").hidden) return;
  $("voice-choice-name").textContent = `選択中のモデル: ${name}(別のモデルは検索して選べます)`;
  $("voice-choice").hidden = false;
  for (const id of ["voice-speaker", "voice-style", "voice-enable"]) $<HTMLButtonElement>(id).disabled = true;
  const credit = state?.status.voice?.credit;
  if (credit) $<HTMLInputElement>("voice-credit").value = credit;
}

function renderResults(): void {
  const list = $("voice-results");
  list.replaceChildren();
  const hasPrev = results !== null && results.page > 1;
  const hasNext = results !== null && results.page < searchPageCount(results.total);
  $<HTMLButtonElement>("voice-prev").hidden = !hasPrev;
  $<HTMLButtonElement>("voice-next").hidden = !hasNext;
  if (results === null) return;
  for (const model of results.models) {
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = model.name;
    const small = document.createElement("small");
    small.textContent = ` ${model.licenses.join(" / ")}${model.voice_timbre ? ` · ${model.voice_timbre}` : ""}`;
    button.append(small);
    button.addEventListener("click", () => chooseModel(model));
    item.append(button);
    list.append(item);
  }
  $("voice-search-state").textContent =
    results.models.length === 0 ? "見つかりませんでした" : `${results.total}件中 ${results.page}/${searchPageCount(results.total)}ページ`;
}

function fillSelect(select: HTMLSelectElement, options: { value: string; label: string }[], chosen: string | null): void {
  select.replaceChildren(
    ...options.map((o) => {
      const option = document.createElement("option");
      option.value = o.value;
      option.textContent = o.label;
      return option;
    }),
  );
  if (chosen !== null && options.some((o) => o.value === chosen)) select.value = chosen;
  select.disabled = options.length === 0;
}

function speakerOf(model: VoiceModel): string | null {
  const value = $<HTMLSelectElement>("voice-speaker").value;
  return model.speakers.some((sp) => sp.uuid === value) ? value : (model.speakers[0]?.uuid ?? null);
}

function fillStyles(model: VoiceModel, speakerUuid: string | null, styleId: number | null): void {
  const speaker = model.speakers.find((sp) => sp.uuid === speakerUuid);
  fillSelect(
    $<HTMLSelectElement>("voice-style"),
    (speaker?.styles ?? []).map((st) => ({ value: String(st.style_id), label: `${st.name} (${st.style_id})` })),
    styleId === null ? null : String(styleId),
  );
}

/** A model picked from the search results. Everything shown comes from the
 * typed DTO and goes in through textContent. */
function chooseModel(model: VoiceModel): void {
  selected = model;
  const voice = state?.status.voice ?? null;
  const same = voice !== null && voice.model_uuid === model.uuid;
  const first = defaultSelection(model);
  const speakerUuid = same && voice.speaker_uuid !== null ? voice.speaker_uuid : first.speakerUuid;
  $("voice-choice-name").textContent = model.name;
  $("voice-choice-license").textContent = `ライセンス: ${model.licenses.join(" / ") || "不明"}${model.category ? `  分類: ${model.category}` : ""}`;
  $("voice-choice-description").textContent = model.description;
  fillSelect(
    $<HTMLSelectElement>("voice-speaker"),
    model.speakers.map((sp) => ({ value: sp.uuid, label: sp.name })),
    speakerUuid,
  );
  fillStyles(model, speakerUuid, same ? voice.style_id : first.styleId);
  $<HTMLInputElement>("voice-credit").value = same && voice.credit ? voice.credit : creditFor(model.name);
  $("voice-choice").hidden = false;
  render();
}

async function voicePost(path: string, body: unknown, done: (detail: string) => string): Promise<boolean> {
  const out = $("voice-result");
  try {
    const { status, json } = await api(path, { method: "POST", body: JSON.stringify(body) });
    const reply = (json ?? {}) as { ok?: boolean; detail?: string };
    const ok = status === 200 && reply.ok === true;
    out.textContent = ok ? done(reply.detail ?? "") : `できませんでした: ${reply.detail ?? status}`;
    out.className = ok ? "" : "error";
    return ok;
  } catch {
    out.textContent = "できませんでした(アプリに接続できません)";
    out.className = "error";
    return false;
  } finally {
    void poll();
  }
}

async function searchModels(page: number): Promise<void> {
  if (searching) return;
  const checked = checkKeyword($<HTMLInputElement>("voice-keyword").value);
  const status = $("voice-search-state");
  if (!checked.ok) {
    status.textContent = checked.reason;
    return;
  }
  searching = true;
  $<HTMLButtonElement>("voice-search").disabled = true;
  status.textContent = "検索中…";
  lastSearch = { keyword: checked.keyword, sort: $<HTMLSelectElement>("voice-sort").value, page };
  try {
    const { status: code, json } = await api("/api/voice/search", { method: "POST", body: JSON.stringify(lastSearch) });
    const parsed = code === 200 ? parseSearchPage(json) : null;
    if (parsed === null) {
      const reply = (json ?? {}) as { detail?: string };
      status.textContent = `検索できませんでした: ${reply.detail ?? code}`;
    } else {
      results = parsed;
      renderResults();
    }
  } catch {
    status.textContent = "検索できませんでした(アプリに接続できません)";
  } finally {
    searching = false;
    $<HTMLButtonElement>("voice-search").disabled = false;
  }
}

async function enableAivis(): Promise<void> {
  const voice = state?.status.voice ?? null;
  if (selected === null || voice === null) return;
  const credit = checkCredit($<HTMLInputElement>("voice-credit").value);
  if (!credit.ok) {
    $("voice-result").textContent = credit.reason;
    return;
  }
  const confirm = $<HTMLInputElement>("voice-egress").checked;
  if (voice.backend !== "aivis" && !confirm) {
    $("voice-result").textContent = "返信テキストが外部に送信されることの確認にチェックを入れてください";
    return;
  }
  const styleValue = $<HTMLSelectElement>("voice-style").value;
  const patch = enablePatch({
    model: selected,
    speakerUuid: speakerOf(selected),
    styleId: styleValue === "" ? null : Number(styleValue),
    credit: credit.text,
    confirmEgress: confirm || voice.backend === "aivis",
  });
  await voicePost("/api/voice", patch, () => "Aivis Cloud の声に切り替えました。次の発話から反映されます");
}

/** A speaker or style change on the voice that is already speaking applies at
 * once; on another model it waits for "この声を使う". */
async function liveStyleChange(): Promise<void> {
  const voice = state?.status.voice ?? null;
  if (selected === null || voice === null) return;
  const speakerUuid = speakerOf(selected);
  fillStyles(selected, speakerUuid, null);
  const styleValue = $<HTMLSelectElement>("voice-style").value;
  if (voice.backend !== "aivis" || voice.model_uuid !== selected.uuid) return;
  await voicePost(
    "/api/voice",
    { speaker_uuid: speakerUuid, style_id: styleValue === "" ? null : Number(styleValue) },
    () => "スタイルを変えました。次の発話から反映されます",
  );
}

async function liveStyleOnly(): Promise<void> {
  const voice = state?.status.voice ?? null;
  if (selected === null || voice === null) return;
  if (voice.backend !== "aivis" || voice.model_uuid !== selected.uuid) return;
  const styleValue = $<HTMLSelectElement>("voice-style").value;
  await voicePost("/api/voice", { style_id: styleValue === "" ? null : Number(styleValue) }, () => "スタイルを変えました。次の発話から反映されます");
}

async function saveKey(): Promise<void> {
  const input = $<HTMLInputElement>("voice-key");
  const checked = checkKey(input.value);
  if (!checked.ok) {
    $("voice-result").textContent = checked.reason;
    return;
  }
  // The field is emptied before the request is answered and whatever the
  // answer: the page keeps no copy of the key.
  input.value = "";
  await voicePost("/api/voice/key", { key: checked.key }, (detail) => `キーを保存しました(${detail})`);
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
      connection = { kind: "lost" };
    } else if (status === 200) {
      const next = parseState(json);
      if (next === null) {
        connection = { kind: "mismatch" };
      } else {
        state = next;
        connection = { kind: "connected" };
      }
    } else {
      connection = { kind: "http-error", status };
    }
  } catch {
    connection = { kind: "reconnecting" };
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
    const shown = controlResultText(action, status === 200 && body.ok === true, body.detail, status);
    const out = $("control-result");
    out.textContent = shown.text;
    out.title = shown.raw;
  } catch {
    $("control-result").textContent = "失敗しました(アプリに接続できません)";
    $("control-result").title = "";
  } finally {
    setBusy(false);
    void poll();
  }
}

// Pasting a new launch URL into this same tab only changes the fragment, so the
// browser would not reload and the old in-memory secret would stay. Reload so the
// one load path above reads the new token and scrubs the address bar again.
window.addEventListener("hashchange", () => {
  if (shouldReloadForHash(location.hash)) location.reload();
});

$("send").addEventListener("click", () => void send());
$("chat-input").addEventListener("keydown", (event) => {
  const e = event as KeyboardEvent;
  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    void send();
  }
});
$("model-apply").addEventListener("click", () => void selectModel());
$("voice-search").addEventListener("click", () => void searchModels(1));
$("voice-keyword").addEventListener("keydown", (event) => {
  if ((event as KeyboardEvent).key === "Enter") void searchModels(1);
});
$("voice-prev").addEventListener("click", () => void searchModels(Math.max(1, lastSearch.page - 1)));
$("voice-next").addEventListener("click", () => void searchModels(lastSearch.page + 1));
$("voice-speaker").addEventListener("change", () => void liveStyleChange());
$("voice-style").addEventListener("change", () => void liveStyleOnly());
$("voice-enable").addEventListener("click", () => void enableAivis());
$("voice-local").addEventListener("click", () => void voicePost("/api/voice", { backend: "local" }, () => "ローカルの声に戻しました。外部への送信は止まっています"));
$("voice-preview").addEventListener("click", () => void voicePost("/api/voice/preview", {}, () => "試し読みを再生します"));
$("voice-key-save").addEventListener("click", () => void saveKey());
$("voice-key-check").addEventListener("click", () => void voicePost("/api/voice/key/check", {}, () => "キーを確認しています"));
$("voice-key-remove").addEventListener("click", () => {
  if (window.confirm("保存したAPIキーを削除します。Aivis Cloud の声を使っている場合はローカルの声に戻ります。よろしいですか?")) {
    void voicePost("/api/voice/key/remove", {}, (detail) => detail);
  }
});
for (const key of TUNING_KEYS) {
  const input = $<HTMLInputElement>(`tune-${key}`);
  input.addEventListener("input", () => {
    $(`tune-${key}-out`).textContent = clampTuning(key, Number(input.value)).toFixed(2);
  });
  input.addEventListener("change", () => {
    const [low, high] = TUNING_RANGES[key];
    if (!(Number(input.value) >= low && Number(input.value) <= high)) return;
    void voicePost("/api/voice", tuningPatch(key, Number(input.value)), () => "調整しました。次の発話から反映されます");
  });
}
$("persona-text").addEventListener("input", () => renderPersona());
$("persona-save").addEventListener("click", () => void savePersona());
$("persona-reload").addEventListener("click", () => void loadPersona());
$("memory-add").addEventListener("click", () => {
  memoryEntries.push({ kind: "persona-fact", claim: "" });
  buildMemoryRows();
  renderMemory();
});
$("memory-save").addEventListener("click", () => void saveMemory());
$("memory-reload").addEventListener("click", () => void loadMemory());
$("voice-input-on").addEventListener("click", () => void setVoiceInput(true));
$("voice-input-off").addEventListener("click", () => void setVoiceInput(false));
$<HTMLSelectElement>("voice-input-device").addEventListener(
  "change",
  (event) =>
    void setVoiceInputChoice("/api/speech-input/device", voiceInputDeviceBody((event.target as HTMLSelectElement).value), "マイク"),
);
$<HTMLInputElement>("voice-input-headset").addEventListener(
  "change",
  (event) =>
    void setVoiceInputChoice("/api/speech-input/duplex", voiceInputDuplexBody((event.target as HTMLInputElement).checked), "ヘッドセット設定"),
);
$("stop").addEventListener("click", () => void control("stop"));
$("mute").addEventListener("click", () => void control("mute"));

if (token === null) {
  connection = { kind: "no-token" };
  connectionLost = true;
  setBusy(true);
  render();
} else {
  render();
  void poll();
  pollTimer = setInterval(() => void poll(), POLL_MS);
}
