// Provider-neutral event envelope: normalization and trust assignment.
//
// Implements docs/domain-contract.adoc sections 2-3 and the trust
// invariants of docs/security-threat-model.adoc sections 3-4:
//   - the normalizer, not the payload, assigns source_class/plane/trust_level;
//   - content kinds cannot carry `authorization`;
//   - only `operator.command` (plane: control) may carry authorization.

export type EventKind =
  | "chat.message"
  | "chat.donation"
  | "speech.input"
  | "game.event"
  | "stream.event"
  | "timer.tick"
  | "system.health"
  | "operator.command";

export type SourceClass =
  | "public_chat"
  | "donation"
  | "speech"
  | "game"
  | "stream"
  | "timer"
  | "operator"
  | "system";

export type Plane = "content" | "control" | "system";
export type TrustLevel = "untrusted" | "semi_trusted" | "trusted";

export type Capability =
  | "performer.mute"
  | "performer.stop"
  | "obs.control"
  | "avatar.control"
  | "memory.admin"
  | "tool.grant";

export const CAPABILITIES: readonly Capability[] = [
  "performer.mute",
  "performer.stop",
  "obs.control",
  "avatar.control",
  "memory.admin",
  "tool.grant",
];

export type AuthorizationMethod =
  | "operator_ui"
  | "operator_hotkey"
  | "signed_local_api";

export interface EventAuthorization {
  principal: string;
  method: AuthorizationMethod;
  capabilities: Capability[];
}

export interface EventEnvelope {
  schema_version: "0.2.0";
  event_id: string;
  correlation_id: string;
  sequence: number;
  observed_at: string;
  source: string;
  source_class: SourceClass;
  plane: Plane;
  trust_level: TrustLevel;
  kind: EventKind;
  actor_id: string | null;
  priority_hint: number | null;
  authorization?: EventAuthorization;
  payload: Record<string, unknown>;
}

// The strict kind -> (source_class, plane, allowed trust) mapping from
// domain-contract.adoc section 3.1.
interface KindBinding {
  sourceClass: SourceClass;
  plane: Plane;
  allowedTrust: readonly TrustLevel[];
}

export const KIND_BINDINGS: Record<EventKind, KindBinding> = {
  "chat.message": { sourceClass: "public_chat", plane: "content", allowedTrust: ["untrusted", "semi_trusted"] },
  "chat.donation": { sourceClass: "donation", plane: "content", allowedTrust: ["untrusted", "semi_trusted"] },
  "speech.input": { sourceClass: "speech", plane: "content", allowedTrust: ["untrusted", "semi_trusted"] },
  "game.event": { sourceClass: "game", plane: "content", allowedTrust: ["semi_trusted"] },
  "stream.event": { sourceClass: "stream", plane: "system", allowedTrust: ["semi_trusted", "trusted"] },
  "timer.tick": { sourceClass: "timer", plane: "system", allowedTrust: ["trusted"] },
  "system.health": { sourceClass: "system", plane: "system", allowedTrust: ["trusted"] },
  "operator.command": { sourceClass: "operator", plane: "control", allowedTrust: ["trusted"] },
};

export class NormalizationError extends Error {}

let idCounter = 0;

function defaultEventId(): string {
  idCounter += 1;
  return `evt-${Date.now().toString(36)}-${idCounter.toString(36)}`;
}

export interface NormalizeInput {
  kind: EventKind;
  source: string;
  payload: Record<string, unknown>;
  event_id?: string;
  correlation_id?: string;
  sequence?: number;
  observed_at?: string;
  actor_id?: string | null;
  priority_hint?: number | null;
  /** Optional explicit trust; must be inside the kind's allowed set. */
  trust_level?: TrustLevel;
  authorization?: EventAuthorization;
}

/**
 * Construct a normalized envelope from adapter input.
 *
 * Provider payloads are data only: any command-like text inside `payload`
 * cannot influence the normalized kind, plane, or trust level.
 */
export function normalizeEvent(input: NormalizeInput): EventEnvelope {
  const binding = KIND_BINDINGS[input.kind];
  if (!binding) {
    throw new NormalizationError(`unknown event kind: ${String((input as { kind?: unknown }).kind)}`);
  }

  if (input.authorization && input.kind !== "operator.command") {
    throw new NormalizationError(
      `${input.kind} cannot carry authorization: control-plane capabilities must be minted by a trusted local adapter`,
    );
  }

  if (input.kind === "operator.command" && !input.authorization) {
    throw new NormalizationError("operator.command requires authorization from a trusted local adapter");
  }

  if (!input.source || typeof input.source !== "string") {
    throw new NormalizationError("event source is required");
  }

  const trust = input.trust_level ?? binding.allowedTrust[0];
  if (!binding.allowedTrust.includes(trust)) {
    throw new NormalizationError(
      `trust_level ${trust} is not allowed for ${input.kind} (allowed: ${binding.allowedTrust.join(", ")})`,
    );
  }

  return {
    schema_version: "0.2.0",
    event_id: input.event_id ?? defaultEventId(),
    correlation_id: input.correlation_id ?? `corr-${input.event_id ?? defaultEventId()}`,
    sequence: input.sequence ?? 0,
    observed_at: input.observed_at ?? new Date().toISOString(),
    source: input.source,
    source_class: binding.sourceClass,
    plane: binding.plane,
    trust_level: trust,
    kind: input.kind,
    actor_id: input.actor_id ?? null,
    priority_hint: input.priority_hint ?? null,
    authorization: input.authorization,
    payload: input.payload,
  };
}
