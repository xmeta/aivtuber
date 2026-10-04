// Validates repository examples/fixtures against their JSON Schemas.
// - examples/events/*.json must conform to event-envelope.schema.json
// - examples/events/invalid/*.json must REJECT against event-envelope.schema.json
// - examples/reflex-decisions/*.json must conform to reflex-decision.schema.json
// - examples/reflex-decisions/invalid/*.json must REJECT against reflex-decision.schema.json
// - examples/security/*.json must conform to security-regression-case.schema.json,
//   and their embedded input.event must conform to event-envelope.schema.json
// - Performance Asset valid/starter fixtures must conform to performance-asset.schema.json
// - examples/performance-assets/invalid/*.json must REJECT against that schema
// - examples/evaluation/reaction-quality/*.json must conform to the reaction-quality dataset schema
//   and development/holdout partitions must not share case IDs or leakage groups.
// - examples/evaluation/reaction-quality-review/*.json must conform to the pairwise-review schema
//   and match the referenced dataset partition, case categories, quality dimensions, and seeded A/B mapping.
//
// Usage: bun scripts/validate.mjs  (or: node scripts/validate.mjs)

import { Ajv2020 } from "ajv/dist/2020.js";
import addFormats from "ajv-formats";
import { createHash } from "node:crypto";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

function loadJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function listJson(dir) {
  const abs = join(root, dir);
  if (!existsSync(abs)) return [];
  return readdirSync(abs)
    .filter((f) => f.endsWith(".json"))
    .sort()
    .map((f) => `${dir}/${f}`);
}

const ajv = new Ajv2020({
  allErrors: true,
  strict: true,
  // `not: { required: [...] }` guards reference properties defined elsewhere
  // in the schema; union types like ["object", "string", "null"] are intentional.
  strictRequired: false,
  allowUnionTypes: true,
});
addFormats(ajv);

const schemaFiles = listJson("schemas");
if (schemaFiles.length === 0) {
  console.error("No schemas found under schemas/");
  process.exit(1);
}

const validators = {};
let failures = 0;
let passes = 0;

for (const file of schemaFiles) {
  const schema = loadJson(join(root, file));
  try {
    validators[file] = ajv.compile(schema);
    console.log(`compiled  ${file}`);
  } catch (err) {
    failures += 1;
    console.error(`SCHEMA INVALID ${file}: ${err.message}`);
  }
}

const envelope = validators["schemas/event-envelope.schema.json"];
const reflex = validators["schemas/reflex-decision.schema.json"];
const regression = validators["schemas/security-regression-case.schema.json"];
const asset = validators["schemas/performance-asset.schema.json"];
const reactionQuality = validators["schemas/reaction-quality-dataset.schema.json"];
const reactionQualityReview = validators["schemas/reaction-quality-review.schema.json"];

function report(ok, label, validator) {
  if (ok) {
    passes += 1;
    console.log(`ok        ${label}`);
  } else {
    failures += 1;
    console.error(`FAIL      ${label}`);
    for (const e of validator.errors ?? []) {
      console.error(`          ${e.instancePath || "/"} ${e.message}`);
    }
  }
}

for (const file of listJson("examples/events")) {
  if (!envelope) break;
  report(envelope(loadJson(join(root, file))), file, envelope);
}

for (const file of listJson("examples/events/invalid")) {
  if (!envelope) break;
  const valid = envelope(loadJson(join(root, file)));
  if (valid) {
    failures += 1;
    console.error(`FAIL      ${file} (expected rejection, but it validated)`);
  } else {
    passes += 1;
    console.log(`ok        ${file} (rejected as intended)`);
  }
}

for (const file of listJson("examples/reflex-decisions")) {
  if (!reflex) break;
  report(reflex(loadJson(join(root, file))), file, reflex);
}

for (const file of listJson("examples/reflex-decisions/invalid")) {
  if (!reflex) break;
  const valid = reflex(loadJson(join(root, file)));
  if (valid) {
    failures += 1;
    console.error(`FAIL      ${file} (expected rejection, but it validated)`);
  } else {
    passes += 1;
    console.log(`ok        ${file} (rejected as intended)`);
  }
}

for (const file of listJson("examples/security")) {
  const doc = loadJson(join(root, file));
  if (regression) report(regression(doc), file, regression);
  if (envelope && doc?.input?.event) {
    report(envelope(doc.input.event), `${file} -> input.event`, envelope);
  }
}

for (const dir of [
  "examples/assets",
  "examples/starter-reaction-pack/descriptors",
  "examples/performance-assets/valid",
]) {
  for (const file of listJson(dir)) {
    if (!asset) break;
    report(asset(loadJson(join(root, file))), file, asset);
  }
}

for (const file of listJson("examples/performance-assets/invalid")) {
  if (!asset) break;
  const valid = asset(loadJson(join(root, file)));
  if (valid) {
    failures += 1;
    console.error(`FAIL      ${file} (expected rejection, but it validated)`);
  } else {
    passes += 1;
    console.log(`ok        ${file} (rejected as intended)`);
  }
}

const reactionDocuments = [];
for (const file of listJson("examples/evaluation/reaction-quality")) {
  if (!reactionQuality) break;
  const doc = loadJson(join(root, file));
  const valid = reactionQuality(doc);
  report(valid, file, reactionQuality);
  if (valid) reactionDocuments.push({ file, doc });
}

const reactionGroups = new Map();
for (const { file, doc } of reactionDocuments) {
  const identity = `${doc.dataset_id}@${doc.dataset_version}`;
  let group = reactionGroups.get(identity);
  if (!group) {
    group = {
      compatibility: JSON.stringify(doc.compatibility),
      partitions: new Set(),
      caseIds: new Map(),
      leakageGroups: new Map(),
      coverage: new Map(),
    };
    reactionGroups.set(identity, group);
  }

  if (group.compatibility !== JSON.stringify(doc.compatibility)) {
    failures += 1;
    console.error(`FAIL      ${identity}: incompatible compatibility metadata in ${file}`);
  }

  group.partitions.add(doc.partition);
  if (!group.coverage.has(doc.partition)) group.coverage.set(doc.partition, new Map());

  for (const testCase of doc.cases) {
    const priorCase = group.caseIds.get(testCase.case_id);
    if (priorCase) {
      failures += 1;
      console.error(
        `FAIL      ${identity}: duplicate case_id ${testCase.case_id} in ${priorCase} and ${file}`,
      );
    } else {
      group.caseIds.set(testCase.case_id, file);
    }

    const priorPartition = group.leakageGroups.get(testCase.leakage_group);
    if (priorPartition && priorPartition !== doc.partition) {
      failures += 1;
      console.error(
        `FAIL      ${identity}: leakage_group ${testCase.leakage_group} crosses ${priorPartition} and ${doc.partition}`,
      );
    } else {
      group.leakageGroups.set(testCase.leakage_group, doc.partition);
    }

    const coverage = group.coverage.get(doc.partition);
    coverage.set(testCase.category, (coverage.get(testCase.category) ?? 0) + 1);
  }
}

for (const [identity, group] of reactionGroups) {
  for (const requiredPartition of ["development", "holdout"]) {
    if (!group.partitions.has(requiredPartition)) {
      failures += 1;
      console.error(`FAIL      ${identity}: missing ${requiredPartition} partition`);
    }
  }

  if (group.partitions.has("development") && group.partitions.has("holdout")) {
    passes += 1;
    console.log(`ok        ${identity}: development/holdout partition boundary`);
  }

  for (const [partition, categories] of [...group.coverage.entries()].sort()) {
    const summary = [...categories.entries()]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([category, count]) => `${category}=${count}`)
      .join(", ");
    console.log(`coverage  ${identity} ${partition}: ${summary}`);
  }
}

function reviewPresentation(caseId, seed) {
  const firstByte = createHash("sha256").update(`${seed}:${caseId}`).digest()[0];
  return firstByte % 2 === 0
    ? { a_role: "base", b_role: "candidate" }
    : { a_role: "candidate", b_role: "base" };
}

function sameReviewPresentation(actual, expected) {
  return actual?.a_role === expected.a_role && actual?.b_role === expected.b_role;
}

const reactionReviewDocuments = [];
for (const file of listJson("examples/evaluation/reaction-quality-review")) {
  if (!reactionQualityReview) break;
  const doc = loadJson(join(root, file));
  const valid = reactionQualityReview(doc);
  report(valid, file, reactionQualityReview);
  if (valid) reactionReviewDocuments.push({ file, doc });
}

for (const { file, doc } of reactionReviewDocuments) {
  const source = reactionDocuments.find(
    ({ doc: candidate }) =>
      candidate.dataset_id === doc.dataset.dataset_id &&
      candidate.dataset_version === doc.dataset.dataset_version &&
      candidate.partition === doc.dataset.partition &&
      candidate.partition_revision === doc.dataset.partition_revision,
  );

  if (!source) {
    failures += 1;
    console.error(
      `FAIL      ${file}: no matching reaction-quality dataset partition for review identity`,
    );
    continue;
  }

  let reviewValid = true;
  if (JSON.stringify(source.doc.compatibility) !== JSON.stringify(doc.dataset.compatibility)) {
    failures += 1;
    reviewValid = false;
    console.error(`FAIL      ${file}: review compatibility does not match source dataset`);
  }

  const sourceCases = new Map(source.doc.cases.map((testCase) => [testCase.case_id, testCase]));
  const seenReviewCases = new Set();
  for (const reviewCase of doc.cases) {
    const sourceCase = sourceCases.get(reviewCase.case_id);
    if (!sourceCase) {
      failures += 1;
      reviewValid = false;
      console.error(`FAIL      ${file}: unknown review case_id ${reviewCase.case_id}`);
      continue;
    }

    if (seenReviewCases.has(reviewCase.case_id)) {
      failures += 1;
      reviewValid = false;
      console.error(`FAIL      ${file}: duplicate review case_id ${reviewCase.case_id}`);
    }
    seenReviewCases.add(reviewCase.case_id);

    if (sourceCase.category !== reviewCase.category) {
      failures += 1;
      reviewValid = false;
      console.error(
        `FAIL      ${file}: category mismatch for ${reviewCase.case_id}: ${reviewCase.category} vs ${sourceCase.category}`,
      );
    }

    const expected = reviewPresentation(reviewCase.case_id, doc.protocol.randomization_seed);
    if (!sameReviewPresentation(reviewCase.presentation, expected)) {
      failures += 1;
      reviewValid = false;
      console.error(
        `FAIL      ${file}: seeded A/B presentation mismatch for ${reviewCase.case_id}`,
      );
    }

    if (doc.status === "complete" && reviewCase.reviews.length === 0) {
      failures += 1;
      reviewValid = false;
      console.error(`FAIL      ${file}: complete review has no labels for ${reviewCase.case_id}`);
    }

    const reviewers = new Set();
    for (const review of reviewCase.reviews) {
      if (reviewers.has(review.reviewer_id)) {
        failures += 1;
        reviewValid = false;
        console.error(
          `FAIL      ${file}: duplicate reviewer_id ${review.reviewer_id} for ${reviewCase.case_id}`,
        );
      }
      reviewers.add(review.reviewer_id);

      const dimensions = new Set();
      for (const label of review.dimension_labels) {
        if (dimensions.has(label.dimension)) {
          failures += 1;
          reviewValid = false;
          console.error(
            `FAIL      ${file}: duplicate dimension ${label.dimension} for ${reviewCase.case_id}/${review.reviewer_id}`,
          );
        }
        dimensions.add(label.dimension);
        if (!sourceCase.quality_dimensions.includes(label.dimension)) {
          failures += 1;
          reviewValid = false;
          console.error(
            `FAIL      ${file}: dimension ${label.dimension} is not declared by source case ${reviewCase.case_id}`,
          );
        }
      }
    }
  }

  if (reviewValid) {
    passes += 1;
    console.log(
      `ok        ${file}: review identity, cases, dimensions, and seeded blinding match source dataset`,
    );
  }
}

// Verify AsciiDoc `link:` targets exist (catches broken doc links).
function collectAdocFiles(dir, out = []) {
  const abs = join(root, dir);
  if (!existsSync(abs)) return out;
  for (const entry of readdirSync(abs, { withFileTypes: true })) {
    const rel = `${dir}/${entry.name}`;
    if (entry.isDirectory()) collectAdocFiles(rel, out);
    else if (entry.name.endsWith(".adoc")) out.push(rel);
  }
  return out;
}

for (const file of ["README.adoc", ...collectAdocFiles("docs")]) {
  const text = readFileSync(join(root, file), "utf8");
  const base = join(root, file, "..");
  for (const match of text.matchAll(/link:([^\[\s]+)\[/g)) {
    const target = match[1];
    if (/^[a-z][a-z0-9+.-]*:/i.test(target)) continue; // external (http:, https:, mailto:)
    if (!existsSync(join(base, target))) {
      failures += 1;
      console.error(`FAIL      ${file}: broken link -> ${target}`);
    } else {
      passes += 1;
      console.log(`ok        ${file}: link ${target}`);
    }
  }
}

// Reject leaked local automation/device execution signatures in any tracked file.
const trackedFiles = execFileSync("git", ["ls-files", "-z"], {
  cwd: root,
  encoding: "utf8",
})
  .split("\0")
  .filter(Boolean);

const leakedSignature = `[${"executed"} on ${"device"}:`;
for (const file of trackedFiles) {
  const text = readFileSync(join(root, file)).toString("utf8").toLowerCase();
  if (text.includes(leakedSignature)) {
    failures += 1;
    console.error(`FAIL      ${file}: leaked automation/device execution signature`);
  }
}

// ---------------------------------------------------------------------------
// Issue #147: the property/fuzz regression workflow must stay bounded and out
// of the pull-request gate.
//
// `scripts/validate-toolchain-pins.mjs` deliberately enforces a single Rust
// channel across ci.yml, so the nightly fuzz campaign lives in its own
// workflow file. That puts it outside the toolchain validator's reach, so
// these assertions stand in for it. They are cheap text checks because the
// failure they prevent is a silently unbounded, or PR-gating, fuzz job.
// ---------------------------------------------------------------------------
{
  const fuzzWorkflow = join(root, ".github/workflows/fuzz.yml");
  const ciWorkflow = readFileSync(join(root, ".github/workflows/ci.yml"), "utf8");

  if (!existsSync(fuzzWorkflow)) {
    failures += 1;
    console.error("FAIL      .github/workflows/fuzz.yml: missing (issue #147)");
  } else {
    const fuzz = readFileSync(fuzzWorkflow, "utf8");

    // A PR-gating fuzz campaign would put nightly plus a cargo-fuzz install in
    // every pull request, which is exactly what #147 decided against.
    if (/^\s*pull_request:/m.test(fuzz)) {
      failures += 1;
      console.error(
        "FAIL      .github/workflows/fuzz.yml: must not trigger on pull_request (issue #147)",
      );
    } else {
      passes += 1;
      console.log("ok        .github/workflows/fuzz.yml: no pull_request trigger");
    }

    // cargo-fuzz is in no Cargo.lock, so the explicit version is the only thing
    // making the CI install reproducible. See docs/supply-chain.adoc section 7.
    const install = fuzz.match(/^\s*run:.*install cargo-fuzz[^\n]*/m);
    if (!install) {
      failures += 1;
      console.error(
        "FAIL      .github/workflows/fuzz.yml: cargo-fuzz install not found (issue #147)",
      );
    } else if (!/--version\s+\S+/.test(install[0])) {
      failures += 1;
      console.error(
        `FAIL      .github/workflows/fuzz.yml: cargo-fuzz install is not version-pinned: ${install[0]}`,
      );
    } else {
      passes += 1;
      console.log("ok        .github/workflows/fuzz.yml: cargo-fuzz install is version-pinned");
    }

    // An explicit budget and a job timeout are what stop a long campaign from
    // degrading into an open-ended gate.
    if (!/timeout-minutes:/.test(fuzz)) {
      failures += 1;
      console.error(
        "FAIL      .github/workflows/fuzz.yml: missing timeout-minutes on the fuzz job (issue #147)",
      );
    } else {
      passes += 1;
      console.log("ok        .github/workflows/fuzz.yml: fuzz job has a timeout");
    }

    if (!/RUNS:/.test(fuzz)) {
      failures += 1;
      console.error(
        "FAIL      .github/workflows/fuzz.yml: campaign budget is not plumbed through (issue #147)",
      );
    } else {
      passes += 1;
      console.log("ok        .github/workflows/fuzz.yml: campaign budget is plumbed through");
    }

    // The default (blank target) input runs every fuzz target sequentially in
    // ONE job, so the per-target -max_total_time caps plus a reserve for
    // checkout, the nightly toolchain, the cargo-fuzz install, one build per
    // target and the artifact upload must fit inside timeout-minutes. When
    // they do not, the job is killed mid-campaign and the last target never
    // runs — the bounded-campaign guarantee silently degrades (review on
    // PR #199). The reserve is the non-fuzz overhead allowance; raise it
    // deliberately, not by accident.
    const fuzzTimeout = fuzz.match(/^\s*timeout-minutes:\s*(\d+)/m);
    const fuzzCap = fuzz.match(/-max_total_time=(\d+)/);
    const fuzzTargets = fuzz.match(/^\s*targets="([a-z_ ]+)"/m);
    const FUZZ_SETUP_RESERVE_SECONDS = 960;
    if (!fuzzTimeout || !fuzzCap || !fuzzTargets) {
      failures += 1;
      console.error(
        "FAIL      .github/workflows/fuzz.yml: cannot read timeout-minutes, -max_total_time or the default target list to bound the campaign (issue #147)",
      );
    } else {
      const fuzzTargetCount = fuzzTargets[1].trim().split(/\s+/).length;
      const fuzzCampaignSeconds = fuzzTargetCount * Number(fuzzCap[1]);
      const fuzzNeededSeconds = fuzzCampaignSeconds + FUZZ_SETUP_RESERVE_SECONDS;
      const fuzzBudgetSeconds = Number(fuzzTimeout[1]) * 60;
      if (fuzzNeededSeconds > fuzzBudgetSeconds) {
        failures += 1;
        console.error(
          `FAIL      .github/workflows/fuzz.yml: ${fuzzTargetCount} targets x -max_total_time=${fuzzCap[1]}s + ${FUZZ_SETUP_RESERVE_SECONDS}s setup reserve = ${fuzzNeededSeconds}s exceeds timeout-minutes ${fuzzTimeout[1]} (${fuzzBudgetSeconds}s); the job would be killed before the last target finishes (issue #147)`,
        );
      } else {
        passes += 1;
        console.log(
          `ok        .github/workflows/fuzz.yml: campaign fits the job budget (${fuzzNeededSeconds}s <= ${fuzzBudgetSeconds}s)`,
        );
      }
    }
  }

  // The PR-time property gate must be deterministic: a fixed RNG seed and a
  // bounded case count. Without both, every run explores a different space and
  // no failure can be reproduced from the documented command.
  //
  // These two file-wide matches see the FIRST occurrence, which is the copy
  // inside the required `rust` job. The drift check further down pins the
  // standalone `regressions` copy to the same values, so the bound below holds
  // for both.
  const regressionSeed = ciWorkflow.match(/PROPTEST_RNG_SEED:\s*"(\d+)"/);
  const regressionCases = ciWorkflow.match(/PROPTEST_CASES:\s*"(\d+)"/);
  if (!regressionSeed || !regressionCases) {
    failures += 1;
    console.error(
      "FAIL      .github/workflows/ci.yml: bounded property regressions job must pin PROPTEST_RNG_SEED and PROPTEST_CASES (issue #147)",
    );
  } else if (Number(regressionCases[1]) > 256) {
    failures += 1;
    console.error(
      `FAIL      .github/workflows/ci.yml: PR-time PROPTEST_CASES=${regressionCases[1]} exceeds the 256-case bound (issue #147)`,
    );
  } else {
    passes += 1;
    console.log(
      `ok        .github/workflows/ci.yml: bounded property regressions seed=${regressionSeed[1]} cases=${regressionCases[1]}`,
    );
  }

  // The checked-in regression corpus is what makes the seeded run replayable.
  // Without it the gate would silently degrade to random search.
  //
  // Existence alone is not enough: proptest writes a header of `#` comments, so
  // a truncated or hand-emptied file still exists and still contains no case.
  // Require at least one real persistence line, which is `cc <seed> ...`.
  const regressionDir = join(root, "crates/scheduler/proptest-regressions");
  if (!existsSync(regressionDir)) {
    failures += 1;
    console.error(
      "FAIL      crates/scheduler/proptest-regressions: missing (issue #147)",
    );
  } else {
    const caseFiles = readdirSync(regressionDir).filter((f) => f.endsWith(".txt"));
    const seeds = caseFiles.flatMap((file) =>
      readFileSync(join(regressionDir, file), "utf8")
        .split("\n")
        .map((line) => line.trim())
        // proptest persistence lines start with `cc `; `#` is a comment.
        .filter((line) => line.startsWith("cc ")),
    );
    if (seeds.length === 0) {
      failures += 1;
      console.error(
        `FAIL      crates/scheduler/proptest-regressions: no persisted proptest case found in ${caseFiles.length || 0} file(s) (issue #147)`,
      );
    } else {
      passes += 1;
      console.log(
        `ok        crates/scheduler/proptest-regressions: ${seeds.length} persisted case(s) present`,
      );
    }
  }

// A regression failure only blocks a merge if it runs inside a REQUIRED status
  // check. main's branch protection requires the three contexts below; the
  // standalone `regressions` job is not one of them, so a failure there leaves
  // the PR mergeable. The seeded subset therefore has to run as a step inside a
  // job whose reported context name IS one of these.
  //
  // The list mirrors the repository settings rather than a file on main, so the
  // assertion works on any branch carrying validate.mjs. Refresh it with
  // `gh api repos/xmeta/aivtuber/branches/main/protection/required_status_checks`
  // when branch protection changes; the last branch below fails if a listed
  // context no longer names a job in ci.yml, which is the staleness signal.
  const REQUIRED_STATUS_CONTEXTS = [
    "Validate schemas and fixtures",
    "Rust checks",
    "Schema and security fixtures",
  ];

  // Split the workflow into its `jobs:` blocks so "which job holds this step"
  // is answerable. A regex over the whole file would happily match the
  // non-gating standalone job, which is exactly the mistake this guards.
  const jobBlocks = [];
  const ciLines = ciWorkflow.split("\n");
  let inJobs = false;
  let currentJob = null;
  for (const line of ciLines) {
    if (line === "jobs:") {
      inJobs = true;
      continue;
    }
    if (!inJobs) continue;
    const header = line.match(/^ {2}([A-Za-z0-9_-]+):\s*$/);
    if (header) {
      currentJob = { id: header[1], lines: [] };
      jobBlocks.push(currentJob);
    } else if (currentJob) {
      currentJob.lines.push(line);
    }
  }
  const jobBody = (job) => job.lines.join("\n");
  // Job keys sit at four spaces; step names are `- name:` at six, so this
  // cannot pick up a step by accident.
  const jobContextName = (job) => jobBody(job).match(/^ {4}name:\s*(.+)$/m)?.[1]?.trim();

  const gatingJob = jobBlocks.find((job) =>
    jobBody(job).includes("- name: Bounded property regression subset (gating)"),
  );
  const gatingSeed = gatingJob && jobBody(gatingJob).match(/PROPTEST_RNG_SEED:\s*"(\d+)"/);
  const missingContexts = REQUIRED_STATUS_CONTEXTS.filter(
    (name) => !jobBlocks.some((job) => jobContextName(job) === name),
  );

  if (!gatingJob || !gatingSeed) {
    failures += 1;
    console.error(
      "FAIL      .github/workflows/ci.yml: missing the seeded property step inside the required `Rust checks` job (issue #147)",
    );
  } else if (!REQUIRED_STATUS_CONTEXTS.includes(jobContextName(gatingJob))) {
    failures += 1;
    console.error(
      `FAIL      .github/workflows/ci.yml: the seeded property step sits in job "${gatingJob.id}", whose context name is not a required status check (${JSON.stringify(REQUIRED_STATUS_CONTEXTS)}), so a failure there will not block a merge (issue #147)`,
    );
  } else if (missingContexts.length > 0) {
    failures += 1;
    console.error(
      `FAIL      .github/workflows/ci.yml: required status checks ${JSON.stringify(missingContexts)} name no job in ci.yml; refresh REQUIRED_STATUS_CONTEXTS in validate.mjs from the branch protection settings (issue #147)`,
    );
  } else {
    passes += 1;
    console.log(
      `ok        .github/workflows/ci.yml: seeded property subset (seed=${gatingSeed[1]}) runs inside required context "${jobContextName(gatingJob)}"`,
    );
  }

  // The gating step is a duplicate of the standalone job's step, so the two
  // must explore the same space. A plain /PROPTEST_RNG_SEED:/ match over the
  // file only sees the first occurrence, so a change to the second copy would
  // pass unnoticed; compare the two job bodies explicitly instead.
  const envOf = (body) => ({
    seed: body.match(/PROPTEST_RNG_SEED:\s*"(\d+)"/)?.[1],
    cases: body.match(/PROPTEST_CASES:\s*"(\d+)"/)?.[1],
    shrinks: body.match(/PROPTEST_MAX_SHRINK_ITERS:\s*"(\d+)"/)?.[1],
  });
  const standaloneJob = jobBlocks.find((job) => job.id === "regressions");
  if (!gatingJob || !standaloneJob) {
    failures += 1;
    console.error(
      "FAIL      .github/workflows/ci.yml: expected both the `rust` and `regressions` jobs to carry the seeded property subset (issue #147)",
    );
  } else {
    const gatingEnv = envOf(jobBody(gatingJob));
    const standaloneEnv = envOf(jobBody(standaloneJob));
    const drifted = ["seed", "cases", "shrinks"].filter(
      (key) => gatingEnv[key] !== standaloneEnv[key],
    );
    if (drifted.length > 0) {
      failures += 1;
      console.error(
        `FAIL      .github/workflows/ci.yml: the gating step and the standalone regressions job disagree on ${drifted.join(", ")} (gating=${JSON.stringify(gatingEnv)} standalone=${JSON.stringify(standaloneEnv)}); a regression CI catches must be the same one the merge gate replays (issue #147)`,
      );
    } else {
      passes += 1;
      console.log(
        `ok        .github/workflows/ci.yml: gating step and standalone job share seed=${gatingEnv.seed} cases=${gatingEnv.cases} shrink_iters=${gatingEnv.shrinks}`,
      );
    }
  }

  // The replay instructions in docs/property-testing.adoc have to match what
  // proptest 1.11.0 actually does, not what is tempting to assume: `cases`
  // counts only newly generated inputs, persisted seeds replay first and are
  // not counted towards it, so PROPTEST_CASES=1 runs the stored case PLUS one
  // new one and only PROPTEST_CASES=0 is replay-only. A doc claiming anything
  // else sends the reader after a proof that cannot hold.
  const propDoc = readFileSync(
    join(root, "docs/property-testing.adoc"),
    "utf8",
  );
  const propSource = readFileSync(
    join(root, "crates/scheduler/tests/properties.rs"),
    "utf8",
  );

  // The documented default must be the case count the suite pins in its
  // ProptestConfig, so a change there cannot leave the doc claiming the
  // proptest default of 256.
  const casesKey = "cases:";
  const casesAt = propSource.indexOf(casesKey);
  const commaAt = casesAt === -1 ? -1 : propSource.indexOf(",", casesAt);
  const pinnedValue =
    casesAt === -1
      ? ""
      : propSource.slice(
          casesAt + casesKey.length,
          commaAt === -1 ? undefined : commaAt,
        ).trim();
  const marker = " cases per property";
  const markerAt = propDoc.indexOf(marker);
  let docStart = markerAt;
  while (
    docStart > 0 &&
    propDoc[docStart - 1] >= "0" &&
    propDoc[docStart - 1] <= "9"
  ) {
    docStart -= 1;
  }
  const documentedValue =
    markerAt === -1 || docStart === markerAt
      ? ""
      : propDoc.slice(docStart, markerAt);
  if (!/^[0-9]+$/.test(pinnedValue)) {
    failures += 1;
    console.error(
      "FAIL      crates/scheduler/tests/properties.rs: no numeric cases: in ProptestConfig (issue #147)",
    );
  } else if (documentedValue !== pinnedValue) {
    failures += 1;
    console.error(
      "FAIL      docs/property-testing.adoc: documents " +
        (documentedValue || "no") +
        " cases per property, but properties.rs pins cases: " +
        pinnedValue +
        " (issue #147)",
    );
  } else {
    passes += 1;
    console.log(
      "ok        docs/property-testing.adoc: default case count matches pinned cases: " +
        pinnedValue,
    );
  }

  // A replay-only command has to exist in the docs, and it has to be CASES=0.
  if (!propDoc.includes("PROPTEST_CASES=0 cargo test")) {
    failures += 1;
    console.error(
      "FAIL      docs/property-testing.adoc: missing the PROPTEST_CASES=0 replay-only command (issue #147)",
    );
  } else {
    passes += 1;
    console.log(
      "ok        docs/property-testing.adoc: replay-only command uses PROPTEST_CASES=0",
    );
  }

  // Any other PROPTEST_CASES=1 in the doc (the PROPTEST_CASES=10000 widening
  // command excluded) is the replay-only claim review caught: at cases: 1
  // proptest still generates one input after replaying persisted seeds.
  const oneAssign = "PROPTEST_CASES=1";
  let claimsReplayOnly = false;
  for (
    let at = propDoc.indexOf(oneAssign);
    at !== -1;
    at = propDoc.indexOf(oneAssign, at + 1)
  ) {
    const next = propDoc[at + oneAssign.length];
    if (next === undefined || next < "0" || next > "9") {
      claimsReplayOnly = true;
      break;
    }
  }
  if (claimsReplayOnly) {
    failures += 1;
    console.error(
      "FAIL      docs/property-testing.adoc: PROPTEST_CASES=1 is presented as a replay-only run; proptest generates one new case after replaying persisted seeds (issue #147)",
    );
  } else {
    passes += 1;
    console.log(
      "ok        docs/property-testing.adoc: no PROPTEST_CASES=1 replay instruction",
    );
  }
}

console.log(`\n${passes} passed, ${failures} failed`);
process.exit(failures > 0 ? 1 : 0);
