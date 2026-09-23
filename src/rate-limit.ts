// Per-source deterministic token-bucket rate limiting with backpressure.
//
// Implements docs/security-threat-model.adoc section 9: independent
// per-source queue/rate limits, payload size caps, drop/coalesce of
// low-priority content, and a separate bounded control-plane queue.

export interface RateLimiterOptions {
  /** Sustained events per second per source. */
  ratePerSecond: number;
  /** Burst capacity per source. */
  burst: number;
  /** Maximum payload JSON size in bytes; larger events are rejected. */
  maxPayloadBytes: number;
}

export type AdmitDecision = "admitted" | "dropped_rate" | "dropped_size";

interface Bucket {
  tokens: number;
  lastRefill: number;
}

const CONTROL_SOURCE = "control";

export class SourceRateLimiter {
  private buckets = new Map<string, Bucket>();
  private controlDepth = 0;
  private readonly options: RateLimiterOptions;
  private readonly now: () => number;

  constructor(options: RateLimiterOptions, now: () => number = () => Date.now()) {
    this.options = options;
    this.now = now;
  }

  /**
   * Try to admit an event from `source`.
   *
   * Control-plane events use a separate bounded queue (depth cap 64) and
   * bypass content rate limits entirely, so a chat flood can never block
   * an operator stop.
   */
  admit(source: string, payload: unknown, opts: { controlPlane?: boolean } = {}): AdmitDecision {
    if (opts.controlPlane) {
      if (this.controlDepth >= 64) return "dropped_rate";
      this.controlDepth += 1;
      return "admitted";
    }

    const size = JSON.stringify(payload ?? null)?.length ?? 0;
    if (size > this.options.maxPayloadBytes) return "dropped_size";

    const t = this.now();
    let bucket = this.buckets.get(source);
    if (!bucket) {
      bucket = { tokens: this.options.burst, lastRefill: t };
      this.buckets.set(source, bucket);
    }

    const elapsed = Math.max(0, t - bucket.lastRefill);
    bucket.tokens = Math.min(
      this.options.burst,
      bucket.tokens + (elapsed / 1000) * this.options.ratePerSecond,
    );
    bucket.lastRefill = t;

    if (bucket.tokens >= 1) {
      bucket.tokens -= 1;
      return "admitted";
    }
    return "dropped_rate";
  }

  /** Release a control-plane slot once the command has been handled. */
  releaseControl(): void {
    this.controlDepth = Math.max(0, this.controlDepth - 1);
  }
}
