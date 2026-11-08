// Fetches the sample VRM used by the smoke test, pinned to an upstream commit
// and verified by SHA-256. The model is NEVER committed: its license is the
// metadata embedded in the file (see "Model and rights" in docs/vts-obs-adapters.adoc section 6), not the
// repository's code license.
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const MODEL = {
  repo: "pixiv/three-vrm",
  commit: "3caf7a21ee68f0d5dfa363ab3d577450bbf60136",
  path: "packages/three-vrm/examples/models/VRM1_Constraint_Twist_Sample.vrm",
  sha256: "12c2b97e95e700783a6a550dc0eee2d7880aeedccef9ae67bc4c5a2f0f2631a2",
  bytes: 10776032,
};

const here = dirname(fileURLToPath(import.meta.url));
export const DEFAULT_MODEL_PATH = resolve(here, ".cache", "VRM1_Constraint_Twist_Sample.vrm");

const sha256 = (buf) => createHash("sha256").update(buf).digest("hex");

/// Returns the path of the verified model, downloading it if needed.
export async function ensureModel(dest = DEFAULT_MODEL_PATH) {
  if (existsSync(dest) && sha256(readFileSync(dest)) === MODEL.sha256) return dest;
  const url = `https://raw.githubusercontent.com/${MODEL.repo}/${MODEL.commit}/${MODEL.path}`;
  const response = await fetch(url);
  if (!response.ok) throw new Error(`model download failed: HTTP ${response.status}`);
  const buf = Buffer.from(await response.arrayBuffer());
  const actual = sha256(buf);
  if (buf.length !== MODEL.bytes || actual !== MODEL.sha256) {
    throw new Error(`model checksum mismatch: expected ${MODEL.sha256}, got ${actual} (${buf.length} bytes)`);
  }
  mkdirSync(dirname(dest), { recursive: true });
  writeFileSync(`${dest}.part`, buf);
  renameSync(`${dest}.part`, dest);
  return dest;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  console.log(await ensureModel(process.argv[2] ? resolve(process.argv[2]) : DEFAULT_MODEL_PATH));
}
