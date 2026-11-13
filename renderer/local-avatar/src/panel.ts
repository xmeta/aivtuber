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
  checkChat,
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
  otherReplies,
  parseSearchPage,
  parseState,
  pushChat,
  repliesFor,
  safeAvatarUrl,
  searchPageCount,
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
  renderVoice();
  renderIngress();
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
