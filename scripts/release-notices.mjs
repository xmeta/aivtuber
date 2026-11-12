// The third-party notice texts that every release archive must carry in
// THIRD-PARTY-NOTICES.txt (issue #349). Shared by scripts/package-release.mjs
// (builds the file; fails on a missing text) and
// scripts/smoke-release-archive.mjs (asserts the archive has every text).
//
// The files are vendored under packaging/third-party/. The first line of each
// is "Source: <upstream URL at a pinned revision>"; refresh them from the
// revisions named there when a pinned engine version changes.
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");

/// Engine-library licence and notice texts, in the order they are written.
export const VENDORED_NOTICES = [
  "sherpa-onnx-LICENSE.txt",
  "onnxruntime-LICENSE.txt",
  "onnxruntime-ThirdPartyNotices.txt",
  "kaldi-native-fbank-LICENSE.txt",
  "kaldi-decoder-LICENSE.txt",
  "openfst-COPYING.txt",
  "simple-sentencepiece-LICENSE.txt",
  "utf8proc-LICENSE.md.txt",
  "eigen-COPYING.MPL2.txt",
  "eigen-COPYING.README.txt",
  "eigen-COPYING.BSD.txt",
  "eigen-COPYING.APACHE.txt",
  "eigen-COPYING.MINPACK.txt",
];

/// The text the packager used to write into the renderer notices when a
/// package shipped no licence file. It must never appear in an archive.
export const MISSING_LICENCE_STUB = "no licence file shipped in the package";

/// Reads one vendored text; throws when it is missing, empty or lacks its
/// "Source:" header (a stub or a hand-truncated file must not ship).
export function readVendoredNotice(name) {
  const path = join(repoRoot, "packaging", "third-party", name);
  if (!existsSync(path)) throw new Error(`vendored notice packaging/third-party/${name} is missing`);
  const text = readFileSync(path, "utf8").replace(/\r\n/g, "\n");
  if (!text.startsWith("Source: https://") || text.trim().length < 200) {
    throw new Error(`vendored notice packaging/third-party/${name} is empty or has no "Source:" header`);
  }
  return text.trimEnd();
}

/// The "==== <file> ====" section heading used in THIRD-PARTY-NOTICES.txt.
export const heading = (name) => `==== ${name} ====`;
