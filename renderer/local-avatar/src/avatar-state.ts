// Pure avatar target/current state: no three.js, no DOM, no clock. The render
// loop feeds `step` an elapsed-frame delta; nothing here reads wall-clock time.
// High-level decisions never happen here: the renderer only interpolates
// toward whatever target the host last commanded (ADR-0003).

import {
  EXPRESSIONS,
  GAZE_ANGLES,
  VISEME_EXPRESSIONS,
  isExpressionPreset,
  isGesturePreset,
  type ExpressionPreset,
  type GesturePreset,
  type MouthViseme,
} from "./profile";
import type { RejectReason, WireCommand } from "./protocol";

/// Local interpolation time constant, seconds. Design parameter, not measured.
export const SMOOTHING_TAU_S = 0.06;
/// Frame deltas are clamped so a stalled tab cannot teleport state or blow up
/// the interpolation factor.
export const MAX_STEP_S = 0.1;
export const GESTURE_DURATION_S = 0.8;
/// Effective gesture amplitude cap (the wire allows up to 2).
export const GESTURE_AMPLITUDE_CAP = 1;

const EXPRESSION_KEYS = Object.keys(EXPRESSIONS) as ExpressionPreset[];
const VISEME_KEYS = Object.keys(VISEME_EXPRESSIONS) as MouthViseme[];

export interface Values {
  expressions: Record<ExpressionPreset, number>;
  visemes: Record<MouthViseme, number>;
  yaw: number;
  pitch: number;
}

const zeroValues = (): Values => ({
  expressions: Object.fromEntries(EXPRESSION_KEYS.map((k) => [k, 0])) as Values["expressions"],
  visemes: Object.fromEntries(VISEME_KEYS.map((k) => [k, 0])) as Values["visemes"],
  yaw: 0,
  pitch: 0,
});

const clamp01 = (x: number): number => Math.min(1, Math.max(0, x));

export class AvatarState {
  /// What the host last commanded.
  readonly target: Values = zeroValues();
  /// What is applied to the model this frame (interpolating toward `target`).
  readonly current: Values = zeroValues();
  private gesture: { kind: GesturePreset; t: number; amplitude: number } | null = null;

  /// Applies one command to the target. Returns a reject reason, or null.
  apply(cmd: WireCommand): RejectReason | null {
    switch (cmd.op) {
      case "expression":
        if (!isExpressionPreset(cmd.preset)) return "unknown_preset";
        this.target.expressions[cmd.preset] = clamp01(cmd.intensity);
        return null;
      case "viseme":
        // One active viseme at a time; `neutral` closes the mouth.
        for (const key of VISEME_KEYS) this.target.visemes[key] = 0;
        if (cmd.viseme !== "neutral") this.target.visemes[cmd.viseme] = clamp01(cmd.weight);
        return null;
      case "gaze": {
        const angles = GAZE_ANGLES[cmd.target];
        if (angles === undefined) return "invalid_argument";
        [this.target.yaw, this.target.pitch] = angles;
        return null;
      }
      case "gesture":
        if (!isGesturePreset(cmd.preset)) return "unknown_preset";
        this.gesture = {
          kind: cmd.preset,
          t: 0,
          amplitude: Math.min(GESTURE_AMPLITUDE_CAP, Math.max(0, cmd.amplitude)),
        };
        return null;
      case "reset":
        this.snapToNeutral();
        return null;
    }
  }

  /// Zeroes target AND current at once: neutral is a snap, never an
  /// interpolation, so no stale emotion is ever visible (ADR-0003 decision 4).
  snapToNeutral(): void {
    Object.assign(this.target, zeroValues());
    Object.assign(this.current, zeroValues());
    this.gesture = null;
  }

  /// Advances interpolation by `dtSeconds` of frame time.
  step(dtSeconds: number): void {
    const dt = Math.min(MAX_STEP_S, Math.max(0, dtSeconds));
    const a = 1 - Math.exp(-dt / SMOOTHING_TAU_S);
    for (const key of EXPRESSION_KEYS) {
      this.current.expressions[key] += (this.target.expressions[key] - this.current.expressions[key]) * a;
    }
    for (const key of VISEME_KEYS) {
      this.current.visemes[key] += (this.target.visemes[key] - this.current.visemes[key]) * a;
    }
    this.current.yaw += (this.target.yaw - this.current.yaw) * a;
    this.current.pitch += (this.target.pitch - this.current.pitch) * a;
    if (this.gesture !== null) {
      this.gesture.t += dt;
      if (this.gesture.t >= GESTURE_DURATION_S) this.gesture = null;
    }
  }

  /// Bounded procedural head rotation (radians) for the active gesture.
  headPose(): { x: number; y: number } {
    if (this.gesture === null) return { x: 0, y: 0 };
    const p = this.gesture.t / GESTURE_DURATION_S;
    const wave = Math.sin(p * Math.PI * 2) * this.gesture.amplitude;
    return this.gesture.kind === "nod" ? { x: 0.25 * wave, y: 0 } : { x: 0, y: 0.3 * wave };
  }
}
