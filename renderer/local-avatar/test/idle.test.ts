// Unit tests for the renderer-local idle layer (issue #383). Pure functions of
// the animation clock: exact, no browser.
import { describe, expect, test } from "bun:test";
import {
  BLINK_DOUBLE_PROBABILITY,
  BLINK_DURATION_MAX_S,
  BLINK_DURATION_MIN_S,
  IDLE_BONES,
  MAX_LIMB_RAD,
  MAX_TORSO_RAD,
  SACCADE_MAX_DEG,
  SPEAK_NOD_X,
  blinkScaleForHost,
  blinkSlot,
  blinkWeight,
  idlePose,
  saccadeOffset,
} from "../src/idle";

const ON = { enabled: true, speaking: 0 } as const;
const OFF = { enabled: false, speaking: 0 } as const;
const HZ = 60;
const TEN_MIN_FRAMES = 10 * 60 * HZ;

describe("determinism", () => {
  test("same clock gives an identical pose regardless of call history", () => {
    const ref = idlePose(123.456, ON);
    for (let t = 0; t < 50; t += 0.37) idlePose(t, ON); // unrelated calls in between
    expect(idlePose(123.456, ON)).toEqual(ref);
    expect(blinkWeight(77.7)).toBe(blinkWeight(77.7));
  });
  test("non-finite or negative clocks give the rest pose", () => {
    expect(idlePose(Number.NaN, ON)).toEqual(idlePose(0, OFF));
    expect(idlePose(-1, ON)).toEqual(idlePose(0, OFF));
  });
});

describe("bounds", () => {
  test("every output stays in its documented bounds over 10 minutes at 60 Hz", () => {
    let maxBlink = 0;
    for (let i = 0; i < TEN_MIN_FRAMES; i++) {
      const p = idlePose(i / HZ, { enabled: true, speaking: (i % 7) / 6 });
      for (const bone of IDLE_BONES) {
        const limit = bone.includes("Arm") || bone.includes("Hand") ? MAX_LIMB_RAD : MAX_TORSO_RAD;
        for (const a of [p[bone].x, p[bone].y, p[bone].z]) {
          expect(Number.isFinite(a)).toBe(true);
          expect(Math.abs(a)).toBeLessThanOrEqual(limit);
        }
      }
      expect(p.blink).toBeGreaterThanOrEqual(0);
      expect(p.blink).toBeLessThanOrEqual(1);
      expect(Math.abs(p.gazeYawDeg)).toBeLessThanOrEqual(SACCADE_MAX_DEG);
      expect(Math.abs(p.gazePitchDeg)).toBeLessThanOrEqual(SACCADE_MAX_DEG);
      maxBlink = Math.max(maxBlink, p.blink);
    }
    expect(maxBlink).toBeGreaterThan(0.99);
  });
  test("breathing is visible but subtle", () => {
    let lo = Infinity;
    let hi = -Infinity;
    for (let i = 0; i < 20 * HZ; i++) {
      const x = idlePose(i / HZ, ON).chest.x;
      lo = Math.min(lo, x);
      hi = Math.max(hi, x);
    }
    expect(hi - lo).toBeGreaterThan(0.015);
    expect(hi - lo).toBeLessThan(0.06);
  });
});

describe("blinks", () => {
  const slots = Array.from({ length: 2000 }, (_, k) => blinkSlot(k));
  test("intervals between blinks are 2-6 s and irregular", () => {
    for (let k = 1; k < slots.length; k++) {
      const gap = slots[k]!.startS - slots[k - 1]!.startS;
      expect(gap).toBeGreaterThanOrEqual(2 - 1e-9);
      expect(gap).toBeLessThanOrEqual(6 + 1e-9);
    }
    const distinct = new Set(slots.map((s) => Math.round((s.startS % 4) * 100)));
    expect(distinct.size).toBeGreaterThan(50);
  });
  test("about 10-15 percent are double blinks; durations are 120-180 ms", () => {
    const doubles = slots.filter((s) => s.secondStartS !== null).length / slots.length;
    expect(doubles).toBeGreaterThan(0.08);
    expect(doubles).toBeLessThan(0.16);
    expect(BLINK_DOUBLE_PROBABILITY).toBeGreaterThanOrEqual(0.1);
    for (const s of slots) {
      expect(s.durationS).toBeGreaterThanOrEqual(BLINK_DURATION_MIN_S);
      expect(s.durationS).toBeLessThanOrEqual(BLINK_DURATION_MAX_S);
    }
  });
  test("weight is smooth (no step) and in 0..1 over 10 minutes at 60 Hz", () => {
    let prev = blinkWeight(0);
    let maxJump = 0;
    let closings = 0;
    for (let i = 1; i < TEN_MIN_FRAMES; i++) {
      const w = blinkWeight(i / HZ);
      expect(w).toBeGreaterThanOrEqual(0);
      expect(w).toBeLessThanOrEqual(1);
      maxJump = Math.max(maxJump, Math.abs(w - prev));
      if (w > 0.9 && prev <= 0.9) closings += 1;
      prev = w;
    }
    expect(maxJump).toBeLessThan(0.5);
    expect(maxJump).toBeGreaterThan(0.05);
    expect(closings).toBeGreaterThan(100);
  });
  test("host expressions that narrow the eyes scale the blink down", () => {
    expect(blinkScaleForHost([0, 0, 0])).toBe(1);
    expect(blinkScaleForHost([1, 0, 0])).toBe(0);
    expect(blinkScaleForHost([0.25, 0.5, 0])).toBe(0.5);
    expect(blinkScaleForHost([Number.NaN, 2])).toBe(0);
  });
});

describe("saccades", () => {
  test("offsets are bounded, change over time and hold for roughly 0.4-2 s", () => {
    const step = 0.01;
    let changes = 0;
    let lastChange = 0;
    let prev = saccadeOffset(0);
    let minHold = Infinity;
    let maxHold = 0;
    for (let t = step; t < 600; t += step) {
      const o = saccadeOffset(t);
      expect(Math.abs(o.yaw)).toBeLessThanOrEqual(SACCADE_MAX_DEG);
      expect(Math.abs(o.pitch)).toBeLessThanOrEqual(SACCADE_MAX_DEG);
      const moving = Math.abs(o.yaw - prev.yaw) + Math.abs(o.pitch - prev.pitch) > 1e-6;
      // a ramp is a few consecutive samples; the hold is measured between ramp starts
      if (moving && t - lastChange > 0.2) {
        if (changes > 1) {
          minHold = Math.min(minHold, t - lastChange);
          maxHold = Math.max(maxHold, t - lastChange);
        }
        changes += 1;
        lastChange = t;
      }
      prev = o;
    }
    expect(changes).toBeGreaterThan(200);
    expect(minHold).toBeGreaterThan(0.35);
    expect(maxHold).toBeLessThan(2.1);
  });
});

describe("switch-off, rest pose, mirroring, speaking", () => {
  test("disabled applies only the rest pose: no motion, no blink, no gaze offset", () => {
    const a = idlePose(0.3, OFF);
    const b = idlePose(97.1, { enabled: false, speaking: 1 });
    expect(b).toEqual(a);
    expect(a.blink).toBe(0);
    expect(a.gazeYawDeg).toBe(0);
    expect(a.gazePitchDeg).toBe(0);
    expect(Math.abs(a.leftUpperArm.z)).toBeGreaterThan(1.25);
    expect(Math.abs(a.leftUpperArm.z)).toBeLessThan(1.35);
    expect(a.leftLowerArm.y).not.toBe(0);
    expect(a.chest).toEqual({ x: 0, y: 0, z: 0 });
  });
  test("left and right arms mirror (left z negative, right z positive)", () => {
    for (const t of [0, 1.7, 12.3, 301.9]) {
      const p = idlePose(t, ON);
      const rest = idlePose(t, OFF);
      expect(rest.leftUpperArm.z).toBeLessThan(0);
      expect(rest.rightUpperArm.z).toBeGreaterThan(0);
      expect(rest.leftUpperArm.z).toBe(-rest.rightUpperArm.z);
      expect(rest.leftUpperArm.x).toBe(rest.rightUpperArm.x);
      expect(rest.leftLowerArm.y).toBe(-rest.rightLowerArm.y);
      expect(p.leftUpperArm.z).toBeLessThan(0);
      expect(p.rightUpperArm.z).toBeGreaterThan(0);
      expect(Math.sign(p.leftLowerArm.y)).toBe(-Math.sign(p.rightLowerArm.y));
    }
  });
  test("speaking adds bounded extra head motion; out-of-range levels are clamped", () => {
    let extra = 0;
    for (let i = 0; i < 20 * HZ; i++) {
      const t = i / HZ;
      const quiet = idlePose(t, ON);
      const loud = idlePose(t, { enabled: true, speaking: 1 });
      extra = Math.max(extra, Math.abs(loud.head.x - quiet.head.x));
      expect(Math.abs(loud.head.x - quiet.head.x)).toBeLessThanOrEqual(SPEAK_NOD_X + 1e-9);
      expect(idlePose(t, { enabled: true, speaking: -3 })).toEqual(quiet);
      expect(idlePose(t, { enabled: true, speaking: 9 })).toEqual(loud);
    }
    expect(extra).toBeGreaterThan(SPEAK_NOD_X * 0.8);
  });
});
