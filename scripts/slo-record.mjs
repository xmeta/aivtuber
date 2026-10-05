// SLO calibration history recorder — issue #71.
//
// Appends trusted main-branch operational SLO reports (`slo-report` output) to
// the `slo-data` branch, so `slo-baseline` has a durable, re-run-invariant
// history to pool. This is the bridge between #58's continuous measurement and
// SLO baseline selection: without it, `slo-baseline` accepts SLO reports that
// no workflow ever persists, and the only way to obtain one is to run the
// benchmark and the report by hand on a laptop — which is not a measured
// baseline anyone can audit later.
//
// Run only from jobs that own write credentials for that branch (push to main).
// Untrusted pull_request jobs must never invoke this script.
//
// Usage:
//   bun scripts/slo-record.mjs --files <slo-report.json>... [--root <dir>]
//
// Layout on the branch — one document per logical run, grouped by the #58
// compatibility series:
//   reports/<series>/<run>.json
//
// `<series>` encodes the same compatibility boundary the benchmark history
// uses (mode + dataset_id + config_version + seed), through the *same* helper,
// so the two histories cannot disagree about what a series is. `<run>` is
// derived from the report's `source.run_id`.
//
// Identity, mirroring #58's `recording.run_id`:
//   - re-recording a run id REPLACES that run's artifact, so a re-run of one
//     workflow run is idempotent and the branch never holds two artifacts for
//     one run. That is what lets `slo-baseline` pool without ever having to
//     choose between conflicting attempts: the recorder's latest append is the
//     authoritative one, exactly as `benchmark-record.mjs` defines it for the
//     benchmark history;
//   - distinct run ids accumulate, so repeated nightly runs of one revision
//     become a baseline even when their reports are byte-identical.
//
// Security: an SLO report carries indicator counts, latency percentiles, and
// dataset/config/commit identity. It carries no prompts, generated content, or
// secrets, so nothing sensitive can leak into history.

import { readFileSync, writeFileSync, mkdirSync, existsSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { join, dirname, isAbsolute, relative } from "node:path";
import { fileURLToPath } from "node:url";

import { compatKey, seriesFileStem, stableHash } from "./benchmark-record.mjs";

const scriptRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const BRANCH = "slo-data";

function parseArgs(argv) {
  const args = { files: [], root: undefined };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--root") {
      args.root = argv[++i];
      continue;
    }
    if (argv[i] === "--files") {
      i++;
      while (i < argv.length && !argv[i].startsWith("--")) args.files.push(argv[i++]);
      i--;
      continue;
    }
    console.error(`unknown argument ${argv[i]}`);
    process.exit(1);
  }
  if (args.files.length === 0) {
    console.error("usage: slo-record.mjs --files <slo-report.json>... [--root <dir>]");
    process.exit(1);
  }
  return args;
}

/// The #58 compatibility series a report was measured in, derived from the
/// report's own `source` block and rendered through the benchmark history's
/// helper so both histories key on exactly the same string.
export function seriesKeyOf(report) {
  const source = report?.source ?? {};
  return compatKey({
    mode: source.mode,
    dataset_id: source.dataset_id,
    configuration: { config_version: source.config_version, seed: source.seed },
  });
}

/// Filesystem-safe, collision-free stem for one run identity.
export function runFileStem(runId) {
  const slug =
    String(runId)
      .replace(/[^a-zA-Z0-9._-]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 60) || "run";
  return `${slug}-${stableHash(String(runId))}`;
}

/// The single path a report occupies on the history branch. One path per
/// (series, run identity) is the whole point: it is what makes a re-record
/// idempotent and leaves `slo-baseline` nothing to disambiguate.
export function reportPathOf(report) {
  return `reports/${seriesFileStem(seriesKeyOf(report))}/${runFileStem(runIdOf(report))}.json`;
}

export function runIdOf(report) {
  const runId = report?.source?.run_id;
  return typeof runId === "string" ? runId.trim() : "";
}

/// Refuse anything that is not a recordable measurement. This is deliberately
/// stricter than "it parsed as JSON": a `benchmark-result.json` reaching this
/// script would otherwise be persisted as if it were calibration evidence, and
/// the persisted history is what a target's number is later justified by.
export function requireRecordable(report, file) {
  const fail = (message) => {
    throw new Error(`${file}: ${message}`);
  };
  if (report === null || typeof report !== "object" || Array.isArray(report)) {
    fail("not an SLO report document");
  }
  if (typeof report.catalog_version !== "string" || report.catalog_version.trim() === "") {
    fail("missing `catalog_version`; this is not an SLO report (`slo-report` output)");
  }
  if (typeof report.schema_version !== "string" || report.schema_version.trim() === "") {
    fail("missing `schema_version`; this is not an SLO report (`slo-report` output)");
  }
  const source = report.source;
  if (source === null || typeof source !== "object") {
    fail("missing the `source` identity block; this is not an SLO report");
  }
  const runId = runIdOf(report);
  if (runId === "") {
    fail(
      "carries no run identity, so it is a measurement but not calibration history; " +
        "regenerate it with `slo-report --run-id <stable-id>` (stable across retries of one " +
        "run, distinct across distinct runs)",
    );
  }
  for (const field of ["dataset_id", "config_version"]) {
    if (typeof source[field] !== "string" || source[field].trim() === "") {
      fail(`has no \`source.${field}\`, so it names no compatibility series to pool within`);
    }
  }
  if (typeof source.mode !== "string" || source.mode.trim() === "") {
    fail("has no `source.mode`, so it names no compatibility series to pool within");
  }
  return runId;
}

function git(args, cwd, options = {}) {
  return execFileSync("git", args, { encoding: "utf8", cwd, ...options }).trim();
}

/// Materialize the history branch in an isolated worktree, so the main working
/// tree and its checked-out branch are never modified.
function ensureHistoryWorktree(root, worktree) {
  const remoteHasBranch = git(["ls-remote", "--heads", "origin", BRANCH], root).length > 0;
  if (remoteHasBranch) {
    git(["fetch", "--quiet", "origin", BRANCH], root);
  }
  try {
    rmSync(worktree, { recursive: true, force: true });
  } catch {
    // best effort; `git worktree prune` below handles leftovers
  }
  git(["worktree", "prune"], root);
  if (remoteHasBranch) {
    // -B recreates the local branch at origin's tip and checks it out here, so
    // the commit below advances a real ref instead of a detached HEAD (a
    // detached HEAD cannot be pushed as `slo-data` from a fresh runner).
    git(["worktree", "add", "-B", BRANCH, worktree, `origin/${BRANCH}`], root);
  } else {
    const localExists =
      git(["for-each-ref", `refs/heads/${BRANCH}`, "--format=%(refname)"], root).length > 0;
    if (localExists) {
      git(["worktree", "add", worktree, BRANCH], root);
    } else {
      git(["worktree", "add", "--orphan", "-b", BRANCH, worktree], root);
    }
  }
}

const README = [
  "# slo-data",
  "",
  "Durable operational SLO calibration history (issue #71) — the bridge from",
  "#58's continuous measurement to SLO baseline selection.",
  "",
  "Written only by trusted jobs (push to main) via `scripts/slo-record.mjs`;",
  "`pull_request` jobs never receive credentials for this branch.",
  "",
  "## Layout",
  "",
  "- `reports/<series>/<run>.json` — one `slo-report` document per logical run.",
  "  `<series>` encodes the #58 compatibility boundary",
  "  (`mode + dataset_id + config_version + seed`) through the same helper the",
  "  `benchmark-data` history uses, and `<run>` is derived from the report's",
  "  `source.run_id`. Re-recording a run id replaces that run's artifact, so a",
  "  re-run of one workflow run is idempotent while distinct runs accumulate.",
  "",
  "## Retrieval",
  "",
  "```sh",
  "git fetch origin slo-data",
  "git worktree add target/slo-history origin/slo-data",
  "cargo run --locked -p aivtuber-app --bin slo-baseline -- \\",
  "  target/slo-history/reports/<series>/*.json \\",
  "  --out target/aivtuber-slo/baseline-proposal.json",
  "```",
  "",
  "Pool one `reports/<series>/` directory at a time: a baseline is only valid",
  "within one compatibility series, and `slo-baseline` refuses a history that",
  "spans two.",
  "",
].join("\n");

function main() {
  const { files, root: rootArg } = parseArgs(process.argv.slice(2));
  const root = rootArg ? (isAbsolute(rootArg) ? rootArg : join(scriptRoot, rootArg)) : scriptRoot;
  const worktree = join(root, "target", "slo-data-wt");

  const incoming = files.map((file) => {
    const path = isAbsolute(file) ? file : join(root, file);
    let report;
    try {
      report = JSON.parse(readFileSync(path, "utf8"));
    } catch (error) {
      console.error(`${file}: not an SLO report: ${error.message}`);
      process.exit(1);
    }
    try {
      requireRecordable(report, file);
    } catch (error) {
      console.error(`slo-record: ${error.message}`);
      process.exit(1);
    }
    return { file, report, path: reportPathOf(report), runId: runIdOf(report) };
  });

  ensureHistoryWorktree(root, worktree);

  let added = 0;
  let replaced = 0;
  const written = new Set();
  for (const entry of incoming) {
    const target = join(worktree, entry.path);
    if (existsSync(target) && !written.has(entry.path)) replaced++;
    else if (!existsSync(target)) added++;
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(target, `${JSON.stringify(entry.report, null, 2)}\n`);
    written.add(entry.path);
  }
  writeFileSync(join(worktree, "README.md"), README);

  git(["add", "-A", "--", "reports", "README.md"], worktree);
  const dirty = git(["status", "--porcelain", "--", "reports", "README.md"], worktree);
  if (dirty.length === 0) {
    console.log(`no changes for ${BRANCH} (already recorded)`);
    return;
  }
  // Explicit commit identity: fresh GitHub-hosted runners have no
  // user.name/user.email configured, and the commit would fail with "Author
  // identity unknown".
  git(
    [
      "-c",
      "user.name=slo-bot",
      "-c",
      "user.email=actions@users.noreply.github.com",
      "-c",
      "commit.gpgsign=false",
      "commit",
      "--quiet",
      "-m",
      `slo: record ${added} new + ${replaced} replaced calibration run(s)`,
    ],
    worktree,
  );
  console.log(
    `recorded ${added} new + ${replaced} replaced SLO run(s) on ${BRANCH} ` +
      `(worktree ${relative(root, worktree)})`,
  );
}

const invokedDirectly = process.argv[1]?.replace(/\\/g, "/").endsWith("slo-record.mjs");
if (invokedDirectly) {
  main();
}
