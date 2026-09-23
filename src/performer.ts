// Deterministic performer: event trace -> performance plan.
//
// Implements the roadmap Phase 1 exit criterion: a recorded event trace
// deterministically produces the same performance plan. Given the same
// trace, assets, seed, and config, every planned action is reproducible.

import type { EventEnvelope } from "./envelope.js";
import { authorize } from "./security.js";
import { AssetStore, type PerformanceAsset } from "./asset-store.js";
import { Scheduler, type PlannedPerformance, type PriorityClass } from "./scheduler.js";
import { applyVariation, createRng, type AppliedVariation } from "./rng.js";

export interface PerformerConfig {
  compiler_version: string;
  voice_model?: string | null;
  /** Seed recorded in the replay trace. */
  seed: number;
  scheduler?: { minReactionSpacingMs?: number };
}

export interface EventOutcome {
  event_id: string;
  action:
    | "scheduled"
    | "operator_stop"
    | "cooldown_rejected"
    | "priority_rejected"
    | "no_asset"
    | "dropped";
  plan: PlannedPerformance | null;
  variation: AppliedVariation | null;
  asset_id: string | null;
}

/** Deterministic priority mapping; untrusted content never reaches operator class. */
function priorityFor(event: EventEnvelope): PriorityClass {
  switch (event.kind) {
    case "operator.command":
      return "operator";
    case "chat.donation":
      return "paid_interaction";
    case "game.event":
      return "system_reaction";
    case "chat.message":
    case "speech.input":
      return "conversation";
    case "stream.event":
      return "commentary";
    case "timer.tick":
    case "system.health":
      return "background";
  }
}

/** Deterministic intent -> fallback asset family when no reflex layer runs. */
function intentFor(event: EventEnvelope): string | null {
  const kind = event.payload?.["intent"];
  if (typeof kind === "string") return kind;
  if (event.kind === "chat.donation") return "gratitude.viewer";
  return null;
}

export class DeterministicPerformer {
  readonly assets: AssetStore;
  readonly scheduler: Scheduler;
  private readonly config: PerformerConfig;
  private clockMs = 0;

  constructor(assets: AssetStore, config: PerformerConfig) {
    this.assets = assets;
    this.config = config;
    this.scheduler = new Scheduler(config.scheduler);
  }

  get currentTimeMs(): number {
    return this.clockMs;
  }

  /**
   * Advance the monotonic clock, then process one event deterministically.
   * The seed-derived RNG stream is consumed exactly once per handled event,
   * in trace order, so replay reproduces identical variation.
   */
  handle(event: EventEnvelope, atMs: number): EventOutcome {
    this.clockMs = atMs;

    const priority = priorityFor(event);

    // Deterministic operator override: stop, no model, no asset needed.
    if (event.kind === "operator.command") {
      const auth = authorize(event);
      if (auth.control_authorized && auth.required_action === "performer.stop") {
        const stopped = this.scheduler.stopCurrent(atMs);
        return {
          event_id: event.event_id,
          action: "operator_stop",
          plan: stopped ?? null,
          variation: null,
          asset_id: null,
        };
      }
      return { event_id: event.event_id, action: "dropped", plan: null, variation: null, asset_id: null };
    }

    // Resolve an asset: exact intent id first, then variant group.
    const intent = intentFor(event);
    let asset: PerformanceAsset | undefined;
    let variation: AppliedVariation | null = null;

    if (intent) {
      asset = this.assets.get(intent) ?? this.pickVariant(intent);
    }

    if (!asset) {
      return { event_id: event.event_id, action: "no_asset", plan: null, variation: null, asset_id: null };
    }

    // Consume the deterministic variation stream for this event.
    variation = applyVariation(asset.variation ?? {}, createRng(this.config.seed + event.sequence));

    const plan = this.scheduler.schedule({
      eventId: event.event_id,
      assetId: asset.id,
      priority,
      interruptible: true,
      interruptPointsMs: [...asset.interrupt_points_ms],
      startAtMs: atMs + variation.startDelayMs,
      durationMs: asset.speech?.duration_ms ?? 600,
    });

    if (!plan) {
      const action = this.scheduler.lastRejection === "cooldown" ? "cooldown_rejected" : "priority_rejected";
      return { event_id: event.event_id, action, plan: null, variation, asset_id: asset.id };
    }

    return { event_id: event.event_id, action: "scheduled", plan, variation, asset_id: asset.id };
  }

  /** Deterministic variant selection: lowest asset id among the group (stable ordering). */
  private pickVariant(variantGroup: string): PerformanceAsset | undefined {
    return this.assets
      .variantsOf(variantGroup)
      .map((a) => a.id)
      .sort()
      .map((id) => this.assets.get(id)!)
      .find(() => true);
  }

  /** Run a full recorded trace; returns outcomes in trace order. */
  runTrace(trace: { event: EventEnvelope; atMs: number }[]): EventOutcome[] {
    return trace.map(({ event, atMs }) => this.handle(event, atMs));
  }
}
