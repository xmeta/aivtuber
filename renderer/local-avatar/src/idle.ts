// Renderer-local idle life (issue #383): a pure, procedural pose layer.
//
// Renderer-private presentation, like the old fixed blink: it is NOT host
// state, never crosses the `local-avatar/1` wire and is not replay-relevant.
// No three.js, no DOM, no wall clock, no Math.random. The pose is a pure
// function of the animation clock `tS` (seconds) and a small input, so the
// same clock always yields the same pose regardless of call history: blinks
// and saccades come from a fixed-seed hash of a slot index, not from mutable
// state.
//
// Axis conventions (VRM normalized bones, model facing +Z, character-left on
// +X, as the pre-existing rest pose assumed: left arm z negative lowers it):
//   upper arm   z = swing down (left -, right +); x negative = swing forward
//   lower arm   y = elbow flex toward the front (left -, right +)
//   shoulder    z = lift (left +, right -)
//   chest/spine x negative = lean back (inhale)
// Rotations are Euler XYZ radians, applied as absolute local rotations.
//
// Disabled: the REST POSE still applies (it replaces the old stiff A-pose and
// is not motion); only the time-varying motion, the blink and the gaze
// offsets are switched off.

export interface Vec3 {
  x: number;
  y: number;
  z: number;
}

export interface IdleInput {
  /// Master switch for motion (`?idle=0` turns it off).
  enabled: boolean;
  /// Speaking level 0..1 (e.g. the largest current viseme weight). Clamped.
  speaking: number;
}

export interface IdlePose {
  hips: Vec3;
  spine: Vec3;
  chest: Vec3;
  neck: Vec3;
  head: Vec3;
  leftShoulder: Vec3;
  rightShoulder: Vec3;
  leftUpperArm: Vec3;
  rightUpperArm: Vec3;
  leftLowerArm: Vec3;
  rightLowerArm: Vec3;
  leftHand: Vec3;
  rightHand: Vec3;
  /// Blink weight 0..1 (0 open, 1 closed).
  blink: number;
  /// Micro-saccade gaze offset in degrees, added to the host-commanded gaze.
  gazeYawDeg: number;
  gazePitchDeg: number;
}

export type IdleBone = Exclude<keyof IdlePose, "blink" | "gazeYawDeg" | "gazePitchDeg">;
export const IDLE_BONES: readonly IdleBone[] = [
  "hips", "spine", "chest", "neck", "head", "leftShoulder", "rightShoulder",
  "leftUpperArm", "rightUpperArm", "leftLowerArm", "rightLowerArm", "leftHand", "rightHand",
];

// ---- named constants (radians unless noted; design values, not measured) ----

/// Fixed PRNG seed: the same avatar blinks and glances the same way every run.
export const IDLE_SEED = 0x5eed1de1;

// Rest pose (applies even when motion is disabled).
export const REST_UPPER_ARM_Z = 1.3; // from T-pose, arms close to the body
export const REST_UPPER_ARM_FORWARD_X = -0.1; // slight forward rotation
export const REST_LOWER_ARM_FLEX_Y = 0.22; // forearm bend
export const REST_HAND_Z = 0.1; // relaxed wrist, toward the thigh
export const REST_SHOULDER_DROP_Z = 0.05;

// Breathing.
export const BREATH_PERIOD_S = 4.0;
export const BREATH_CHEST_X = 0.03;
export const BREATH_SPINE_X = 0.02;
export const BREATH_SHOULDER_LIFT_Z = 0.012;
export const BREATH_ARM_Z = 0.008;
export const BREATH_DEPTH_PERIOD_S = 23; // slow modulation of breath depth
export const BREATH_DEPTH_MIN = 0.8;

// Weight-shift sway (incommensurate periods).
export const SWAY_HIPS_ROLL_Z = 0.025;
export const SWAY_HIPS_ROLL_PERIOD_S = 13.1;
export const SWAY_HIPS_YAW_Y = 0.03;
export const SWAY_HIPS_YAW_PERIOD_S = 17.3;
export const SWAY_SPINE_ROLL_Z = 0.015; // counter-roll
export const SWAY_SPINE_ROLL_PERIOD_S = 13.1;
export const SWAY_SPINE_YAW_Y = 0.012;
export const SWAY_SPINE_YAW_PERIOD_S = 9.7;

// Head drift (sum of sines with incommensurate periods).
export const DRIFT_HEAD_X = [0.02, 0.01] as const;
export const DRIFT_HEAD_X_PERIOD_S = [7.3, 3.1] as const;
export const DRIFT_HEAD_Y = [0.03, 0.015, 0.008] as const;
export const DRIFT_HEAD_Y_PERIOD_S = [11.3, 4.7, 2.9] as const;
export const DRIFT_HEAD_Z = [0.015, 0.006] as const;
export const DRIFT_HEAD_Z_PERIOD_S = [8.9, 3.7] as const;
export const NECK_SHARE = 0.5; // neck takes this share of the head drift

// Arm life (tiny, so the hanging arms are not frozen).
export const ARM_DRIFT_Z = 0.012;
export const ARM_DRIFT_PERIOD_S = 6.7;
export const FOREARM_DRIFT_Y = 0.03;
export const FOREARM_DRIFT_PERIOD_S = 8.3;

// Blinks: one slot per BLINK_SLOT_S; the first blink starts at a seeded jitter
// in [0, BLINK_JITTER_S) so intervals between slot starts are
// BLINK_SLOT_S +/- BLINK_JITTER_S, i.e. 2..6 s.
export const BLINK_SLOT_S = 4;
export const BLINK_JITTER_S = 2;
export const BLINK_DURATION_MIN_S = 0.12;
export const BLINK_DURATION_MAX_S = 0.18;
export const BLINK_DOUBLE_PROBABILITY = 0.12;
export const BLINK_DOUBLE_GAP_S = 0.15; // open gap between the two blinks of a double
/// Host expressions that already narrow or close the eyes (`joy`=happy,
/// `calm`=relaxed, `sadness`=sad). The idle blink is scaled by
/// 1 - max(those weights), so a full host expression suppresses it and the
/// eyes never double-close.
export function blinkScaleForHost(narrowingExpressionWeights: readonly number[]): number {
  let m = 0;
  for (const w of narrowingExpressionWeights) if (Number.isFinite(w)) m = Math.max(m, w);
  return clamp(1 - m, 0, 1);
}

// Micro saccades: one slot per SACCADE_SLOT_S; the gaze changes at a seeded
// point in [0, SACCADE_CHANGE_SPAN_S] of the slot, so holds last
// SLOT +/- SPAN = 0.4..2.0 s.
export const SACCADE_SLOT_S = 1.2;
export const SACCADE_CHANGE_SPAN_S = 0.8;
export const SACCADE_RAMP_S = 0.05;
export const SACCADE_MAX_DEG = 2;

// Speaking emphasis (scaled by the speaking level, 0..1).
export const SPEAK_NOD_X = 0.04;
export const SPEAK_NOD_PERIOD_S = 0.9;
export const SPEAK_TILT_Z = 0.03;
export const SPEAK_TILT_PERIOD_S = 2.3;
export const SPEAK_CHEST_X = 0.012;

// Output bounds (absolute radians per component, rest pose included).
export const MAX_LIMB_RAD = 1.6; // arms and hands
export const MAX_TORSO_RAD = 0.25; // hips, spine, chest, neck, head, shoulders

// ---- seeded hash --------------------------------------------------------------

/// mulberry32 mixing step over (seed, n, salt): deterministic uniform [0,1).
export function hash01(n: number, salt: number): number {
  let a = (IDLE_SEED ^ Math.imul(n | 0, 0x9e3779b1) ^ Math.imul(salt | 0, 0x85ebca6b)) >>> 0;
  a = (a + 0x6d2b79f5) >>> 0;
  let t = Math.imul(a ^ (a >>> 15), 1 | a);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}

const TAU = Math.PI * 2;
const clamp = (x: number, lo: number, hi: number): number => Math.min(hi, Math.max(lo, x));
const sine = (tS: number, periodS: number, phase: number): number => Math.sin((TAU * tS) / periodS + phase);
const smoothstep = (x: number): number => {
  const c = clamp(x, 0, 1);
  return c * c * (3 - 2 * c);
};
/// Fixed phase per named channel so sines do not all start at zero together.
const phase = (channel: number): number => hash01(channel, 7) * TAU;

// ---- blinks -------------------------------------------------------------------

export interface BlinkSlot {
  /// Start of the first blink (seconds on the animation clock).
  startS: number;
  durationS: number;
  /// Start of the second blink of a double blink, or null.
  secondStartS: number | null;
}

/// The blink scheduled in slot `k` (k >= 0). A pure function of `k`.
export function blinkSlot(k: number): BlinkSlot {
  const startS = k * BLINK_SLOT_S + hash01(k, 1) * BLINK_JITTER_S;
  const durationS = BLINK_DURATION_MIN_S + hash01(k, 2) * (BLINK_DURATION_MAX_S - BLINK_DURATION_MIN_S);
  const isDouble = hash01(k, 3) < BLINK_DOUBLE_PROBABILITY;
  return { startS, durationS, secondStartS: isDouble ? startS + durationS + BLINK_DOUBLE_GAP_S : null };
}

/// Closed fraction at the given offset into one blink: a smooth close and open.
function blinkCurve(offsetS: number, durationS: number): number {
  if (offsetS < 0 || offsetS > durationS) return 0;
  const s = Math.sin((Math.PI * offsetS) / durationS);
  return s * s;
}

/// Raw blink weight 0..1 at clock `tS`. A slot's blinks end at most
/// JITTER + 2*MAX_DURATION + GAP < SLOT after the slot start, so only the
/// current slot can be active.
export function blinkWeight(tS: number): number {
  if (!(tS >= 0)) return 0;
  const slot = blinkSlot(Math.floor(tS / BLINK_SLOT_S));
  let w = blinkCurve(tS - slot.startS, slot.durationS);
  if (slot.secondStartS !== null) w = Math.max(w, blinkCurve(tS - slot.secondStartS, slot.durationS));
  return clamp(w, 0, 1);
}

// ---- saccades -----------------------------------------------------------------

function saccadeValue(k: number): { yaw: number; pitch: number } {
  return {
    yaw: (hash01(k, 11) * 2 - 1) * SACCADE_MAX_DEG,
    pitch: (hash01(k, 12) * 2 - 1) * SACCADE_MAX_DEG * 0.6, // vertical glances are smaller
  };
}

/// Gaze offset (degrees) at clock `tS`, a pure function of `tS`.
export function saccadeOffset(tS: number): { yaw: number; pitch: number } {
  if (!(tS >= 0)) return { yaw: 0, pitch: 0 };
  const k = Math.floor(tS / SACCADE_SLOT_S);
  const changeAt = k * SACCADE_SLOT_S + hash01(k, 10) * SACCADE_CHANGE_SPAN_S;
  const prev = saccadeValue(k - 1);
  const next = saccadeValue(k);
  const a = smoothstep((tS - changeAt) / SACCADE_RAMP_S);
  return { yaw: prev.yaw + (next.yaw - prev.yaw) * a, pitch: prev.pitch + (next.pitch - prev.pitch) * a };
}

// ---- pose ---------------------------------------------------------------------

const v = (x = 0, y = 0, z = 0): Vec3 => ({ x, y, z });

function restPose(): IdlePose {
  return {
    hips: v(), spine: v(), chest: v(), neck: v(), head: v(),
    leftShoulder: v(0, 0, -REST_SHOULDER_DROP_Z),
    rightShoulder: v(0, 0, REST_SHOULDER_DROP_Z),
    leftUpperArm: v(REST_UPPER_ARM_FORWARD_X, 0, -REST_UPPER_ARM_Z),
    rightUpperArm: v(REST_UPPER_ARM_FORWARD_X, 0, REST_UPPER_ARM_Z),
    leftLowerArm: v(0, -REST_LOWER_ARM_FLEX_Y, 0),
    rightLowerArm: v(0, REST_LOWER_ARM_FLEX_Y, 0),
    leftHand: v(0, 0, -REST_HAND_Z),
    rightHand: v(0, 0, REST_HAND_Z),
    blink: 0,
    gazeYawDeg: 0,
    gazePitchDeg: 0,
  };
}

function clampPose(p: IdlePose): IdlePose {
  for (const bone of IDLE_BONES) {
    const limit = bone.includes("Arm") || bone.includes("Hand") ? MAX_LIMB_RAD : MAX_TORSO_RAD;
    const r = p[bone];
    r.x = clamp(r.x, -limit, limit);
    r.y = clamp(r.y, -limit, limit);
    r.z = clamp(r.z, -limit, limit);
  }
  p.blink = clamp(p.blink, 0, 1);
  p.gazeYawDeg = clamp(p.gazeYawDeg, -SACCADE_MAX_DEG, SACCADE_MAX_DEG);
  p.gazePitchDeg = clamp(p.gazePitchDeg, -SACCADE_MAX_DEG, SACCADE_MAX_DEG);
  return p;
}

/// Pure idle pose at animation clock `tS` seconds. Non-finite or negative
/// clocks and out-of-range speaking levels are treated as 0 / clamped.
export function idlePose(tS: number, input: IdleInput): IdlePose {
  const p = restPose();
  if (!input.enabled || !Number.isFinite(tS) || tS < 0) return clampPose(p);
  const speaking = Number.isFinite(input.speaking) ? clamp(input.speaking, 0, 1) : 0;

  // Breathing: smooth raised-cosine, 0 (exhaled) .. 1 (inhaled), slow depth variation.
  const depth = BREATH_DEPTH_MIN + (1 - BREATH_DEPTH_MIN) * (0.5 + 0.5 * sine(tS, BREATH_DEPTH_PERIOD_S, phase(1)));
  const breath = 0.5 - 0.5 * Math.cos((TAU * tS) / BREATH_PERIOD_S);
  const b = breath * depth;
  p.chest.x -= BREATH_CHEST_X * b;
  p.spine.x -= BREATH_SPINE_X * b;
  p.leftShoulder.z += BREATH_SHOULDER_LIFT_Z * b;
  p.rightShoulder.z -= BREATH_SHOULDER_LIFT_Z * b;
  p.leftUpperArm.z -= BREATH_ARM_Z * b;
  p.rightUpperArm.z += BREATH_ARM_Z * b;

  // Weight shift.
  p.hips.z += SWAY_HIPS_ROLL_Z * sine(tS, SWAY_HIPS_ROLL_PERIOD_S, phase(2));
  p.hips.y += SWAY_HIPS_YAW_Y * sine(tS, SWAY_HIPS_YAW_PERIOD_S, phase(3));
  p.spine.z -= SWAY_SPINE_ROLL_Z * sine(tS, SWAY_SPINE_ROLL_PERIOD_S, phase(2));
  p.spine.y += SWAY_SPINE_YAW_Y * sine(tS, SWAY_SPINE_YAW_PERIOD_S, phase(4));

  // Head drift, shared with the neck.
  let hx = 0;
  let hy = 0;
  let hz = 0;
  DRIFT_HEAD_X.forEach((a, i) => (hx += a * sine(tS, DRIFT_HEAD_X_PERIOD_S[i]!, phase(20 + i))));
  DRIFT_HEAD_Y.forEach((a, i) => (hy += a * sine(tS, DRIFT_HEAD_Y_PERIOD_S[i]!, phase(30 + i))));
  DRIFT_HEAD_Z.forEach((a, i) => (hz += a * sine(tS, DRIFT_HEAD_Z_PERIOD_S[i]!, phase(40 + i))));
  p.head.x += hx * (1 - NECK_SHARE);
  p.head.y += hy * (1 - NECK_SHARE);
  p.head.z += hz * (1 - NECK_SHARE);
  p.neck.x += hx * NECK_SHARE;
  p.neck.y += hy * NECK_SHARE;
  p.neck.z += hz * NECK_SHARE;

  // Arms: tiny independent drift.
  p.leftUpperArm.z -= ARM_DRIFT_Z * sine(tS, ARM_DRIFT_PERIOD_S, phase(50));
  p.rightUpperArm.z += ARM_DRIFT_Z * sine(tS, ARM_DRIFT_PERIOD_S, phase(51));
  p.leftLowerArm.y -= FOREARM_DRIFT_Y * sine(tS, FOREARM_DRIFT_PERIOD_S, phase(52));
  p.rightLowerArm.y += FOREARM_DRIFT_Y * sine(tS, FOREARM_DRIFT_PERIOD_S, phase(53));

  // Speaking emphasis.
  if (speaking > 0) {
    p.head.x += speaking * SPEAK_NOD_X * sine(tS, SPEAK_NOD_PERIOD_S, 0);
    p.head.z += speaking * SPEAK_TILT_Z * sine(tS, SPEAK_TILT_PERIOD_S, phase(60));
    p.chest.x += speaking * SPEAK_CHEST_X * sine(tS, SPEAK_NOD_PERIOD_S, 0.6);
  }

  p.blink = blinkWeight(tS);
  const g = saccadeOffset(tS);
  p.gazeYawDeg = g.yaw;
  p.gazePitchDeg = g.pitch;
  return clampPose(p);
}
