// Unit tests for the renderer's protocol/state logic. No WebGL, no socket:
// the same code the page runs, driven by a stub transport. Frames use the
// shapes in crates/adapters/tests/fixtures/local-avatar-1/.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { AvatarState, SMOOTHING_TAU_S } from "../src/avatar-state";
import { Controller, MAX_PENDING_ACKS, backoffMs } from "../src/controller";
import {
  RELOAD_POLL_MS,
  activeIdUrl,
  activeModelUrl,
  modelIdFromHeader,
  shouldReload,
} from "../src/model-source";
import { PROFILE_ID } from "../src/profile";
import { decodeHostFrame, type RendererFrame } from "../src/protocol";

const HASH = "a".repeat(64);

function harness() {
  const sent: RendererFrame[] = [];
  const state = new AvatarState();
  let n = 0;
  const controller = new Controller({
    state,
    send: (f) => sent.push(f),
    newEpoch: () => `epoch-${++n}`,
    modelHash: HASH,
    profile: PROFILE_ID,
  });
  const host = (epoch: string, seq: number, cmd: unknown) =>
    controller.onMessage(JSON.stringify({ v: 1, epoch, seq, cmd }));
  return { sent, state, controller, host };
}

function ready(h: ReturnType<typeof harness>, epoch = "epoch-1") {
  h.controller.onOpen();
  h.controller.onFrameRendered();
  h.sent.length = 0;
  return epoch;
}

describe("handshake", () => {
  test("hello on open; ready only after a neutral frame rendered", () => {
    const h = harness();
    h.controller.onOpen();
    expect(h.sent).toEqual([
      { type: "hello", v: 1, epoch: "epoch-1", model_hash: HASH, profile: PROFILE_ID },
    ]);
    expect(h.controller.isReady).toBe(false);
    h.controller.onFrameRendered();
    expect(h.sent.at(-1)).toEqual({ type: "ready", epoch: "epoch-1" });
    expect(h.controller.isReady).toBe(true);
  });

  test("commands before ready are rejected not_ready and change nothing", () => {
    const h = harness();
    h.controller.onOpen();
    h.sent.length = 0;
    h.host("epoch-1", 1, { op: "expression", preset: "joy", intensity: 1 });
    expect(h.sent).toEqual([{ type: "reject", epoch: "epoch-1", seq: 1, reason: "not_ready" }]);
    expect(h.state.target.expressions.joy).toBe(0);
  });

  test("hello model_hash and profile satisfy the host's bounds", () => {
    expect(HASH).toMatch(/^[0-9a-f]{64}$/);
    expect(PROFILE_ID).toMatch(/^[A-Za-z0-9._@-]{1,64}$/);
  });
});

describe("epoch and seq", () => {
  test("ack only after the frame containing the command is rendered", () => {
    const h = harness();
    ready(h);
    h.host("epoch-1", 1, { op: "gaze", target: "chat" });
    expect(h.sent).toEqual([]);
    h.controller.onFrameRendered();
    expect(h.sent).toEqual([{ type: "ack", epoch: "epoch-1", seq: 1 }]);
    h.controller.onFrameRendered();
    expect(h.sent).toHaveLength(1);
  });

  test("foreign epoch is rejected stale_epoch and never moves last seq", () => {
    const h = harness();
    ready(h);
    h.host("epoch-0", 99, { op: "reset" });
    expect(h.sent).toEqual([{ type: "reject", epoch: "epoch-1", seq: 99, reason: "stale_epoch" }]);
    expect(h.controller.lastAcceptedSeq).toBe(0);
  });

  test("non-increasing seq is rejected stale_seq", () => {
    const h = harness();
    ready(h);
    h.host("epoch-1", 5, { op: "reset" });
    h.host("epoch-1", 5, { op: "reset" });
    h.host("epoch-1", 4, { op: "reset" });
    const rejects = h.sent.filter((f) => f.type === "reject");
    expect(rejects.map((f) => f.type === "reject" && f.reason)).toEqual(["stale_seq", "stale_seq"]);
    expect(h.controller.lastAcceptedSeq).toBe(5);
  });

  test("a new connection mints a new epoch, starts neutral and forgets old commands", () => {
    const h = harness();
    ready(h);
    h.host("epoch-1", 1, { op: "expression", preset: "joy", intensity: 1 });
    for (let i = 0; i < 10; i++) h.state.step(0.1);
    expect(h.state.current.expressions.joy).toBeGreaterThan(0.9);
    h.controller.onClose();
    expect(h.state.current.expressions.joy).toBe(0); // snap, not interpolation
    h.controller.onOpen();
    expect(h.controller.currentEpoch).toBe("epoch-2");
    h.sent.length = 0;
    h.controller.onFrameRendered(); // ready for epoch-2
    h.sent.length = 0;
    // the old epoch's command is refused and state stays neutral
    h.host("epoch-1", 2, { op: "expression", preset: "joy", intensity: 1 });
    expect(h.sent).toEqual([{ type: "reject", epoch: "epoch-2", seq: 2, reason: "stale_epoch" }]);
    expect(h.state.target.expressions.joy).toBe(0);
  });

  test("close drops unacked commands instead of acking them later", () => {
    const h = harness();
    ready(h);
    h.host("epoch-1", 1, { op: "gaze", target: "away" });
    h.controller.onClose();
    h.controller.onFrameRendered();
    expect(h.sent).toEqual([]);
    expect(h.state.current.yaw).toBe(0);
  });

  test("pending acks are bounded", () => {
    const h = harness();
    ready(h);
    for (let seq = 1; seq <= MAX_PENDING_ACKS + 3; seq++) h.host("epoch-1", seq, { op: "reset" });
    expect(h.controller.pendingAckCount).toBe(MAX_PENDING_ACKS);
    expect(h.sent.filter((f) => f.type === "reject")).toHaveLength(3);
  });
});

describe("command mapping", () => {
  test("unknown presets are rejected; raw VRM names do not leak through the wire", () => {
    const h = harness();
    ready(h);
    h.host("epoch-1", 1, { op: "expression", preset: "happy", intensity: 1 }); // raw VRM name
    h.host("epoch-1", 2, { op: "gesture", preset: "backflip", amplitude: 1 });
    expect(h.sent).toEqual([
      { type: "reject", epoch: "epoch-1", seq: 1, reason: "unknown_preset" },
      { type: "reject", epoch: "epoch-1", seq: 2, reason: "unknown_preset" },
    ]);
  });

  test("out-of-range or malformed arguments are rejected invalid_argument", () => {
    const h = harness();
    ready(h);
    h.host("epoch-1", 1, { op: "expression", preset: "joy", intensity: 2 });
    h.host("epoch-1", 2, { op: "viseme", viseme: "A", weight: 1 }); // old stand-in letter
    h.host("epoch-1", 3, { op: "gaze", target: "ceiling" });
    const reasons = h.sent.map((f) => f.type === "reject" && f.reason);
    expect(reasons).toEqual(["invalid_argument", "invalid_argument", "invalid_argument"]);
  });

  test("only one viseme is active and neutral closes the mouth", () => {
    const h = harness();
    ready(h);
    h.host("epoch-1", 1, { op: "viseme", viseme: "aa", weight: 0.8 });
    h.host("epoch-1", 2, { op: "viseme", viseme: "oh", weight: 0.5 });
    expect(h.state.target.visemes).toEqual({ aa: 0, ih: 0, ou: 0, ee: 0, oh: 0.5 });
    h.host("epoch-1", 3, { op: "viseme", viseme: "neutral", weight: 0 });
    expect(Object.values(h.state.target.visemes).every((v) => v === 0)).toBe(true);
  });

  test("reset zeroes expressions, visemes, gaze and gestures at once", () => {
    const h = harness();
    ready(h);
    h.host("epoch-1", 1, { op: "expression", preset: "anger", intensity: 1 });
    h.host("epoch-1", 2, { op: "gaze", target: "game" });
    h.host("epoch-1", 3, { op: "gesture", preset: "nod", amplitude: 2 });
    h.state.step(0.05);
    expect(h.state.headPose().x).not.toBe(0);
    h.host("epoch-1", 4, { op: "reset" });
    expect(h.state.current.expressions.anger).toBe(0);
    expect(h.state.target.yaw).toBe(0);
    expect(h.state.headPose()).toEqual({ x: 0, y: 0 });
  });

  test("interpolation converges and is frame-delta driven", () => {
    const state = new AvatarState();
    state.apply({ op: "expression", preset: "joy", intensity: 1 });
    state.step(SMOOTHING_TAU_S);
    expect(state.current.expressions.joy).toBeCloseTo(1 - Math.exp(-1), 5);
    state.step(0); // no elapsed frame time -> no movement
    expect(state.current.expressions.joy).toBeCloseTo(1 - Math.exp(-1), 5);
  });
});

describe("decode and backoff", () => {
  test("host fixture frames decode", () => {
    const url = new URL("../../../crates/adapters/tests/fixtures/local-avatar-1/valid-host.json", import.meta.url);
    const frames = JSON.parse(readFileSync(url, "utf8")) as unknown[];
    const kinds = frames.map((f) => decodeHostFrame(JSON.stringify(f)).kind);
    // the last fixture is a gesture with amplitude 2.0, still in range
    expect(kinds.every((k) => k === "frame")).toBe(true);
  });

  test("invalid host fixtures never decode to a frame", () => {
    const url = new URL("../../../crates/adapters/tests/fixtures/local-avatar-1/invalid-host.json", import.meta.url);
    const cases = JSON.parse(readFileSync(url, "utf8")) as Array<{ frame?: unknown; raw?: string }>;
    for (const c of cases) {
      const text = c.raw ?? JSON.stringify(c.frame ?? c);
      expect(decodeHostFrame(text).kind).not.toBe("frame");
    }
  });

  test("oversized and non-JSON frames are ignored", () => {
    expect(decodeHostFrame("x".repeat(5000)).kind).toBe("ignore");
    expect(decodeHostFrame("not json").kind).toBe("ignore");
    expect(decodeHostFrame("[]").kind).toBe("ignore");
  });

  test("backoff doubles from 250 ms and is capped at 5 s", () => {
    expect([0, 1, 2, 3, 4, 5, 6, 50].map(backoffMs)).toEqual([250, 500, 1000, 2000, 4000, 5000, 5000, 5000]);
  });
});

describe("active model source (#362)", () => {
  const A = "a".repeat(64);
  const B = "b".repeat(64);

  test("the urls carry the secret in the query and name no file", () => {
    expect(activeModelUrl("t0k", false)).toBe("/avatar/model.vrm?token=t0k");
    expect(activeModelUrl("t0k", true)).toBe("/avatar/model.vrm?token=t0k&fallback=sample");
    expect(activeIdUrl("t0k")).toBe("/avatar/active?token=t0k");
    // The secret is encoded, so it cannot add a parameter of its own.
    expect(activeModelUrl("a&fallback=sample", false)).toBe("/avatar/model.vrm?token=a%26fallback%3Dsample");
  });

  test("a model id is exactly 64 lowercase hex characters, else the page does not watch", () => {
    expect(modelIdFromHeader(A)).toBe(A);
    for (const bad of [null, "", "abc", "A".repeat(64), "g".repeat(64), `${A}0`]) {
      expect(modelIdFromHeader(bad)).toBeNull();
    }
  });

  test("the page reloads only for a well-formed, different id", () => {
    expect(shouldReload(A, { id: B })).toBe(true);
    expect(shouldReload(A, { id: A })).toBe(false);
    expect(shouldReload(null, { id: B })).toBe(false);
    for (const bad of [null, undefined, "x", 7, {}, { id: 7 }, { id: "" }, { id: "nothex" }, { id: B.toUpperCase() }]) {
      expect(shouldReload(A, bad)).toBe(false);
    }
  });

  test("the poll is slow enough to be cheap and fast enough to feel live", () => {
    expect(RELOAD_POLL_MS).toBeGreaterThanOrEqual(1000);
    expect(RELOAD_POLL_MS).toBeLessThanOrEqual(5000);
  });
});
