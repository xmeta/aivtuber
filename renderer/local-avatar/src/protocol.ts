// `local-avatar/1` wire types for the renderer side. The authoritative
// definition is crates/adapters/src/local_avatar/protocol.rs and its fixtures
// in crates/adapters/tests/fixtures/local-avatar-1/. Host frames are parsed
// defensively here even though the host validates them too: this page must
// never trust whatever answers on its WebSocket.

export const PROTOCOL_VERSION = 1;
/// Same bound as the host: one frame in either direction.
export const MAX_FRAME_BYTES = 4096;

export type Viseme = "aa" | "ih" | "ou" | "ee" | "oh" | "neutral";
export const VISEMES: readonly Viseme[] = ["aa", "ih", "ou", "ee", "oh", "neutral"];

export type GazeTarget = "camera" | "chat" | "game" | "speaker" | "away";
export const GAZE_TARGETS: readonly GazeTarget[] = ["camera", "chat", "game", "speaker", "away"];

export type RejectReason =
  | "stale_epoch"
  | "stale_seq"
  | "unknown_preset"
  | "invalid_argument"
  | "not_ready"
  | "other";

export type WireCommand =
  | { op: "expression"; preset: string; intensity: number }
  | { op: "gesture"; preset: string; amplitude: number }
  | { op: "gaze"; target: GazeTarget }
  | { op: "viseme"; viseme: Viseme; weight: number }
  | { op: "reset" };

export interface HostFrame {
  v: number;
  epoch: string;
  seq: number;
  cmd: WireCommand;
}

export type RendererFrame =
  | { type: "hello"; v: number; epoch: string; model_hash: string; profile: string }
  | { type: "ready"; epoch: string }
  | { type: "ack"; epoch: string; seq: number }
  | { type: "reject"; epoch: string; seq: number; reason: RejectReason };

/// Result of decoding one inbound text frame. `invalid` carries whatever
/// `epoch`/`seq` could be recovered so the renderer can still answer with a
/// typed reject instead of silently dropping a command the host is tracking.
export type Decoded =
  | { kind: "frame"; frame: HostFrame }
  | { kind: "invalid"; epoch: string | null; seq: number | null; reason: RejectReason }
  | { kind: "ignore" };

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

/// Epoch / preset ids: same shape the host accepts (short, printable, no
/// separators).
const isId = (value: unknown): value is string =>
  typeof value === "string" && /^[A-Za-z0-9._@-]{1,64}$/.test(value);

const hasOnly = (value: Record<string, unknown>, keys: readonly string[]): boolean =>
  Object.keys(value).every((k) => keys.includes(k));

const isSeq = (value: unknown): value is number =>
  typeof value === "number" && Number.isSafeInteger(value) && value >= 1;

/// Shape-checks a command. Numeric ranges are the renderer's second line of
/// defence (the host already validates them); preset vocabularies are checked
/// against the renderer profile by the caller, not here.
function decodeCommand(raw: unknown): WireCommand | null {
  if (!isRecord(raw) || typeof raw["op"] !== "string") return null;
  // Closed field sets, as in the host (`deny_unknown_fields`).
  const fields: Record<string, readonly string[]> = {
    expression: ["op", "preset", "intensity"],
    gesture: ["op", "preset", "amplitude"],
    gaze: ["op", "target"],
    viseme: ["op", "viseme", "weight"],
    reset: ["op"],
  };
  const allowed = fields[raw["op"]];
  if (allowed === undefined || !hasOnly(raw, allowed)) return null;
  const num = (key: string, max: number): number | null => {
    const value = raw[key];
    return typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= max
      ? value
      : null;
  };
  switch (raw["op"]) {
    case "expression": {
      const intensity = num("intensity", 1);
      return isId(raw["preset"]) && intensity !== null
        ? { op: "expression", preset: raw["preset"], intensity }
        : null;
    }
    case "gesture": {
      const amplitude = num("amplitude", 2);
      return isId(raw["preset"]) && amplitude !== null
        ? { op: "gesture", preset: raw["preset"], amplitude }
        : null;
    }
    case "gaze":
      return GAZE_TARGETS.includes(raw["target"] as GazeTarget)
        ? { op: "gaze", target: raw["target"] as GazeTarget }
        : null;
    case "viseme": {
      const weight = num("weight", 1);
      return VISEMES.includes(raw["viseme"] as Viseme) && weight !== null
        ? { op: "viseme", viseme: raw["viseme"] as Viseme, weight }
        : null;
    }
    case "reset":
      return { op: "reset" };
    default:
      return null;
  }
}

export function decodeHostFrame(text: string): Decoded {
  if (text.length > MAX_FRAME_BYTES) return { kind: "ignore" };
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return { kind: "ignore" };
  }
  if (!isRecord(raw)) return { kind: "ignore" };
  const epoch = isId(raw["epoch"]) ? raw["epoch"] : null;
  const seq = isSeq(raw["seq"]) ? raw["seq"] : null;
  if (
    raw["v"] !== PROTOCOL_VERSION ||
    epoch === null ||
    seq === null ||
    !hasOnly(raw, ["v", "epoch", "seq", "cmd"])
  ) {
    // Without both epoch and seq there is nothing the host could be waiting on.
    return { kind: "ignore" };
  }
  const cmd = decodeCommand(raw["cmd"]);
  if (cmd === null) {
    const op = isRecord(raw["cmd"]) ? raw["cmd"]["op"] : undefined;
    const known = ["expression", "gesture", "gaze", "viseme", "reset"].includes(op as string);
    return { kind: "invalid", epoch, seq, reason: known ? "invalid_argument" : "other" };
  }
  return { kind: "frame", frame: { v: PROTOCOL_VERSION, epoch, seq, cmd } };
}

export function encodeRendererFrame(frame: RendererFrame): string {
  return JSON.stringify(frame);
}
