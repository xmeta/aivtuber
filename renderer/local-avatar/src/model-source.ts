// Where the avatar page gets its model and how it notices a change (#362).
// Pure helpers, so bun can test them; main.ts does the fetching.
//
// The host serves the *active* model at `/avatar/model.vrm` and its id (the
// SHA-256 of the file) at `/avatar/active`, both only to a caller that presents
// the per-launch secret. The model response carries the id in a header; the
// page polls the id and reloads itself when it differs, so an operator's choice
// in the panel applies without a restart.

export const ACTIVE_MODEL_PATH = "/avatar/model.vrm";
export const ACTIVE_ID_PATH = "/avatar/active";
export const MODEL_ID_HEADER = "x-avatar-model-id";
/** How often the page asks for the active id. One small loopback request. */
export const RELOAD_POLL_MS = 2000;
const MODEL_ID = /^[0-9a-f]{64}$/;

/** The active model's URL. `fallback` asks for the shipped sample instead. */
export function activeModelUrl(token: string, fallback: boolean): string {
  const query = `token=${encodeURIComponent(token)}${fallback ? "&fallback=sample" : ""}`;
  return `${ACTIVE_MODEL_PATH}?${query}`;
}

export function activeIdUrl(token: string): string {
  return `${ACTIVE_ID_PATH}?token=${encodeURIComponent(token)}`;
}

/**
 * The id from a model response header: 64 lowercase hex characters, or `null`
 * (an older host, no id set yet, or anything else). With `null` the page does
 * not watch for changes.
 */
export function modelIdFromHeader(value: string | null): string | null {
  return value !== null && MODEL_ID.test(value) ? value : null;
}

/**
 * Whether the page must reload, given the id it started from and the body of
 * `/avatar/active`. A malformed answer never reloads.
 */
export function shouldReload(baseline: string | null, body: unknown): boolean {
  if (baseline === null || typeof body !== "object" || body === null) return false;
  const id = (body as { id?: unknown }).id;
  return typeof id === "string" && MODEL_ID.test(id) && id !== baseline;
}

// ---- Missing or refused token (#458) ----------------------------------------

/** Why the page cannot talk to the host, for the visible message. */
export type TokenProblem = "missing" | "rejected";

/** The per-launch secret is 64 lowercase hex characters; anything else cannot be valid. */
export function tokenProblem(token: string): TokenProblem | null {
  return /^[0-9a-f]{64}$/.test(token) ? null : "missing";
}

/**
 * Visible text for the operator (the page shows nothing else when it cannot
 * connect, and an OBS Browser Source has no console). Never includes the token
 * or any part of the address.
 */
export function tokenProblemMessage(problem: TokenProblem): string {
  const head =
    problem === "missing"
      ? "このアバター画面を開くための URL に、合言葉(token)が付いていません。"
      : "この URL の合言葉(token)が正しくないか、古くなっています(アプリを起動し直すたびに変わります)。";
  return `${head} 操作パネルの「OBS 用の URL をコピー」で URL をコピーし、貼り付け直してください。`;
}

/** Marker thrown by the page's model fetch when the host answered 403. */
export class TokenRejectedError extends Error {
  constructor() {
    super("the host refused the token");
    this.name = "TokenRejectedError";
  }
}
