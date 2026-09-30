// Benchmark history recorder — issue #58 Phase E.
//
// Appends trusted main-branch / scheduled benchmark results to the
// `benchmark-data` branch as JSONL (one line per result) keyed by suite, and
// regenerates the trend Markdown. Run only from jobs that own write
// credentials for that branch (push to main, scheduled resource bench);
// untrusted pull_request jobs must never invoke this script.
//
// Usage:
//   bun scripts/benchmark-record.mjs --suite <suite> --files <result.json>...
//
// Implementation notes:
// - history lives in a SEPARATE git worktree (`target/benchmark-data-wt`) so
//   the main working tree is never switched, cleaned, or touched; the caller
//   pushes `benchmark-data` afterwards;
// - commits always advance the real local `benchmark-data` ref (never a
//   detached HEAD), so a plain `git push origin benchmark-data` from a fresh
//   runner finds its source ref;
// - appends each result JSON as one JSONL line to `data/<suite>.jsonl`;
// - dedupes on (benchmark_suite, mode, git.commit): the last line wins, and
//   re-recording the same commit is a no-op instead of duplicating rows;
// - regenerates `trends/<suite>.md` from the full series;
// - commits data + trends in one commit inside the worktree (never pushes).
//
// Security: results carry only commit identity, environment, config, and
// aggregate metrics. Prompts, generated private content, and secrets are not
// part of the result contract, so they cannot leak into history.

import { readFileSync, writeFileSync, mkdirSync, existsSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const BRANCH = "benchmark-data";
const WORKTREE = join(root, "target", "benchmark-data-wt");

function parseArgs(argv) {
  const args = { suite: undefined, files: [] };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--suite") args.suite = argv[++i];
    else if (argv[i] === "--files") {
      for (++i; i < argv.length && !argv[i].startsWith("--"); i++) args.files.push(argv[i]);
    }
  }
  if (!args.suite || args.files.length === 0) {
    console.error("usage: benchmark-record.mjs --suite <suite> --files <result.json>...");
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
      git(["for-each-ref", `refs/heads/${BRANCH}`, "--format=%(refname)" ]).length > 0;
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

function identityOf(result) {
  return [result.benchmark_suite, result.mode, result.git.commit].join("|");
}

function dedupe(rows) {
  const byIdentity = new Map();
  for (const row of rows) byIdentity.set(identityOf(row), row);
  // Sorted for stable JSONL diffs across runs.
  return [...byIdentity.values()].sort((a, b) =>
    identityOf(a).localeCompare(identityOf(b), "en"),
  );
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

function renderTrend(suite, rows) {
  const lines = [
    `# Benchmark trend: ${suite}`,
    "",
    "Generated by `scripts/benchmark-record.mjs` (issue #58 Phase E).",
    "Informational only — PR gating is decided by the machine-readable",
    "`benchmark-compare` comparator, never by visual inspection here.",
    "",
    `Results: ${rows.length} recorded run(s) (deduped on suite+mode+commit).`,
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
    lines.push("| commit | " + present.join(" | ") + " | invariants |");
    lines.push("|---" + "|---".repeat(present.length + 1) + "|");
    for (const row of modeRows) {
      lines.push(
        `| \`${row.git.commit}\` | ${present.map((m) => fmtCell(row, m)).join(" | ")} | ${invariantStatus(row)} |`,
      );
    }
    lines.push("");
  }
  return lines.join("\n");
}

function writeHistoryFiles(suite, rows) {
  writeFileSync(
    join(WORKTREE, "data", `${suite}.jsonl`),
    rows.map((row) => JSON.stringify(row)).join("\n") + (rows.length ? "\n" : ""),
  );
  writeFileSync(join(WORKTREE, "trends", `${suite}.md`), renderTrend(suite, rows));
  writeFileSync(
    join(WORKTREE, "README.md"),
    [
      "# benchmark-data",
      "",
      "Durable benchmark history for the continuous benchmark loop (issue #58).",
      "",
      "- `data/<suite>.jsonl` — one `schemas/benchmark-result.schema.json` document per line;",
      "  deduped on (benchmark_suite, mode, git.commit), last write wins.",
      "- `trends/<suite>.md` — generated trend tables; do not edit by hand.",
      "- Written only by trusted jobs (push to main, scheduled benchmark);",
      "  pull_request jobs never receive credentials for this branch.",
      "- Commit identity and dataset/config metadata are preserved verbatim so",
      "  after-action review (#62) can distinguish trends from one-run anomalies.",
      "",
    ].join("\n"),
  );
}

function main() {
  const { suite, files } = parseArgs(process.argv.slice(2));
  ensureHistoryWorktree();

  const dataPath = join(WORKTREE, "data", `${suite}.jsonl`);
  const existing = loadJsonl(dataPath);
  const incoming = files.map((file) => JSON.parse(readFileSync(join(root, file), "utf8")));

  // Guard: results must declare the suite they are recorded under so a stray
  // file cannot pollute another suite's series.
  for (const result of incoming) {
    if (result.benchmark_suite !== suite) {
      console.error(
        `result ${result.benchmark_suite ?? "?"} does not match --suite ${suite}; refusing to record`,
      );
      process.exit(1);
    }
  }

  const merged = dedupe([...existing, ...incoming]);
  const added = merged.length - existing.length;

  writeHistoryFiles(suite, merged);

  gitInWorktree(["add", `data/${suite}.jsonl`, `trends/${suite}.md`, "README.md"]);
  const dirty = gitInWorktree([
    "status",
    "--porcelain",
    "--",
    `data/${suite}.jsonl`,
    `trends/${suite}.md`,
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
    console.log(`recorded ${added} new result(s) for ${suite} on ${BRANCH} (worktree ${WORKTREE})`);
  } else {
    console.log(`no changes for ${suite} (already recorded)`);
  }
}

const invokedDirectly = process.argv[1]?.replace(/\\/g, "/").endsWith("benchmark-record.mjs");
if (invokedDirectly) {
  main();
}

// Exported for tests (tests/benchmark-record.test.mjs).
export const __testables = { identityOf, dedupe, renderTrend };
