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
//                              [--benchmarks <replay-report-dir>]
//
// `--benchmarks` points at the directory of raw replay reports
// (`replay-benchmark`'s `<mode>.json`). Each SLO report is paired with the
// replay report of the same `<stem>` and the pair is stored under the same
// contract/series/run identity, because latency calibration cannot be finished
// from an `SloReport` alone: the probe step (`slo-report --probe-latency`)
// re-reads the per-event observations that a stored SLO report does not carry.
// Without the raw report a stored run could only ever supply the
// threshold-selection percentiles, never the measured conforming ratio at the
// chosen boundary.
//
// Layout on the branch — one document per logical run, partitioned by the SLO
// contract it was written for and then by the #58 compatibility series:
//   reports/<schema-version>/<catalog-version>/<series>/<run>.json
//
// Two compatibility boundaries, both enforced rather than assumed:
//   - `<schema-version>` / `<catalog-version>` are the SLO report contract.
//     `slo-baseline` fails closed on a history that spans two, because
//     indicator definitions from two contracts are not poolable. Partitioning
//     here is what lets an intentional catalog bump accumulate new calibration
//     history without making the existing one unretrievable, and keeps the
//     documented per-directory wildcard pooling to a single contract.
//   - `<series>` encodes the same #58 compatibility boundary the benchmark
//     history uses (mode + dataset_id + config_version + seed), through the
//     *same* helper, so the two histories cannot disagree about what a series
//     is.
//
// `<run>` is derived from the report's `source.run_id`.
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
import { join, dirname, isAbsolute, relative, basename } from "node:path";
import { fileURLToPath } from "node:url";

import { compatKey, seriesFileStem, stableHash } from "./benchmark-record.mjs";

const scriptRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const BRANCH = "slo-data";

function parseArgs(argv) {
  const args = { files: [], root: undefined, benchmarks: undefined };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--root") {
      args.root = argv[++i];
      continue;
    }
    if (argv[i] === "--benchmarks") {
      args.benchmarks = argv[++i];
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
    console.error(
      "usage: slo-record.mjs --files <slo-report.json>... [--root <dir>] " +
        "[--benchmarks <replay-report-dir>]",
    );
    process.exit(1);
  }
  if (args.benchmarks !== undefined && args.benchmarks.trim() === "") {
    console.error("--benchmarks requires a directory path");
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

/// Filesystem-safe, collision-free segment for a contract version.
///
/// A well-formed short version (`1`, `slo-catalog-v1`) is used verbatim, so the
/// partition stays readable to whoever retrieves the history by hand. Anything
/// else is slugified and given the same stable hash suffix the series stem
/// uses, so two distinct versions can never collapse onto one directory.
///
/// The verbatim path requires a leading alphanumeric or `_`, which keeps a
/// document-declared version from ever becoming a `.` or `..` path segment and
/// walking the write out of the history worktree.
export function versionFileStem(version) {
  const raw = String(version).trim();
  if (/^[a-zA-Z0-9_][a-zA-Z0-9._-]{0,63}$/.test(raw)) {
    return raw;
  }
  const slug =
    raw.replace(/[^a-zA-Z0-9._-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 40) || "version";
  return `${slug}-${stableHash(raw)}`;
}

/// The SLO contract a report was written for: the two fields
/// `BaselineProposalSet::from_reports` refuses to pool across. Partitioning the
/// history on them is what makes the documented per-directory retrieval stay
/// valid across an intentional catalog or schema bump.
export function contractSegmentsOf(report) {
  return {
    schema: versionFileStem(report?.schema_version),
    catalog: versionFileStem(report?.catalog_version),
  };
}

/// The single path a report occupies on the history branch. One path per
/// (contract, series, run identity) is the whole point: it makes a re-record
/// idempotent, leaves `slo-baseline` nothing to disambiguate, and keeps every
/// directory poolable to exactly one contract.
export function reportPathOf(report) {
  const contract = contractSegmentsOf(report);
  return `reports/${contract.schema}/${contract.catalog}/${seriesFileStem(seriesKeyOf(report))}/${runFileStem(runIdOf(report))}.json`;
}

/// Where the raw replay report that produced an SLO report is kept.
///
/// It lives in a *parallel* tree, not beside the SLO report: the documented
/// retrieval pools `reports/<…>/*.json` with a wildcard, and a `BenchmarkReport`
/// in that directory would be refused as not an SLO report. Same contract,
/// series, and run identity, so a probe can find the source of a stored run by
/// changing one path segment.
export function benchmarkPathOf(report) {
  const contract = contractSegmentsOf(report);
  return `benchmark-reports/${contract.schema}/${contract.catalog}/${seriesFileStem(seriesKeyOf(report))}/${runFileStem(runIdOf(report))}.json`;
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

/// Refuse a replay report that is not the source of the SLO report it is being
/// paired with.
///
/// The pair is what makes latency calibration durable: the probe step
/// (`slo-report --probe-latency`) re-reads the per-event observations, which a
/// stored `SloReport` does not carry, so the raw report must be retained. If the
/// pair came from a different workload the probe would measure a ratio that has
/// nothing to do with the history it would be pooled into, so the #58 series
/// fields are compared and the pair is refused on any difference. `git_commit`
/// is deliberately not compared: the revision is what a baseline is attributed
/// to, not part of the compatibility boundary, and one series may span several.
export function requirePairable(benchmark, sloReport, file) {
  const fail = (message) => {
    throw new Error(`${file}: ${message}`);
  };
  if (benchmark === null || typeof benchmark !== "object" || Array.isArray(benchmark)) {
    fail("not a replay benchmark report document");
  }
  const metadata = benchmark.metadata;
  if (metadata === null || typeof metadata !== "object") {
    fail("not a replay benchmark report: no `metadata` block");
  }
  if (!Array.isArray(benchmark.events)) {
    fail("not a replay benchmark report: no `events` observations");
  }
  const source = sloReport.source;
  for (const [reportField, metadataField] of [
    ["dataset_id", "dataset_id"],
    ["config_version", "config_version"],
    ["mode", "mode"],
  ]) {
    const observed = reportField === "mode" ? benchmark.mode : metadata[metadataField];
    if (observed !== source[reportField]) {
      fail(
        `does not belong to the same compatibility series as its SLO report ` +
          `(${reportField} ${JSON.stringify(observed)} vs ${JSON.stringify(source[reportField])})`,
      );
    }
  }
  if (metadata.seed !== source.seed) {
    fail(
      `does not belong to the same compatibility series as its SLO report ` +
        `(seed ${JSON.stringify(metadata.seed)} vs ${JSON.stringify(source.seed)})`,
    );
  }
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
  "Two parallel trees, sharing one contract/series/run identity:",
  "",
  "- `reports/<schema>/<catalog>/<series>/<run>.json` — the `slo-report`",
  "  document. This is the tree `slo-baseline` pools.",
  "- `benchmark-reports/<schema>/<catalog>/<series>/<run>.json` — the raw",
  "  `replay-benchmark` mode report that the SLO report was evaluated from.",
  "",
  "Both are partitioned on two compatibility boundaries: the SLO report",
  "contract (`<schema>`/`<catalog>` are `schema_version`/`catalog_version`, and",
  "`slo-baseline` fails closed on a history spanning two) and the #58 workload",
  "series (same helper the `benchmark-data` history uses). One leaf directory is",
  "exactly one contract in exactly one series.",
  "",
  "The trees are kept separate on purpose: the retrieval below pools",
  "`reports/<...>/*.json` with a wildcard, and a replay report in that directory",
  "would be refused as not an SLO report.",
  "",
  "`<run>` is derived from the report's `source.run_id`. Re-recording a run id",
  "replaces both of its artifacts, so a re-run of one workflow run is idempotent",
  "while distinct runs accumulate.",
  "",
  "## Ratio objectives",
  "",
  "git fetch origin slo-data",
  "git worktree add target/slo-history origin/slo-data",
  "",
  "cargo run --locked -p aivtuber-app --bin slo-baseline -- \\",
  "  target/slo-history/reports/<schema>/<catalog>/<series>/*.json \\",
  "  --out target/aivtuber-slo/baseline-proposal.json",
  "",
  "## Latency objectives",
  "",
  "A latency target needs one more *measured* step: the conforming ratio at a",
  "chosen `threshold_ms`. That ratio cannot be reconstructed from the stored",
  "percentiles, so each stored run is re-probed from its stored replay report —",
  "which is what `benchmark-reports/` keeps. Probe every run in the leaf at the",
  "boundary chosen from `latency_calibration`, then pool the probed reports:",
  "",
  "leaf=reports/<schema>/<catalog>/<series>",
  "raw=benchmark-reports/<schema>/<catalog>/<series>",
  "mkdir -p target/aivtuber-slo/probed",
  "",
  "# One probed report per stored run, each with its own --run-id. The run ids",
  "# are the `source.run_id` values in the sibling reports/ leaf.",
  "for run in target/slo-history/$raw/*.json; do",
  "  cargo run --locked -p aivtuber-app --bin slo-report -- \\",
  "    --report $run \\",
  "    --run-id <that-run's-source.run_id> \\",
  "    --probe-latency availability.event_to_first_audio_within_target=250 \\",
  "    --out target/aivtuber-slo/probed/$(basename $run)",
  "done",
  "",
  "cargo run --locked -p aivtuber-app --bin slo-baseline -- \\",
  "  target/aivtuber-slo/probed/*.json \\",
  "  --out target/aivtuber-slo/latency-baseline.json",
  "",
].join("\n");

function main() {
  const { files, root: rootArg, benchmarks: benchmarksArg } = parseArgs(process.argv.slice(2));
  const root = rootArg ? (isAbsolute(rootArg) ? rootArg : join(scriptRoot, rootArg)) : scriptRoot;
  const benchmarkDir =
    benchmarksArg === undefined
      ? undefined
      : isAbsolute(benchmarksArg)
        ? benchmarksArg
        : join(root, benchmarksArg);
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

  // Pair each SLO report with the replay report it was evaluated from. A
  // missing pair is refused rather than skipped: a stored run whose source is
  // missing cannot be probed later, which is exactly the gap this closes.
  const sources = new Map();
  if (benchmarkDir !== undefined) {
    for (const entry of incoming) {
      const stem = basename(entry.file, ".json");
      const candidate = join(benchmarkDir, `${stem}.json`);
      if (!existsSync(candidate)) {
        console.error(
          `slo-record: ${entry.file}: no replay report at ${candidate}; the latency probe ` +
            `needs the source report, so a run without it cannot be probed later`,
        );
        process.exit(1);
      }
      let benchmark;
      try {
        benchmark = JSON.parse(readFileSync(candidate, "utf8"));
      } catch (error) {
        console.error(`slo-record: ${candidate}: not a replay benchmark report: ${error.message}`);
        process.exit(1);
      }
      try {
        requirePairable(benchmark, entry.report, candidate);
      } catch (error) {
        console.error(`slo-record: ${error.message}`);
        process.exit(1);
      }
      sources.set(entry.path, {
        document: benchmark,
        path: benchmarkPathOf(entry.report),
      });
    }
  }

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

    const source = sources.get(entry.path);
    if (source !== undefined) {
      const sourceTarget = join(worktree, source.path);
      mkdirSync(dirname(sourceTarget), { recursive: true });
      writeFileSync(sourceTarget, `${JSON.stringify(source.document, null, 2)}\n`);
    }
  }
  writeFileSync(join(worktree, "README.md"), README);

  // `git add` rejects a pathspec that matches nothing (exit 128), so the
  // benchmark tree is only named when this invocation actually wrote one.
  const paths = existsSync(join(worktree, "benchmark-reports"))
    ? ["reports", "benchmark-reports", "README.md"]
    : ["reports", "README.md"];
  git(["add", "-A", "--", ...paths], worktree);
  const dirty = git(["status", "--porcelain", "--", ...paths], worktree);
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
