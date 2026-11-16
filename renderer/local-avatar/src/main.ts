// Local VRM renderer page (ADR-0003, issue #219). WebSocket client of the
// Rust host's `local-avatar/1` endpoint. The host owns ordering and
// authorization; this page only interpolates toward the commanded state.
//
// Page query parameters:
//   token  per-launch secret from the runtime's launch URL (required)
//   model  same-origin path of a VRM file to load instead of the host's active
//          model (default: the active model, `/avatar/model.vrm`, which the
//          operation panel can change; the page then reloads itself)
//   debug  "1" exposes a read-only state snapshot at window.__avatarDebug
//          (used by the headless smoke test; carries no secret)

import * as THREE from "three";
import { GLTFLoader } from "three/addons/loaders/GLTFLoader.js";
import { VRMLoaderPlugin, VRMUtils, type VRM } from "@pixiv/three-vrm";
import { AvatarState, MAX_STEP_S } from "./avatar-state";
import { Controller, backoffMs } from "./controller";
import { EXPRESSIONS, GAZE_ANGLES, PROFILE_ID, VISEME_EXPRESSIONS } from "./profile";
import { MAX_FRAME_BYTES, encodeRendererFrame } from "./protocol";
import {
  RELOAD_POLL_MS,
  activeIdUrl,
  activeModelUrl,
  modelIdFromHeader,
  MODEL_ID_HEADER,
  shouldReload,
} from "./model-source";

const MAX_MODEL_BYTES = 64 * 1024 * 1024; // same ceiling as the host's static server
const params = new URLSearchParams(location.search);
const token = params.get("token") ?? "";
const debug = params.get("debug") === "1";

/// Same-origin, host-allowlisted path characters only (mirrors the Rust
/// static server), so a crafted `model` value cannot reach another origin.
function resolveModelUrl(raw: string | null): URL {
  const url = new URL(raw ?? "model.vrm", location.href);
  if (url.origin !== location.origin || !/^\/[A-Za-z0-9._\-/]+$/.test(url.pathname)) {
    throw new Error("model must be a same-origin path of [A-Za-z0-9._-/]");
  }
  if (url.pathname.split("/").includes("..")) throw new Error("model path must not contain ..");
  url.search = "";
  url.hash = "";
  return url;
}

async function sha256Hex(bytes: ArrayBuffer): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, "0")).join("");
}

// ---- scene ----------------------------------------------------------------

const renderer = new THREE.WebGLRenderer({ alpha: true, antialias: true, premultipliedAlpha: true });
renderer.setClearColor(0x000000, 0); // transparent for OBS / compositing
renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
document.body.appendChild(renderer.domElement);
const scene = new THREE.Scene();
const camera = new THREE.PerspectiveCamera(22, 1, 0.1, 20);
const light = new THREE.DirectionalLight(0xffffff, Math.PI);
light.position.set(1, 1, 1).normalize();
scene.add(light);
const gazeTarget = new THREE.Object3D();
scene.add(gazeTarget);

const REST_ARM_RAD = 1.2;
const GAZE_DISTANCE_M = 2;
const FOCUS_Y_M = 1.3;

function fitViewport(): void {
  const width = Math.max(1, window.innerWidth);
  const height = Math.max(1, window.innerHeight);
  renderer.setSize(width, height);
  camera.aspect = width / height;
  // Keep at least ~0.7 m of horizontal coverage in portrait sources.
  const halfFov = THREE.MathUtils.degToRad(camera.fov / 2);
  const distance = Math.max(2.2, 0.7 / camera.aspect / (2 * Math.tan(halfFov)));
  camera.position.set(0, FOCUS_Y_M + 0.05, distance);
  camera.lookAt(0, FOCUS_Y_M, 0);
  camera.updateProjectionMatrix();
}
fitViewport();
window.addEventListener("resize", fitViewport);

// ---- state ----------------------------------------------------------------

const state = new AvatarState();
let vrm: VRM | null = null;
let controller: Controller | null = null;
let ws: WebSocket | null = null;
let frameCount = 0;
let animClockS = 0; // animation clock advanced by frame deltas, not wall time
let lastTickMs: number | null = null;

function applyToModel(model: VRM, dt: number): void {
  const em = model.expressionManager;
  if (em) {
    for (const [preset, vrmName] of Object.entries(EXPRESSIONS)) {
      em.setValue(vrmName, state.current.expressions[preset as keyof typeof EXPRESSIONS]);
    }
    for (const [viseme, vrmName] of Object.entries(VISEME_EXPRESSIONS)) {
      em.setValue(vrmName, state.current.visemes[viseme as keyof typeof VISEME_EXPRESSIONS]);
    }
    // Renderer-local idle blink: fixed period on the animation clock.
    em.setValue("blink", animClockS % 4 > 3.88 ? 1 : 0);
  }
  gazeTarget.position.set(
    Math.tan(THREE.MathUtils.degToRad(state.current.yaw)) * GAZE_DISTANCE_M,
    FOCUS_Y_M + Math.tan(THREE.MathUtils.degToRad(state.current.pitch)) * GAZE_DISTANCE_M,
    GAZE_DISTANCE_M,
  );
  const head = model.humanoid.getNormalizedBoneNode("head");
  const chest = model.humanoid.getNormalizedBoneNode("chest");
  const pose = state.headPose();
  if (head !== null) head.rotation.set(pose.x, pose.y, 0);
  // Static relaxed rest pose: VRM models are authored in a T-pose. VRMA clips
  // for real body motion are out of scope (ADR-0003 known limitations).
  const leftArm = model.humanoid.getNormalizedBoneNode("leftUpperArm");
  const rightArm = model.humanoid.getNormalizedBoneNode("rightUpperArm");
  if (leftArm !== null) leftArm.rotation.z = -REST_ARM_RAD;
  if (rightArm !== null) rightArm.rotation.z = REST_ARM_RAD;
  if (chest !== null) chest.rotation.x = 0.01 * Math.sin((animClockS * Math.PI * 2) / 7.5);
  model.update(dt);
}

function tick(nowMs: number): void {
  requestAnimationFrame(tick);
  if (vrm === null) return;
  const dt = lastTickMs === null ? 0 : Math.min(MAX_STEP_S, Math.max(0, (nowMs - lastTickMs) / 1000));
  lastTickMs = nowMs;
  animClockS += dt;
  state.step(dt);
  applyToModel(vrm, dt);
  renderer.render(scene, camera);
  frameCount += 1;
  controller?.onFrameRendered();
}

// ---- connection -----------------------------------------------------------

function connect(attempt: number): void {
  const socket = new WebSocket(`ws://${location.host}/ws?token=${encodeURIComponent(token)}`);
  ws = socket;
  socket.onopen = () => {
    controller?.onOpen();
  };
  socket.onmessage = (event) => {
    if (typeof event.data === "string" && event.data.length <= MAX_FRAME_BYTES) {
      controller?.onMessage(event.data);
    }
  };
  socket.onclose = () => {
    if (ws === socket) ws = null;
    // A session that reached `ready` was healthy: restart the backoff.
    const next = controller?.isReady ? 0 : attempt + 1;
    controller?.onClose(); // snap to neutral before anything else
    setTimeout(() => connect(next), backoffMs(next));
  };
}

// ---- boot -----------------------------------------------------------------

function summarizeMeta(model: VRM): Record<string, unknown> {
  // VRM license metadata is the model's own license statement; surface it so
  // the operator can see what they are streaming.
  const meta = model.meta as unknown as Record<string, unknown>;
  const pick = [
    "metaVersion", "name", "authors", "licenseUrl", "avatarPermission", "commercialUsage",
    "allowRedistribution", "modification", "creditNotation", "licenseName", "version",
  ];
  return Object.fromEntries(pick.filter((k) => meta[k] !== undefined).map((k) => [k, meta[k]]));
}

interface FetchedModel {
  bytes: ArrayBuffer;
  /** The id the host gave the active model (null for an explicit `model` path). */
  activeId: string | null;
  base: string;
}

async function fetchModel(fallback: boolean): Promise<FetchedModel> {
  const explicit = params.get("model");
  const url = explicit !== null ? resolveModelUrl(explicit).toString() : activeModelUrl(token, fallback);
  const response = await fetch(url, { credentials: "omit", cache: "no-store" });
  if (!response.ok) throw new Error(`model fetch failed: HTTP ${response.status}`);
  const bytes = await response.arrayBuffer();
  if (bytes.byteLength > MAX_MODEL_BYTES) throw new Error("model exceeds size limit");
  return {
    bytes,
    activeId: explicit !== null ? null : modelIdFromHeader(response.headers.get(MODEL_ID_HEADER)),
    base: explicit !== null ? new URL(url).pathname.replace(/[^/]*$/, "") : "/avatar/",
  };
}

async function parseVrm(fetched: FetchedModel): Promise<VRM> {
  const loader = new GLTFLoader();
  loader.register((parser) => new VRMLoaderPlugin(parser));
  const gltf = await loader.parseAsync(fetched.bytes, fetched.base);
  const loaded = gltf.userData["vrm"] as VRM | undefined;
  if (loaded === undefined) throw new Error("file is not a VRM model");
  return loaded;
}

/**
 * Reloads the page when the host's active model changes (the operator chose
 * another in the panel). One small authenticated request per interval; a failed
 * or malformed answer does nothing.
 */
function watchActiveModel(baseline: string | null): void {
  if (baseline === null || params.get("model") !== null) return;
  setInterval(() => {
    fetch(activeIdUrl(token), { credentials: "omit", cache: "no-store" })
      .then((response) => (response.ok ? response.json() : null))
      .then((body: unknown) => {
        if (shouldReload(baseline, body)) location.reload();
      })
      .catch(() => {
        // the host is restarting or busy; the next poll tries again
      });
  }, RELOAD_POLL_MS);
}

async function boot(): Promise<void> {
  let fetched = await fetchModel(false);
  let loaded: VRM;
  try {
    loaded = await parseVrm(fetched);
  } catch (error) {
    // The operator's model passed the host's checks but this page cannot
    // decode it. Showing the shipped sample keeps the stream alive; the hello
    // below reports the sample's hash, so the panel can tell the operator that
    // the page is not showing the model they chose.
    if (fetched.activeId === null) throw error;
    console.error("[local-avatar] the active model could not be loaded; showing the sample:", error);
    const activeId = fetched.activeId;
    fetched = { ...(await fetchModel(true)), activeId };
    loaded = await parseVrm(fetched);
  }
  const modelHash = await sha256Hex(fetched.bytes);
  VRMUtils.rotateVRM0(loaded);
  if (loaded.lookAt) loaded.lookAt.target = gazeTarget;
  scene.add(loaded.scene);
  vrm = loaded;
  console.info("[local-avatar] model loaded", { sha256: modelHash, meta: summarizeMeta(loaded) });

  controller = new Controller({
    state,
    newEpoch: () => crypto.randomUUID(),
    modelHash,
    profile: PROFILE_ID,
    send: (frame) => {
      if (ws !== null && ws.readyState === WebSocket.OPEN) ws.send(encodeRendererFrame(frame));
    },
  });
  if (debug) installDebugHook(loaded, modelHash);
  requestAnimationFrame(tick);
  connect(0);
  watchActiveModel(fetched.activeId);
}

function installDebugHook(model: VRM, modelHash: string): void {
  // Read-only: returns copies, accepts no input.
  const hook = {
    snapshot: () => {
      const em = model.expressionManager;
      const value = (vrmName: string): number => em?.getValue(vrmName) ?? 0;
      return {
        epoch: controller?.currentEpoch ?? null,
        lastSeq: controller?.lastAcceptedSeq ?? 0,
        ready: controller?.isReady ?? false,
        pendingAcks: controller?.pendingAckCount ?? 0,
        connected: ws !== null && ws.readyState === WebSocket.OPEN,
        frame: frameCount,
        modelHash,
        expressions: Object.fromEntries(
          Object.entries(EXPRESSIONS).map(([preset, vrmName]) => [preset, value(vrmName)]),
        ),
        visemes: Object.fromEntries(
          Object.entries(VISEME_EXPRESSIONS).map(([viseme, vrmName]) => [viseme, value(vrmName)]),
        ),
        gaze: { yaw: state.current.yaw, pitch: state.current.pitch },
        gazeTarget: { yaw: state.target.yaw, pitch: state.target.pitch },
        gazeAngles: GAZE_ANGLES,
        meta: summarizeMeta(model),
      };
    },
  };
  Object.defineProperty(window, "__avatarDebug", { value: Object.freeze(hook) });
}

boot().catch((error: unknown) => {
  // No connection is attempted without a rendered model, so the host keeps
  // reporting `disconnected` rather than a renderer that cannot draw.
  console.error("[local-avatar] boot failed:", error);
});
