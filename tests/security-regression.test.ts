import { describe, expect, test } from "bun:test";

import { join } from "node:path";

import { evaluateCase, loadSecurityCases } from "../src/regression.js";
import { authorize, gateMemoryWrite } from "../src/security.js";

const caseDir = join(import.meta.dir, "..", "examples", "security");

describe("security regression fixtures (threat model section 14)", () => {
  const cases = loadSecurityCases(caseDir);

  test("all baseline fixtures are present", () => {
    const ids = cases.map((c) => c.id);
    expect(ids).toContain("security.prompt_injection.chat_obs");
    expect(ids).toContain("security.memory_poisoning.viewer_claim");
    expect(ids).toContain("security.authorization_forgery.model_claim");
    expect(ids).toContain("security.resource_abuse.oversized_chat_flood");
    expect(ids).toContain("security.emergency_control.stop_offline");
  });

  for (const scenario of cases) {
    test(`case: ${scenario.id}`, () => {
      const result = evaluateCase(scenario);
      if (!result.passed) {
        throw new Error(`${scenario.id} failed:\n  ${result.failures.join("\n  ")}`);
      }
      expect(result.passed).toBe(true);
    });
  }

  test("emergency stop is authorized offline with the right capability", () => {
    const offline = cases.find((c) => c.id === "security.emergency_control.stop_offline")!;
    const auth = authorize(offline.input.event);
    expect(auth.control_authorized).toBe(true);
    expect(auth.required_action).toBe("performer.stop");
  });

  test("untrusted chat can never authorize control", () => {
    for (const scenario of cases) {
      if (scenario.input.event.trust_level === "untrusted") {
        expect(authorize(scenario.input.event).control_authorized).toBe(false);
      }
    }
  });

  test("untrusted content and llm claims cannot write memory", () => {
    const poisoning = cases.find((c) => c.id === "security.memory_poisoning.viewer_claim")!;
    expect(gateMemoryWrite(poisoning.input.event).memory_write_allowed).toBe(false);
    expect(
      gateMemoryWrite(poisoning.input.event, { trustedLlmClaim: true }).memory_write_allowed,
    ).toBe(false);
  });
});
