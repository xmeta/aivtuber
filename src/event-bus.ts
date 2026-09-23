// Deterministic event bus with bounded per-plane queues.
//
// Implements roadmap phase 1 "Event bus" plus the backpressure rules of
// threat model section 9: content queues are bounded and overflow drops
// low-priority events; the control plane has its own queue that never
// blocks. Dispatch order is deterministic: control first, then priority
// class, then monotonic sequence.

import type { EventEnvelope } from "./envelope.js";

export interface BusOptions {
  /** Maximum buffered content events; overflow drops the lowest priority. */
  contentQueueLimit?: number;
}

interface QueuedEvent {
  event: EventEnvelope;
  /** Smaller = more important (matches scheduler priority ranks). */
  rank: number;
}

const KIND_RANK: Record<string, number> = {
  "operator.command": 0,
  "chat.donation": 1,
  "game.event": 2,
  "chat.message": 3,
  "speech.input": 3,
  "stream.event": 4,
  "timer.tick": 5,
  "system.health": 5,
};

export class EventBus {
  private content: QueuedEvent[] = [];
  private control: EventEnvelope[] = [];
  private dropped = 0;
  private readonly limit: number;

  constructor(options: BusOptions = {}) {
    this.limit = options.contentQueueLimit ?? 128;
  }

  get stats(): { buffered: number; controlBuffered: number; dropped: number } {
    return { buffered: this.content.length, controlBuffered: this.control.length, dropped: this.dropped };
  }

  publish(event: EventEnvelope): "queued" | "dropped_overflow" {
    if (event.plane === "control") {
      this.control.push(event);
      return "queued";
    }
    const rank = KIND_RANK[event.kind] ?? 5;
    this.content.push({ event, rank });
    if (this.content.length > this.limit) {
      // Drop the lowest-rank (least important) event, latest insertion wins ties.
      let worst = 0;
      for (let i = 1; i < this.content.length; i++) {
        const a = this.content[i];
        const b = this.content[worst];
        if (a.rank > b.rank || (a.rank === b.rank && a.event.sequence >= b.event.sequence)) worst = i;
      }
      this.content.splice(worst, 1);
      this.dropped += 1;
      return "dropped_overflow";
    }
    return "queued";
  }

  /**
   * Drain all buffered events in deterministic dispatch order:
   * control plane first, then ascending rank, then ascending sequence.
   */
  drain(): EventEnvelope[] {
    const controlEvents = this.control.splice(0);
    const contentEvents = this.content.splice(0);
    contentEvents.sort((a, b) => a.rank - b.rank || a.event.sequence - b.event.sequence);
    return [...controlEvents, ...contentEvents.map((q) => q.event)];
  }
}
