// Deterministic capability-based authorization and memory-write gating.
//
// Implements docs/security-threat-model.adoc section 5 (allowlisted
// capabilities, deterministic authorization) and section 8 (memory
// poisoning gate).

import { CAPABILITIES, type Capability, type EventEnvelope } from "./envelope.js";

export interface AuthorizationResult {
  control_authorized: boolean;
  required_action: string | null;
  reason?: string;
}

/**
 * Deterministic mapping from operator action verbs to capabilities.
 * Unknown verbs are never authorized.
 */
const ACTION_TO_CAPABILITY: Record<string, Capability> = {
  stop: "performer.stop",
  mute: "performer.mute",
};

function requestedCapability(action: string): Capability | null {
  if ((CAPABILITIES as readonly string[]).includes(action)) return action as Capability;
  return ACTION_TO_CAPABILITY[action] ?? null;
}

/**
 * Deterministically decide whether an operator.command is authorized.
 *
 * Authorization comes only from the envelope's trusted-local authorization
 * context. Anything an untrusted source said, or anything a model produced,
 * can never appear here.
 */
export function authorize(event: EventEnvelope): AuthorizationResult {
  if (event.kind !== "operator.command") {
    return { control_authorized: false, required_action: null, reason: "not_operator_command" };
  }
  const auth = event.authorization;
  if (!auth) {
    return { control_authorized: false, required_action: null, reason: "missing_authorization" };
  }
  if (event.trust_level !== "trusted" || event.plane !== "control") {
    return { control_authorized: false, required_action: null, reason: "trust_boundary_violation" };
  }
  const requested = typeof event.payload?.action === "string" ? event.payload.action : null;
  if (!requested) {
    return { control_authorized: false, required_action: null, reason: "missing_action" };
  }
  const capability = requestedCapability(requested);
  if (!capability) {
    return { control_authorized: false, required_action: null, reason: "action_not_allowlisted" };
  }
  if (!auth.capabilities.includes(capability)) {
    return { control_authorized: false, required_action: null, reason: "capability_not_granted" };
  }
  return { control_authorized: true, required_action: capability };
}

export interface MemoryGateResult {
  memory_write_allowed: boolean;
  reason: string;
}

/**
 * Gate durable memory writes.
 *
 * A write requires a trusted/semi-trusted system source or an explicit
 * memory.admin capability. Untrusted content and model output are never
 * sufficient on their own.
 */
export function gateMemoryWrite(
  event: EventEnvelope,
  opts: { trustedLlmClaim?: boolean } = {},
): MemoryGateResult {
  if (event.authorization?.capabilities.includes("memory.admin")) {
    return { memory_write_allowed: true, reason: "memory_admin_capability" };
  }
  if (event.plane === "system" && (event.trust_level === "trusted" || event.trust_level === "semi_trusted")) {
    return { memory_write_allowed: true, reason: "system_source" };
  }
  if (opts.trustedLlmClaim) {
    return { memory_write_allowed: false, reason: "llm_claim_requires_external_gate" };
  }
  return { memory_write_allowed: false, reason: "untrusted_content" };
}
