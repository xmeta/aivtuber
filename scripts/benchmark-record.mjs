// Benchmark history recorder — issue #58 Phase E.
//
// Appends trusted main-branch / scheduled benchmark results to the
// `benchmark-data` branch as JSONL (one line per result) grouped into
// compatibility series, and regenerates the trend Markdown. Run only from
// jobs that own write credentials for that branch (push to main, scheduled
// resource bench); untrusted pull_request jobs must never invoke this script.
//
// Usage:
//   bun scripts/benchmark-record.mjs --suite <suite> --run-id <id> --files <result.json>...
//
// Implementation notes:
// - history lives in a SEPARATE git worktree (`target/benchmark-data-wt`) so
//   the main working tree is never switched, cleaned, or touched; the caller
//   pushes `benchmark-data` afterwards;
// - commits always advance the real local `benchmark-data` ref (never a
//   detached HEAD), so a plain `git push origin benchmark-data` from a fresh
//   runner finds its source ref;
// - appends each result JSON as one JSONL line to `data/<suite>.jsonl`;
// - dedupes on (benchmark_suite, mode, git.commit, recording.run_id): a retry
//   of the SAME run replaces its earlier row (re-recording is idempotent),
//   while distinct runs of one revision ACCUMULATE — repeated observations
//   are the raw material for runner-variance baselines
//   (docs/performance-goals.adoc "10+ nightly runs");
// - series are split on the comparator's compatibility boundary
//   (mode + dataset_id + config_version + seed): incompatible workloads get
//   separate data/trend files and are never rendered into one table;
// - regenerates `trends/<suite>.<series>.md` from the full series;
// - commits data + trends in one commit inside the worktree (never pushes).
//
// Security: results carry only commit identity, environment, config, and
// aggregate metrics. Prompts, generated private content, and secrets are not
// part of the result contract, so they cannot leak into history.

import {
  readFileSync,
  writeFileSync,
  mkdirSync,
  existsSync,
  rmSync,
  readdirSync,
} from "node:fs";
import { execFileSync } from "node:child_process";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const BRANCH = "benchmark-data";
const WORKTREE = join(root, "target", "benchmark-data-wt");

function parseArgs(argv) {
  const args = { suite: undefined, runId: undefined, attempt: undefined, files: [] };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--suite") args.suite = argv[++i];
    else if (argv[i] === "--run-id") args.runId = argv[++i];
    else if (argv[i] === "--attempt") args.attempt = argv[++i];
    else if (argv[i] === "--files") {
      for (++i; i < argv.length && !argv[i].startsWith("--"); i++) args.files.push(argv[i]);
    }
  }
  if (!args.suite || args.files.length === 0) {
    console.error(
      "usage: benchmark-record.mjs --suite <suite> [--run-id <id>] [--attempt <n>] --files <result.json>...",
    );
    process.exit(1);
  }
  return args;
}

function git(args, options = {}) {
  return execFileSync("git", args, { encoding: "utf8", cwd: root, ...options }).trim();
}

function gitInWorktree(args, options = {}) {
  return execFileSync("git", args, { encoding: "utf8", cwd: WORKTREE, ...options }).trim();
}

/// Materialize the history branch in an isolated worktree. The main working
/// tree and its checked-out branch are never modified.
function ensureHistoryWorktree() {
  const remoteHasBranch = git(["ls-remote", "--heads", "origin", BRANCH]).length > 0;
  if (remoteHasBranch) {
    git(["fetch", "--quiet", "origin", BRANCH]);
  }
  // The worktree may exist from a previous run in the same job.
  try {
    rmSync(WORKTREE, { recursive: true, force: true });
  } catch {
    // best effort; git worktree prune below handles leftovers
  }
  git(["worktree", "prune"]);
  if (remoteHasBranch) {
    // -B (re)creates the local `benchmark-data` branch at origin's tip and
    // checks it out here, so the commit below advances a real ref instead of
    // a detached HEAD (a detached HEAD cannot be pushed as `benchmark-data`
    // from a fresh runner: "src refspec benchmark-data does not match any").
    // Re-running before a push resets the local branch to origin; the dropped
    // commits only ever re-record results the next run re-appends.
    git(["worktree", "add", "-B", BRANCH, WORKTREE, `origin/${BRANCH}`]);
  } else {
    const localBranchExists =
      git(["for-each-ref", `refs/heads/${BRANCH}`, "--format=%(refname)"]).length > 0;
    if (localBranchExists) {
      // Local branch survived a previous run in this job; reuse it.
      git(["worktree", "add", WORKTREE, BRANCH]);
    } else {
      git(["worktree", "add", "--orphan", "-b", BRANCH, WORKTREE]);
    }
  }
  mkdirSync(join(WORKTREE, "data"), { recursive: true });
  mkdirSync(join(WORKTREE, "trends"), { recursive: true });
}

function loadJsonl(path) {
  if (!existsSync(path)) return [];
  return readFileSync(path, "utf8")
    .split("\n")
    .filter((line) => line.trim().length > 0)
    .map((line) => JSON.parse(line));
}

/// Load every compatibility series of a suite (`data/<suite>.<series>.jsonl`).
/// A legacy flat `data/<suite>.jsonl` from before the series split is folded
/// in once; the caller removes it afterwards so a single write path remains.
function loadHistory(suite) {
  const dataDir = join(WORKTREE, "data");
  const legacyPath = join(dataDir, `${suite}.jsonl`);
  const seriesFiles = existsSync(dataDir)
    ? readdirSync(dataDir).filter(
        (file) => file.startsWith(`${suite}.`) && file.endsWith(".jsonl"),
      )
    : [];
  const rows = [
    ...seriesFiles.flatMap((file) => loadJsonl(join(dataDir, file))),
    ...loadJsonl(legacyPath),
  ];
  return { rows, legacyPath };
}

/// Identity of one recorded observation. The run id makes retries of the same
/// run idempotent (same identity → replaced) while distinct runs of one
/// revision accumulate as separate rows.
function identityOf(result, runId) {
  const run = runId ?? result.recording?.run_id ?? "-";
  return [result.benchmark_suite, result.mode, result.git.commit, run].join("|");
}

function dedupe(rows) {
  const byIdentity = new Map();
  for (const row of rows) byIdentity.set(identityOf(row), row);
  // Sorted for stable JSONL diffs across runs.
  return [...byIdentity.values()].sort((a, b) =>
    identityOf(a).localeCompare(identityOf(b), "en"),
  );
}

/// The compatibility boundary, mirroring benchmark-compare's
/// `ensure_comparable`: results that differ in any of these fields must never
/// be silently compared, so they never share a history series or trend table.
/// `dataset_id` is Optional in the contract; `undefined` and null collapse to
/// one key so legacy rows and contract-optional producers stay comparable.
function compatKey(result) {
  // Optional boundaries collapse to "" (join renders null/undefined as "")
  // so absent and explicit-null stay comparable.
  return [
    result.mode,
    result.dataset_id ?? "",
    result.configuration?.config_version ?? "",
    result.configuration?.seed ?? "",
  ].join("|");
}

/// Short stable hash (32-bit FNV-1a, base36) so long config_version strings
/// cannot overflow filesystem filename limits.
function stableHash(text) {
  let hash = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) {
    hash ^= text.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(36);
}

/// Filesystem-safe, collision-free stem for a series' data/trend files.
function seriesFileStem(key) {
  const slug = key
    .split("|")
    .map((part) => part.replace(/[^a-zA-Z0-9._-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 40) || "-")
    .join("__");
  return `${slug}-${stableHash(key)}`;
}

/// Human-readable series identity for trend headings.
function seriesLabel(key) {
  const [mode, dataset, configVersion, seed] = key.split("|");
  return `${mode} @ ${dataset || "-"} (${configVersion || "-"}, seed ${seed ?? "-"})`;
}

// Metrics surfaced in the trend table, per suite family. Keys are exact
// metric names from the result contract; units are embedded in the names.
const TREND_METRICS = [
  "cached.first_audio.p95_ms",
  "cached.first_visible.p95_ms",
  "routing.route_decision.p95_us",
  "routing.llm_calls_per_100_events",
  "semantic.wrong_reuse_rate_pct",
  "resource.throughput_events_per_s",
  "resource.scheduler_history_retained_count",
  "resource.telemetry_events_retained_count",
  "resource.hot_assets_resident_count",
  "resource.peak_rss_kib",
];

function fmtCell(row, metric) {
  const value = row.metrics?.[metric];
  if (!value) return "-";
  const text = Number.isInteger(value.value) ? String(value.value) : value.value.toFixed(2);
  return value.sample_count != null ? `${text} (n=${value.sample_count})` : text;
}

function invariantStatus(row) {
  const entries = Object.entries(row.invariants ?? {});
  if (entries.length === 0) return "-";
  const failed = entries.filter(([, v]) => (v.value ?? 0) > 0);
  return failed.length === 0 ? "pass" : `FAIL(${failed.map(([n]) => n).join(",")})`;
}

function renderTrend(series, rows) {
  const lines = [
    `# Benchmark trend: ${series}`,
    "",
    "Generated by `scripts/benchmark-record.mjs` (issue #58 Phase E).",
    "Informational only — PR gating is decided by the machine-readable",
    "`benchmark-compare` comparator, never by visual inspection here.",
    "",
    `Results: ${rows.length} recorded run(s) (deduped on suite+mode+commit+run).`,
    "",
  ];

  const byMode = new Map();
  for (const row of rows) {
    const mode = row.mode ?? "unknown";
    if (!byMode.has(mode)) byMode.set(mode, []);
    byMode.get(mode).push(row);
  }

  for (const [mode, modeRows] of [...byMode.entries()].sort()) {
    const present = TREND_METRICS.filter((metric) =>
      modeRows.some((row) => row.metrics?.[metric] != null),
    );
    if (present.length === 0) continue;
    lines.push(`## mode: ${mode}`, "");
    lines.push("| commit | run | " + present.join(" | ") + " | invariants |");
    lines.push("|---" + "|---".repeat(present.length + 2) + "|");
    for (const row of modeRows) {
      lines.push(
        `| \`${row.git.commit}\` | \`${row.recording?.run_id ?? "-"}\` | ${present.map((m) => fmtCell(row, m)).join(" | ")} | ${invariantStatus(row)} |`,
      );
    }
    lines.push("");
  }
  return lines.join("\n");
}

/// Write every compatibility series' data + trend files. Returns the relative
/// paths it produced so the caller can stage exactly those.
function writeHistoryFiles(suite, bySeries) {
  const dataFiles = [];
  const trendFiles = [];
  for (const [key, rows] of bySeries) {
    const stem = seriesFileStem(key);
    const dataPath = `data/${suite}.${stem}.jsonl`;
    const trendPath = `trends/${suite}.${stem}.md`;
    writeFileSync(
      join(WORKTREE, dataPath),
      rows.map((row) => JSON.stringify(row)).join("\n") + (rows.length ? "\n" : ""),
    );
    writeFileSync(join(WORKTREE, trendPath), renderTrend(seriesLabel(key), rows));
    dataFiles.push(dataPath);
    trendFiles.push(trendPath);
  }
  writeFileSync(
    join(WORKTREE, "README.md"),
    [
      "# benchmark-data",
      "",
      "Durable benchmark history for the continuous benchmark loop (issue #58).",
      "",
      `- \`data/<suite>.<series>.jsonl\` — one \`schemas/benchmark-result.schema.json\` document per line;`,
      "  deduped on (benchmark_suite, mode, git.commit, recording.run_id), last write wins.",
      "  Retries of one workflow run are idempotent; distinct runs of the same revision",
      "  accumulate so runner variance can be calibrated (docs/performance-goals.adoc).",
      "- `<series>` encodes the comparator's compatibility boundary (mode + dataset_id +",
      "  config_version + seed) plus a short stable hash: results from different datasets,",
      "  configs, or seeds are never rendered into the same trend table.",
      "- `trends/<suite>.<series>.md` — generated trend tables; do not edit by hand.",
      "- `recording.run_id` / `recording.recorded_at` (+ diagnostic `attempt`) are appended by",
      "  the recorder when a trusted job stores the result; benchmark producers never emit them.",
      "  run_id is stable across re-runs of one workflow run (GitHub run_id), so a re-run",
      "  replaces its earlier row; attempt never participates in identity.",
      "- Written only by trusted jobs (push to main, scheduled benchmark);",
      "  pull_request jobs never receive credentials for this branch.",
      "- Commit identity and dataset/config metadata are preserved verbatim so",
      "  after-action review (#62) can distinguish trends from one-run anomalies.",
      "",
    ].join("\n"),
  );
  return { dataFiles, trendFiles };
}

/// Run identity: explicit --run-id wins, then GitHub Actions' run id, then a
/// caller-provided BENCHMARK_RUN_ID. GITHUB_RUN_ID is invariant across
/// re-runs of one workflow run (only run_attempt increments), so keying on it
/// keeps retries idempotent while distinct runs accumulate. Without an
/// identity, retries and repeated observations of a revision would be
/// indistinguishable.
function resolveRunId(explicit) {
  if (explicit && explicit.trim().length > 0) return explicit.trim();
  const env = process.env;
  if (env.GITHUB_RUN_ID && env.GITHUB_RUN_ID.trim().length > 0) {
    return `run-${env.GITHUB_RUN_ID.trim()}`;
  }
  if (env.BENCHMARK_RUN_ID && env.BENCHMARK_RUN_ID.trim().length > 0) {
    return env.BENCHMARK_RUN_ID.trim();
  }
  console.error(
    "no benchmark run identity: pass --run-id <id> (or set GITHUB_RUN_ID / BENCHMARK_RUN_ID); " +
      "without it retries and repeated observations of one revision cannot be told apart",
  );
  process.exit(1);
}

/// Attach history provenance. run_id keys dedupe (stable across re-runs);
/// attempt is DIAGNOSTIC ONLY — it never participates in identity, so a
/// re-run replaces its earlier row instead of accumulating a phantom
/// observation.
function stampRecording(result, { runId, recordedAt, attempt }) {
  const recording = { run_id: runId, recorded_at: recordedAt };
  const attemptNum = Number.parseInt(attempt, 10);
  if (Number.isInteger(attemptNum) && attemptNum >= 1) {
    recording.attempt = attemptNum;
  }
  return { ...result, recording };
}

function main() {
  const { suite, files, runId: explicitRunId, attempt } = parseArgs(process.argv.slice(2));
  const runId = resolveRunId(explicitRunId);
  ensureHistoryWorktree();

  const { rows: existing, legacyPath } = loadHistory(suite);
  const incomingRaw = files.map((file) => JSON.parse(readFileSync(join(root, file), "utf8")));

  // Guard: results must declare the suite they are recorded under so a stray
  // file cannot pollute another suite's series.
  for (const result of incomingRaw) {
    if (result.benchmark_suite !== suite) {
      console.error(
        `result ${result.benchmark_suite ?? "?"} does not match --suite ${suite}; refusing to record`,
      );
      process.exit(1);
    }
  }

  // History provenance: the run id distinguishes retries (same run → row
  // replaced) from repeated observations (distinct runs → accumulated), and
  // recorded_at/attempt timestamp the append itself. Producers never emit
  // these.
  const recordedAt = `${new Date().toISOString().slice(0, 19)}Z`;
  const incoming = incomingRaw.map((result) =>
    stampRecording(result, { runId, recordedAt, attempt }),
  );

  // One logical run may legitimately span several compatibility boundaries
  // (record-main records all four replay modes of one run in a single
  // invocation); every boundary lands in its own series below.
  const merged = dedupe([...existing, ...incoming]);
  const added = merged.length - existing.length;

  // One series per compatibility boundary; rows sorted for stable JSONL diffs.
  const bySeries = new Map();
  for (const row of merged) {
    const key = compatKey(row);
    if (!bySeries.has(key)) bySeries.set(key, []);
    bySeries.get(key).push(row);
  }
  for (const rows of bySeries.values()) {
    rows.sort((a, b) => identityOf(a).localeCompare(identityOf(b), "en"));
  }

  const { dataFiles, trendFiles } = writeHistoryFiles(suite, bySeries);

  // Fold the legacy flat file into the split layout once, then delete it so
  // exactly one write path remains. `-A` with directory pathspecs also stages
  // the deletion (the file itself must not be a pathspec: it does not exist
  // in the common case).
  rmSync(legacyPath, { force: true });

  gitInWorktree(["add", "-A", "--", "data", "trends", "README.md"]);
  const dirty = gitInWorktree([
    "status",
    "--porcelain",
    "--",
    ...dataFiles,
    ...trendFiles,
    "README.md",
  ]);
  if (dirty.length > 0) {
    // Explicit commit identity: fresh GitHub-hosted runners have no
    // user.name/user.email configured, and the commit would fail with
    // "Author identity unknown". Setting it here (instead of via workflow
    // `git config`) protects every current and future caller.
    gitInWorktree([
      "-c",
      "user.name=benchmark-bot",
      "-c",
      "user.email=actions@users.noreply.github.com",
      "-c",
      "commit.gpgsign=false",
      "commit",
      "--quiet",
      "-m",
      `benchmark: record ${suite} results (${added} new)`,
    ]);
    console.log(
      `recorded ${added} new result(s) for ${suite} on ${BRANCH} (run ${runId}, worktree ${WORKTREE})`,
    );
  } else {
    console.log(`no changes for ${suite} (run ${runId} already recorded)`);
  }
}

const invokedDirectly = process.argv[1]?.replace(/\\/g, "/").endsWith("benchmark-record.mjs");
if (invokedDirectly) {
  main();
}

// The #58 compatibility boundary (`compatKey`, `seriesFileStem`) is shared
// with scripts/slo-record.mjs: the SLO calibration history is keyed on the same
// series, so both histories must derive one identical series identity rather
// than two that can drift apart.
export { compatKey, seriesFileStem, seriesLabel, stableHash };

// Exported for tests (tests/benchmark-record.test.mjs).
export const __testables = {
  identityOf,
  dedupe,
  compatKey,
  seriesFileStem,
  seriesLabel,
  renderTrend,
  stampRecording,
};
