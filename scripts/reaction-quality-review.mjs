// Offline pairwise reaction-quality review tooling (issue #151).
//
// prepare <session.json> <packet.json>
//   Emits a reviewer-safe packet with A/B outputs but no base/candidate policy identity.
//
// report <session.json> [benchmark-gate.json] [output-dir]
//   Aggregates completed human labels, preserves disagreement, and optionally renders
//   quality evidence beside #58 benchmark-gate metrics.

import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Ajv2020 } from "ajv/dist/2020.js";
import addFormats from "ajv-formats";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const schemaPath = join(root, "schemas/reaction-quality-review.schema.json");
let compiledSessionValidator = null;

function loadJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function sessionValidator() {
  if (compiledSessionValidator) return compiledSessionValidator;
  const ajv = new Ajv2020({
    allErrors: true,
    strict: true,
    strictRequired: false,
    allowUnionTypes: true,
  });
  addFormats(ajv);
  compiledSessionValidator = ajv.compile(loadJson(schemaPath));
  return compiledSessionValidator;
}

function expectedPresentation(caseId, seed) {
  const firstByte = createHash("sha256")
    .update(`${seed}:${caseId}`)
    .digest()[0];
  return firstByte % 2 === 0
    ? { a_role: "base", b_role: "candidate" }
    : { a_role: "candidate", b_role: "base" };
}

function samePresentation(left, right) {
  return left?.a_role === right.a_role && left?.b_role === right.b_role;
}

function validateSession(session, { requireComplete = false } = {}) {
  const validate = sessionValidator();
  if (!validate(session)) {
    const detail = (validate.errors ?? [])
      .map((error) => `${error.instancePath || "/"} ${error.message}`)
      .join("; ");
    throw new Error(`invalid reaction-quality review session: ${detail}`);
  }
  if (requireComplete && session.status !== "complete") {
    throw new Error("reaction-quality report requires status=complete");
  }
  if (session.status === "complete" && !session.interpretation) {
    throw new Error("complete reaction-quality session requires interpretation");
  }

  const seenCases = new Set();
  for (const testCase of session.cases) {
    if (seenCases.has(testCase.case_id)) {
      throw new Error(`duplicate review case_id: ${testCase.case_id}`);
    }
    seenCases.add(testCase.case_id);

    const expected = expectedPresentation(
      testCase.case_id,
      session.protocol.randomization_seed,
    );
    if (!samePresentation(testCase.presentation, expected)) {
      throw new Error(
        `case ${testCase.case_id} presentation does not match seeded randomization`,
      );
    }

    if (session.status === "complete" && testCase.reviews.length === 0) {
      throw new Error(`complete session has no reviews for ${testCase.case_id}`);
    }

    const reviewers = new Set();
    for (const review of testCase.reviews) {
      if (reviewers.has(review.reviewer_id)) {
        throw new Error(
          `duplicate reviewer_id ${review.reviewer_id} for ${testCase.case_id}`,
        );
      }
      reviewers.add(review.reviewer_id);

      const dimensions = new Set();
      for (const label of review.dimension_labels) {
        if (dimensions.has(label.dimension)) {
          throw new Error(
            `duplicate dimension ${label.dimension} for ${testCase.case_id}/${review.reviewer_id}`,
          );
        }
        dimensions.add(label.dimension);
      }
    }
  }
  return session;
}

function candidateForPresentedRole(testCase, presentedRole) {
  const role =
    presentedRole === "a"
      ? testCase.presentation.a_role
      : testCase.presentation.b_role;
  const candidate = testCase.candidates[role];
  const material = { reaction_summary: candidate.reaction_summary };
  if (candidate.timing_note != null) material.timing_note = candidate.timing_note;
  if (candidate.output_reference != null) {
    material.output_reference = candidate.output_reference;
  }
  return material;
}

function renderBlindPacket(session) {
  validateSession(session);
  return {
    schema_version: "1",
    review_session_id: session.review_session_id,
    dataset: {
      dataset_id: session.dataset.dataset_id,
      dataset_version: session.dataset.dataset_version,
      partition: session.dataset.partition,
      partition_revision: session.dataset.partition_revision,
    },
    protocol: {
      protocol_id: session.protocol.protocol_id,
      protocol_version: session.protocol.protocol_version,
      instructions_version: session.protocol.instructions_version,
      identity_hidden_during_review: true,
    },
    allowed_choices: ["a_better", "b_better", "tie", "both_unacceptable"],
    cases: session.cases.map((testCase) => ({
      case_id: testCase.case_id,
      category: testCase.category,
      a: candidateForPresentedRole(testCase, "a"),
      b: candidateForPresentedRole(testCase, "b"),
    })),
  };
}

function normalizedChoice(testCase, choice) {
  if (choice === "tie" || choice === "both_unacceptable") return choice;
  const presentedRole = choice === "a_better" ? "a" : "b";
  const role =
    presentedRole === "a"
      ? testCase.presentation.a_role
      : testCase.presentation.b_role;
  return role === "base" ? "base_better" : "candidate_better";
}

function emptyCounts() {
  return {
    base_better: 0,
    candidate_better: 0,
    tie: 0,
    both_unacceptable: 0,
  };
}

function mapDimensionPreference(testCase, preference) {
  if (
    preference === "tie" ||
    preference === "both_unacceptable" ||
    preference === "not_applicable"
  ) {
    return preference;
  }
  const presentedRole = preference === "a_better" ? "a" : "b";
  const role =
    presentedRole === "a"
      ? testCase.presentation.a_role
      : testCase.presentation.b_role;
  return role === "base" ? "base_better" : "candidate_better";
}

function aggregateSession(session) {
  validateSession(session, { requireComplete: true });

  const totals = emptyCounts();
  const reasonCounts = {};
  const dimensionCounts = {};
  const cases = [];

  for (const testCase of session.cases) {
    const counts = emptyCounts();
    const outcomes = new Set();
    const reasons = {};

    for (const review of testCase.reviews) {
      const outcome = normalizedChoice(testCase, review.choice);
      counts[outcome] += 1;
      totals[outcome] += 1;
      outcomes.add(outcome);

      for (const reason of review.reason_labels) {
        reasons[reason] = (reasons[reason] ?? 0) + 1;
        reasonCounts[reason] = (reasonCounts[reason] ?? 0) + 1;
      }

      for (const label of review.dimension_labels) {
        if (!dimensionCounts[label.dimension]) {
          dimensionCounts[label.dimension] = {
            ...emptyCounts(),
            not_applicable: 0,
          };
        }
        const preference = mapDimensionPreference(testCase, label.preference);
        dimensionCounts[label.dimension][preference] += 1;
      }
    }

    cases.push({
      case_id: testCase.case_id,
      category: testCase.category,
      reviewer_count: testCase.reviews.length,
      counts,
      disagreement: outcomes.size > 1,
      reason_counts: reasons,
    });
  }

  const qualityGuardrail = session.interpretation.quality_guardrail;
  const claimStatus =
    qualityGuardrail === "regression"
      ? "blocked_quality_regression"
      : qualityGuardrail === "inconclusive"
        ? "not_supported_inconclusive_quality"
        : "quality_guardrail_passed";

  return {
    schema_version: "1",
    review_session_id: session.review_session_id,
    dataset: session.dataset,
    comparison: session.comparison,
    quality_guardrail: qualityGuardrail,
    product_improvement_claim_status: claimStatus,
    decision: session.interpretation.decision,
    rationale: session.interpretation.rationale,
    totals,
    reason_counts: reasonCounts,
    dimension_counts: dimensionCounts,
    cases,
  };
}

function fmtDelta(value) {
  return value == null ? "-" : `${value >= 0 ? "+" : ""}${value.toFixed(1)}%`;
}

function renderMarkdown(summary, benchmarkGate = null) {
  const lines = [
    `# Reaction-quality evidence: ${summary.review_session_id}`,
    "",
    `Quality guardrail: **${summary.quality_guardrail.toUpperCase()}**`,
    `Decision: **${summary.decision.toUpperCase()}**`,
    `Product-improvement claim status: **${summary.product_improvement_claim_status}**`,
    "",
    summary.rationale,
    "",
    "## Pairwise review outcomes",
    "",
    "| Base better | Candidate better | Tie | Both unacceptable |",
    "|---:|---:|---:|---:|",
    `| ${summary.totals.base_better} | ${summary.totals.candidate_better} | ${summary.totals.tie} | ${summary.totals.both_unacceptable} |`,
    "",
    "These counts are evidence dimensions, not a scalar quality score.",
    "",
  ];

  const dimensions = Object.entries(summary.dimension_counts).sort(([a], [b]) =>
    a.localeCompare(b),
  );
  if (dimensions.length > 0) {
    lines.push(
      "## Dimension evidence",
      "",
      "| Dimension | Base better | Candidate better | Tie | Both unacceptable | N/A |",
      "|---|---:|---:|---:|---:|---:|",
    );
    for (const [dimension, counts] of dimensions) {
      lines.push(
        `| ${dimension} | ${counts.base_better} | ${counts.candidate_better} | ${counts.tie} | ${counts.both_unacceptable} | ${counts.not_applicable} |`,
      );
    }
    lines.push("");
  }

  lines.push(
    "## Case-level evidence",
    "",
    "| Case | Category | Reviewers | Base better | Candidate better | Tie | Both unacceptable | Disagreement |",
    "|---|---|---:|---:|---:|---:|---:|---|",
  );
  for (const testCase of summary.cases) {
    lines.push(
      `| ${testCase.case_id} | ${testCase.category} | ${testCase.reviewer_count} | ${testCase.counts.base_better} | ${testCase.counts.candidate_better} | ${testCase.counts.tie} | ${testCase.counts.both_unacceptable} | ${testCase.disagreement ? "yes" : "no"} |`,
    );
  }
  lines.push("");

  const reasons = Object.entries(summary.reason_counts).sort(([a], [b]) =>
    a.localeCompare(b),
  );
  if (reasons.length > 0) {
    lines.push("## Reason labels", "", "| Reason | Count |", "|---|---:|");
    for (const [reason, count] of reasons) {
      lines.push(`| ${reason} | ${count} |`);
    }
    lines.push("");
  }

  if (benchmarkGate) {
    lines.push(
      "## #58 benchmark evidence",
      "",
      `Benchmark verdict: **${String(benchmarkGate.verdict ?? "unknown").toUpperCase()}**`,
      "",
      "| Metric | Base | Head | Delta | Status |",
      "|---|---:|---:|---:|---|",
    );
    for (const metric of benchmarkGate.metrics ?? []) {
      lines.push(
        `| ${metric.name} | ${metric.base ?? "-"} | ${metric.head ?? "-"} | ${fmtDelta(metric.delta_percent)} | ${metric.status ?? "-"} |`,
      );
    }
    lines.push(
      "",
      "Benchmark and human-review evidence are shown together but remain separate evidence classes.",
      "A quality regression blocks an unqualified product-improvement claim even when latency/cost/reuse proxies improve.",
      "",
    );
  }

  return lines.join("\n");
}

function prepare(sessionPath, packetPath) {
  const session = validateSession(loadJson(sessionPath));
  const packet = renderBlindPacket(session);
  mkdirSync(dirname(packetPath), { recursive: true });
  writeFileSync(packetPath, JSON.stringify(packet, null, 2) + "\n");
  console.log(`wrote blinded review packet: ${packetPath}`);
}

function report(sessionPath, benchmarkPath, outputDir) {
  const session = validateSession(loadJson(sessionPath), { requireComplete: true });
  const summary = aggregateSession(session);
  const benchmark = benchmarkPath ? loadJson(benchmarkPath) : null;
  if (benchmark && !Array.isArray(benchmark.metrics)) {
    throw new Error(
      "benchmark evidence must be benchmark-compare gate-report.json (metrics must be an array)",
    );
  }
  const markdown = renderMarkdown(summary, benchmark);
  mkdirSync(outputDir, { recursive: true });
  writeFileSync(
    join(outputDir, "quality-summary.json"),
    JSON.stringify(
      {
        ...summary,
        benchmark_evidence: benchmark
          ? {
              verdict: benchmark.verdict ?? null,
              base_commit: benchmark.base_commit ?? null,
              head_commit: benchmark.head_commit ?? null,
              metrics: benchmark.metrics ?? [],
              invariants: benchmark.invariants ?? [],
            }
          : null,
      },
      null,
      2,
    ) + "\n",
  );
  writeFileSync(join(outputDir, "quality-summary.md"), markdown + "\n");
  console.log(markdown);
}

function usage() {
  console.error(
    [
      "usage:",
      "  reaction-quality-review prepare <session.json> <packet.json>",
      "  reaction-quality-review report <session.json> [benchmark-gate.json] [output-dir]",
    ].join("\n"),
  );
}

function main(argv) {
  const [command, ...args] = argv;
  if (command === "prepare" && args.length === 2) {
    prepare(args[0], args[1]);
    return;
  }
  if (command === "report" && args.length >= 1 && args.length <= 3) {
    report(args[0], args[1] ?? null, args[2] ?? "target/reaction-quality-review");
    return;
  }
  usage();
  process.exitCode = 2;
}

const invokedDirectly = process.argv[1]
  ?.replace(/\\/g, "/")
  .endsWith("reaction-quality-review.mjs");
if (invokedDirectly) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(`reaction-quality-review: ${error.message}`);
    process.exitCode = 1;
  }
}

export const __testables = {
  expectedPresentation,
  validateSession,
  renderBlindPacket,
  normalizedChoice,
  aggregateSession,
  renderMarkdown,
};
