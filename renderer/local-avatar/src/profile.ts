// Renderer profile `vrm1-default@1`: the ONLY place raw VRM expression names
// and gaze geometry appear (ADR-0003 hard constraint). The host speaks the
// provider-neutral vocabulary; this table is the private translation.

import type { GazeTarget, Viseme } from "./protocol";

export const PROFILE_ID = "vrm1-default@1";

/// Neutral expression preset -> VRM 1.0 preset expression name.
export const EXPRESSIONS = {
  neutral: "neutral",
  joy: "happy",
  anger: "angry",
  sadness: "sad",
  calm: "relaxed",
  surprise: "surprised",
} as const;
export type ExpressionPreset = keyof typeof EXPRESSIONS;

/// Neutral viseme -> VRM 1.0 viseme expression name. `neutral` has no entry:
/// it means every viseme at zero.
export const VISEME_EXPRESSIONS = {
  aa: "aa",
  ih: "ih",
  ou: "ou",
  ee: "ee",
  oh: "oh",
} as const;
export type MouthViseme = keyof typeof VISEME_EXPRESSIONS;

/// Symbolic gaze target -> [yaw, pitch] in degrees, camera-relative. Applied
/// to a renderer-private lookAt target object; the host never sees angles.
export const GAZE_ANGLES: Readonly<Record<GazeTarget, readonly [number, number]>> = {
  camera: [0, 0],
  chat: [-25, 0],
  game: [20, -15],
  speaker: [0, 10],
  away: [35, 10],
};

export const GESTURES = ["nod", "shake"] as const;
export type GesturePreset = (typeof GESTURES)[number];

export const isExpressionPreset = (value: string): value is ExpressionPreset =>
  Object.hasOwn(EXPRESSIONS, value);
export const isGesturePreset = (value: string): value is GesturePreset =>
  (GESTURES as readonly string[]).includes(value);
export const isMouthViseme = (value: Viseme): value is MouthViseme =>
  Object.hasOwn(VISEME_EXPRESSIONS, value);
