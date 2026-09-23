// Performance scheduler: shared timeline, priority classes, cooldowns,
// and interruptibility arbitration.
//
// Implements docs/architecture.adoc sections 6.5, 6.7, and 8: priority
// alone is insufficient; the scheduler also honors interruptibility,
// cooldowns, asset exclusivity, and minimum reaction spacing. All
// decisions are deterministic given the same event trace and seed.

export type PriorityClass =
  | "operator"          // 1. operator/emergency
  | "paid_interaction"  // 2. direct paid/high-priority interaction
  | "system_reaction"   // 3. strong game/system reaction
  | "conversation"      // 4. direct conversation
  | "commentary"        // 5. normal stream commentary
  | "background";       // 6. background/idle

const PRIORITY_ORDER: Record<PriorityClass, number> = {
  operator: 0,
  paid_interaction: 1,
  system_reaction: 2,
  conversation: 3,
  commentary: 4,
  background: 5,
};

export interface PlannedPerformance {
  eventId: string;
  assetId: string;
  priority: PriorityClass;
  interruptible: boolean;
  interruptPointsMs: number[];
  startAtMs: number;
  durationMs: number;
  /** Monotonic generation id used as a cancellation token. */
  generation: number;
}

export interface ScheduledItem extends PlannedPerformance {
  status: "queued" | "playing" | "completed" | "cancelled";
}

export interface SchedulerOptions {
  /** Minimum ms between two reactions from the same semantic family. */
  minReactionSpacingMs?: number;
}

export class Scheduler {
  private items: ScheduledItem[] = [];
  private cooldowns = new Map<string, number>();
  private lastFinishAt: number | null = null;
  private generation = 0;
  private readonly minReactionSpacingMs: number;
  /** Reason for the most recent rejection, for deterministic replay telemetry. */
  lastRejection: "cooldown" | "priority" | null = null;

  constructor(options: SchedulerOptions = {}) {
    this.minReactionSpacingMs = options.minReactionSpacingMs ?? 1500;
  }

  get queue(): readonly ScheduledItem[] {
    return this.items;
  }

  get currentGeneration(): number {
    return this.generation;
  }

  /** Mark items whose end time has passed as completed (deterministic lazy completion). */
  private completeExpired(atMs: number): void {
    for (const item of this.items) {
      if (item.status === "playing" && item.startAtMs + item.durationMs <= atMs) {
        item.status = "completed";
      }
    }
  }

  /**
   * Try to schedule a performance. Returns the planned slot or null when
   * arbitration rejects it (cooldown or lower priority during an
   * uninterruptible performance).
   */
  schedule(plan: Omit<PlannedPerformance, "generation">): PlannedPerformance | null {
    this.lastRejection = null;
    this.completeExpired(plan.startAtMs);

    // Cooldown per asset id.
    const cooldownUntil = this.cooldowns.get(plan.assetId) ?? 0;
    if (plan.startAtMs < cooldownUntil) {
      this.lastRejection = "cooldown";
      return null;
    }

    const playing = this.items.find((i) => i.status === "playing");
    if (playing) {
      const incomingRank = PRIORITY_ORDER[plan.priority];
      const playingRank = PRIORITY_ORDER[playing.priority];
      const canInterrupt = playing.interruptible && incomingRank <= playingRank;
      if (incomingRank > playingRank) {
        this.lastRejection = "priority";
        return null;
      }
      if (canInterrupt) this.cancel(playing, plan.startAtMs);
    }

    // Minimum spacing between reactions (not from stream start).
    if (
      plan.priority !== "operator" &&
      this.lastFinishAt !== null &&
      plan.startAtMs - this.lastFinishAt < this.minReactionSpacingMs
    ) {
      plan = { ...plan, startAtMs: Math.max(plan.startAtMs, this.lastFinishAt + this.minReactionSpacingMs) };
    }

    this.generation += 1;
    const item: ScheduledItem = { ...plan, generation: this.generation, status: "playing" };
    this.items.push(item);
    this.lastFinishAt = plan.startAtMs + plan.durationMs;

    // Cooldown: asset becomes eligible again after it finishes plus spacing.
    this.cooldowns.set(plan.assetId, plan.startAtMs + plan.durationMs + this.minReactionSpacingMs);
    return item;
  }

  /** Deterministic stop for the current performance (operator path). */
  stopCurrent(atMs: number): ScheduledItem | null {
    const playing = this.items.find((i) => i.status === "playing");
    if (!playing) return null;
    playing.status = "cancelled";
    this.lastFinishAt = atMs;
    return playing;
  }

  private cancel(item: ScheduledItem, atMs: number): void {
    item.status = "cancelled";
    this.lastFinishAt = atMs;
  }
  /** Mark everything complete; used by the replay harness between runs. */
  completeAll(): void {
    for (const item of this.items) {
      if (item.status === "playing" || item.status === "queued") item.status = "completed";
    }
  }
}
