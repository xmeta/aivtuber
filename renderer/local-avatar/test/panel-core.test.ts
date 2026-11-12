// Unit tests for the operation panel's pure logic (issue #335).
import { describe, expect, test } from "bun:test";
import {
  MAX_CHAT_CHARS,
  MAX_CHAT_LOG,
  checkChat,
  otherReplies,
  parseState,
  pushChat,
  repliesFor,
  safeAvatarUrl,
  statusRows,
  tokenFromHash,
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
