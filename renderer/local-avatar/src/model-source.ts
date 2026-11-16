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
