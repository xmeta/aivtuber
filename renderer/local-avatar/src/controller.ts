// Epoch/seq protocol state machine, independent of WebSocket and WebGL so it
// is unit-testable. Everything here is event-driven: it never reads a clock.

import { AvatarState } from "./avatar-state";
import {
  PROTOCOL_VERSION,
  decodeHostFrame,
  type RejectReason,
  type RendererFrame,
} from "./protocol";

/// More unacked applied commands than this means the render loop has stalled
/// (the host caps in-flight at 64, so this is only a defensive bound).
export const MAX_PENDING_ACKS = 256;

export interface ControllerOptions {
  state: AvatarState;
  send: (frame: RendererFrame) => void;
  newEpoch: () => string;
  modelHash: string;
  profile: string;
}

export class Controller {
  private epoch: string | null = null;
  private lastSeq = 0;
  private ready = false;
  private awaitingNeutralFrame = false;
  private pendingAcks: number[] = [];

  constructor(private readonly opts: ControllerOptions) {}

  get currentEpoch(): string | null {
    return this.epoch;
  }
  get lastAcceptedSeq(): number {
    return this.lastSeq;
  }
  get isReady(): boolean {
    return this.ready;
  }
  get pendingAckCount(): number {
    return this.pendingAcks.length;
  }

  /// A connection is open: new epoch, snap to neutral, announce. `ready` is
  /// withheld until a frame rendered after the snap (`onFrameRendered`).
  onOpen(): void {
    this.epoch = this.opts.newEpoch();
    this.lastSeq = 0;
    this.ready = false;
    this.pendingAcks = [];
    this.opts.state.snapToNeutral();
    this.awaitingNeutralFrame = true;
    this.opts.send({
      type: "hello",
      v: PROTOCOL_VERSION,
      epoch: this.epoch,
      model_hash: this.opts.modelHash,
      profile: this.opts.profile,
    });
  }

  /// Socket closed: forget the epoch and snap to neutral. Nothing queued for
  /// the old connection survives.
  onClose(): void {
    this.epoch = null;
    this.lastSeq = 0;
    this.ready = false;
    this.awaitingNeutralFrame = false;
    this.pendingAcks = [];
    this.opts.state.snapToNeutral();
  }

  onMessage(text: string): void {
    const epoch = this.epoch;
    if (epoch === null) return;
    const decoded = decodeHostFrame(text);
    if (decoded.kind === "ignore") return;
    if (decoded.kind === "invalid") {
      if (decoded.seq !== null) this.reject(epoch, decoded.seq, decoded.reason);
      return;
    }
    const { frame } = decoded;
    // Order of checks matters: a foreign epoch must never move `lastSeq`.
    if (frame.epoch !== epoch) return this.reject(epoch, frame.seq, "stale_epoch");
    if (frame.seq <= this.lastSeq) return this.reject(epoch, frame.seq, "stale_seq");
    if (!this.ready) return this.reject(epoch, frame.seq, "not_ready");
    if (this.pendingAcks.length >= MAX_PENDING_ACKS) {
      return this.reject(epoch, frame.seq, "other");
    }
    this.lastSeq = frame.seq;
    const reason = this.opts.state.apply(frame.cmd);
    if (reason !== null) return this.reject(epoch, frame.seq, reason);
    // Acked only once the frame containing this state has been rendered.
    this.pendingAcks.push(frame.seq);
  }

  /// Call after every `renderer.render()`.
  onFrameRendered(): void {
    const epoch = this.epoch;
    if (epoch === null) return;
    if (this.awaitingNeutralFrame) {
      this.awaitingNeutralFrame = false;
      this.ready = true;
      this.opts.send({ type: "ready", epoch });
    }
    for (const seq of this.pendingAcks) this.opts.send({ type: "ack", epoch, seq });
    this.pendingAcks = [];
  }

  private reject(epoch: string, seq: number, reason: RejectReason): void {
    this.opts.send({ type: "reject", epoch, seq, reason });
  }
}

/// Reconnect delay for attempt `n` (0-based): 250 ms doubling, capped at 5 s.
/// Deterministic on purpose; the host admits one connection, so there is no
/// thundering herd to jitter against.
export function backoffMs(attempt: number): number {
  return Math.min(5000, 250 * 2 ** Math.min(Math.max(attempt, 0), 10));
}
