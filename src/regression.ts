// Security regression case evaluator.
//
// Implements docs/security-threat-model.adoc section 14: the runtime loads
// the fixture files and asserts the expected authorization, memory-write,
// and model-dependency outcomes.

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import type { EventEnvelope } from "./envelope.js";
import { authorize, gateMemoryWrite } from "./security.js";

export interface SecurityCase {
  schema_version: string;
  id: string;
  scenario_type: string;
  input: { event: EventEnvelope; [k: string]: unknown };
  model_output: unknown;
  expected: {
    control_authorized: boolean;
    memory_write_allowed: boolean;
    requires_model: boolean;
    required_action: string | null;
    notes: string | null;
  };
}

export interface CaseResult {
  id: string;
  passed: boolean;
  failures: string[];
}

export function loadSecurityCases(dir: string): SecurityCase[] {
  return readdirSync(dir)
    .filter((f) => f.endsWith(".json"))
    .sort()
    .map((f) => JSON.parse(readFileSync(join(dir, f), "utf8")) as SecurityCase);
}

/** Evaluate one security regression case against the deterministic gates. */
export function evaluateCase(scenario: SecurityCase): CaseResult {
  const failures: string[] = [];
  const event = scenario.input.event;

  const auth = authorize(event);
  if (auth.control_authorized !== scenario.expected.control_authorized) {
    failures.push(
      `control_authorized: expected ${scenario.expected.control_authorized}, got ${auth.control_authorized} (${auth.reason ?? "granted"})`,
    );
  }
  if ((auth.required_action ?? null) !== (scenario.expected.required_action ?? null)) {
    failures.push(
      `required_action: expected ${scenario.expected.required_action ?? "null"}, got ${auth.required_action ?? "null"}`,
    );
  }

  const memory = gateMemoryWrite(event);
  if (memory.memory_write_allowed !== scenario.expected.memory_write_allowed) {
    failures.push(
      `memory_write_allowed: expected ${scenario.expected.memory_write_allowed}, got ${memory.memory_write_allowed} (${memory.reason})`,
    );
  }

  // requires_model: an authorized operator stop must be fully deterministic.
  if (scenario.expected.requires_model && auth.control_authorized) {
    failures.push("requires_model is true but control was authorized deterministically");
  }

  return { id: scenario.id, passed: failures.length === 0, failures };
}
