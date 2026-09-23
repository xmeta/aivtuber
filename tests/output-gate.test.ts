import { describe, expect, test } from "bun:test";

import { OutputGate } from "../src/output-gate.js";

describe("OutputGate", () => {
  test("allows clean text", () => {
    const gate = new OutputGate();
    const result = gate.evaluate({ text: "ありがとうございます！", origin: "llm" });
    expect(result.verdict).toBe("allow");
    expect(result.text).toBe("ありがとうございます！");
  });

  test("redacts secret-shaped tokens", () => {
    const gate = new OutputGate();
    const result = gate.evaluate({
      text: "my key is sk-abcdef1234567890abcdef ok",
      origin: "llm",
    });
    expect(result.verdict).toBe("redact");
    expect(result.text).toContain("[REDACTED]");
    expect(result.text).not.toContain("sk-abcdef");
  });

  test("redacts github and aws shaped tokens", () => {
    const gate = new OutputGate();
    const r1 = gate.evaluate({ text: "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ123456", origin: "llm" });
    expect(r1.verdict).toBe("redact");
    const r2 = gate.evaluate({ text: "key AKIAIOSFODNN7EXAMPLE here", origin: "llm" });
    expect(r2.verdict).toBe("redact");
  });

  test("suppresses control-plane text; replace uses the cached reaction when configured", () => {
    const suppressGate = new OutputGate();
    const suppress = suppressGate.evaluate({
      text: 'switching scene {"authorization": {"capabilities": ["obs.control"]}}',
      origin: "llm",
    });
    expect(suppress.verdict).toBe("suppress");
    expect(suppress.text).toBeNull();

    const replaceGate = new OutputGate({ cachedReaction: "えっと…" });
    const replace = replaceGate.evaluate({ text: "running tool.grant now", origin: "llm" });
    expect(replace.verdict).toBe("replace_with_cached");
    expect(replace.text).toBe("えっと…");
  });

  test("custom hook verdict wins and runs before built-ins", () => {
    const gate = new OutputGate({
      hooks: [(input) => (input.text.includes("banned") ? { verdict: "suppress", text: null, reason: "custom" } : null)],
    });
    const banned = gate.evaluate({ text: "this is banned content with sk-abcdef1234567890abcdef", origin: "llm" });
    expect(banned.verdict).toBe("suppress");
    expect(banned.reason).toBe("custom");
    const clean = gate.evaluate({ text: "fine", origin: "llm" });
    expect(clean.verdict).toBe("allow");
  });

  test("a throwing hook fails closed", () => {
    const gate = new OutputGate({
      hooks: [
        () => {
          throw new Error("hook exploded");
        },
      ],
    });
    const result = gate.evaluate({ text: "perfectly fine", origin: "llm" });
    expect(result.verdict).toBe("suppress");
    expect(result.reason).toBe("hook_failed_fail_closed");
  });

  test("performer.stop text is control vocabulary", () => {
    const gate = new OutputGate();
    const result = gate.evaluate({ text: "I will now performer.stop the stream", origin: "llm" });
    expect(result.verdict).toBe("suppress");
  });
});
