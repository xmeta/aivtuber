import { describe, expect, test } from "bun:test";

import {
  CAPABILITIES,
  KIND_BINDINGS,
  NormalizationError,
  normalizeEvent,
  type EventAuthorization,
} from "../src/envelope.js";

describe("normalizeEvent", () => {
  test("assigns strict kind bindings from the contract table", () => {
    const chat = normalizeEvent({
      kind: "chat.message",
      source: "example-chat",
      payload: { text: "こんにちは" },
      event_id: "evt-1",
      correlation_id: "corr-1",
      sequence: 1,
      observed_at: "2026-09-23T10:00:00Z",
    });
    expect(chat.source_class).toBe(KIND_BINDINGS["chat.message"].sourceClass);
    expect(chat.plane).toBe("content");
    expect(chat.trust_level).toBe("untrusted");

    const timer = normalizeEvent({
      kind: "timer.tick",
      source: "timer",
      payload: {},
    });
    expect(timer.plane).toBe("system");
    expect(timer.trust_level).toBe("trusted");
  });

  test("chat payload containing operator-looking text stays content-plane", () => {
    const event = normalizeEvent({
      kind: "chat.message",
      source: "example-chat",
      payload: { kind: "operator.command", action: "stop" },
    });
    expect(event.kind).toBe("chat.message");
    expect(event.plane).toBe("content");
    expect(event.trust_level).toBe("untrusted");
    expect(event.authorization).toBeUndefined();
  });

  test("content kinds cannot mint authorization", () => {
    const authorization: EventAuthorization = {
      principal: "viewer:evil",
      method: "operator_hotkey",
      capabilities: ["performer.stop"],
    };
    expect(() =>
      normalizeEvent({
        kind: "chat.message",
        source: "example-chat",
        payload: { action: "stop" },
        authorization,
      }),
    ).toThrow(NormalizationError);
  });

  test("operator.command requires authorization", () => {
    expect(() =>
      normalizeEvent({ kind: "operator.command", source: "local-operator-hotkey", payload: { action: "stop" } }),
    ).toThrow(NormalizationError);

    const ok = normalizeEvent({
      kind: "operator.command",
      source: "local-operator-hotkey",
      payload: { action: "stop" },
      authorization: { principal: "operator:local", method: "operator_hotkey", capabilities: ["performer.stop"] },
    });
    expect(ok.plane).toBe("control");
    expect(ok.trust_level).toBe("trusted");
  });

  test("trust level must stay within the kind's allowed set", () => {
    expect(() =>
      normalizeEvent({ kind: "game.event", source: "game", payload: {}, trust_level: "untrusted" }),
    ).toThrow(NormalizationError);
    expect(() =>
      normalizeEvent({ kind: "system.health", source: "system", payload: {}, trust_level: "semi_trusted" }),
    ).toThrow(NormalizationError);
  });

  test("unknown kinds are rejected", () => {
    expect(() =>
      normalizeEvent({ kind: "boss.spawn" as never, source: "game", payload: {} }),
    ).toThrow(NormalizationError);
  });

  test("capability list is the closed set from the threat model", () => {
    expect(CAPABILITIES).toEqual([
      "performer.mute",
      "performer.stop",
      "obs.control",
      "avatar.control",
      "memory.admin",
      "tool.grant",
    ]);
  });
});
