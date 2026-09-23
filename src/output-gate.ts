// Deterministic output gate for public speech/display.
//
// Implements threat model section 11: generated output passes a
// deterministic policy hook before becoming public. The hook can allow,
// redact, replace with a safe cached reaction, or suppress. A hook
// failure fails closed for public output but does not stop the scheduler.

export type GateVerdict = "allow" | "redact" | "replace_with_cached" | "suppress";

export interface GateInput {
  text: string;
  /** Where the output came from (llm, template, cached, ...). */
  origin: string;
}

export interface GateResult {
  verdict: GateVerdict;
  /** Text to publish (redacted/replacement text when applicable). */
  text: string | null;
  reason: string;
}

export type GateHook = (input: GateInput) => GateResult | null;

/** Secret-shaped tokens that must never be spoken publicly. */
const SECRET_PATTERNS: RegExp[] = [
  /\bsk-[A-Za-z0-9_-]{16,}\b/g, // OpenAI-style keys
  /\bgh[pousr]_[A-Za-z0-9]{20,}\b/g, // GitHub tokens
  /\bAKIA[0-9A-Z]{16}\b/g, // AWS access keys
  /\bxox[baprs]-[A-Za-z0-9-]{10,}\b/g, // Slack tokens
];

/** Control-plane vocabulary that must never appear in public output. */
const CONTROL_TEXT_PATTERNS: RegExp[] = [
  /\bobs\.control\b/i,
  /\btool\.grant\b/i,
  /\bmemory\.admin\b/i,
  /\bperformer\.(stop|mute)\b/i,
  /\b"authorization"\s*:/i,
];

export interface OutputGateOptions {
  /** Optional safe cached reaction used by the replace verdict. */
  cachedReaction?: string;
  hooks?: GateHook[];
}

export class OutputGate {
  private readonly hooks: GateHook[];
  private readonly cachedReaction?: string;

  constructor(options: OutputGateOptions = {}) {
    this.hooks = options.hooks ?? [];
    this.cachedReaction = options.cachedReaction;
  }

  /**
   * Apply deterministic checks. Order: custom hooks, control-text
   * suppression, secret redaction. A throwing hook suppresses (fail
   * closed) without affecting the scheduler.
   */
  evaluate(input: GateInput): GateResult {
    for (const hook of this.hooks) {
      let result: GateResult | null;
      try {
        result = hook(input);
      } catch {
        return { verdict: "suppress", text: null, reason: "hook_failed_fail_closed" };
      }
      if (result) return result;
    }

    if (CONTROL_TEXT_PATTERNS.some((re) => re.test(input.text))) {
      return {
        verdict: this.cachedReaction !== undefined ? "replace_with_cached" : "suppress",
        text: this.cachedReaction ?? null,
        reason: "control_plane_text_in_output",
      };
    }

    const redacted = this.redactSecrets(input.text);
    if (redacted !== input.text) {
      return { verdict: "redact", text: redacted, reason: "secret_shaped_token_redacted" };
    }

    return { verdict: "allow", text: input.text, reason: "clean" };
  }

  private redactSecrets(text: string): string {
    let out = text;
    for (const pattern of SECRET_PATTERNS) {
      out = out.replace(pattern, "[REDACTED]");
    }
    return out;
  }
}
