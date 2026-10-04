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
// - examples/evaluation/moderation-evaluation/*.json must conform to the moderation-evaluation schema,
//   invalid/ must REJECT against it, and inconsistent/ must pass the schema while tripping a
//   cross-field check (#77).
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
const shadowDivergence = validators["schemas/shadow-divergence-report.schema.json"];
const moderationEvaluation = validators["schemas/moderation-evaluation.schema.json"];

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

// Issue #165: an aggregated shadow divergence report must stay evidence, not a
// promotion mechanism. The schema already refuses extra fields and non-diverging
// case references; these checks pin the two invariants that a schema alone
// cannot express.
const SHADOW_DIVERGENCE_CATEGORIES = [
  "same_route_same_target",
  "same_route_different_target",
  "route_transition",
  "response_vs_silent",
  "fallback_vs_success",
  "fallback_reason_mismatch",
  "shadow_unusable",
  "target_incomparable",
];
// Agreement, plus the two categories that are failures to compare rather than
// disagreements. None of them may be retained as a reviewable reference.
const SHADOW_NON_DIVERGING = [
  "same_route_same_target",
  "shadow_unusable",
  "target_incomparable",
];
// Categories kept out of the divergence-rate denominator.
const SHADOW_NON_COMPARABLE = ["shadow_unusable", "target_incomparable"];

const shadowDivergenceDocuments = [];
for (const file of listJson("examples/evaluation/shadow-divergence")) {
  if (!shadowDivergence) break;
  const doc = loadJson(join(root, file));
  const valid = shadowDivergence(doc);
  report(valid, file, shadowDivergence);
  if (valid) shadowDivergenceDocuments.push({ file, doc });
}

for (const file of listJson("examples/evaluation/shadow-divergence/invalid")) {
  if (!shadowDivergence) break;
  const valid = shadowDivergence(loadJson(join(root, file)));
  if (valid) {
    failures += 1;
    console.error(`FAIL      ${file} (expected rejection, but it validated)`);
  } else {
    passes += 1;
    console.log(`ok        ${file} (rejected as intended)`);
  }
}

function shadowDivergenceCrossFieldProblems(doc) {
  const problems = [];

  // Counts must add up. A report whose categories do not sum to the total
  // cannot be consumed by #58 as a metric.
  const counted = SHADOW_DIVERGENCE_CATEGORIES.reduce(
    (sum, category) => sum + (doc.category_counts[category] ?? 0),
    0,
  );
  if (counted !== doc.total_comparisons) {
    problems.push(
      `category counts sum to ${counted}, not total_comparisons ${doc.total_comparisons}`,
    );
  }

  // The divergence rate has comparable comparisons as its denominator, so
  // neither a broken shadow policy nor an unevidenced target pair can make a
  // policy look good by producing evidence nobody could compare.
  const comparable =
    doc.total_comparisons -
    SHADOW_NON_COMPARABLE.reduce(
      (sum, category) => sum + (doc.category_counts[category] ?? 0),
      0,
    );
  if (comparable !== doc.comparable_comparisons) {
    problems.push(
      `comparable_comparisons is ${doc.comparable_comparisons}, but total minus ${SHADOW_NON_COMPARABLE.join("/")} is ${comparable}`,
    );
  }
  if (doc.diverged_comparisons > doc.comparable_comparisons) {
    problems.push(
      `diverged_comparisons ${doc.diverged_comparisons} exceeds comparable_comparisons ${doc.comparable_comparisons}`,
    );
  }

  // #58 reads `diverged_comparisons` as the headline number, so the diverging
  // category counts must sum to it. Without this, `route_transition: 5` beside
  // `diverged_comparisons: 0` and `rate_pct: 0` passes every other check while
  // publishing a self-contradictory report.
  const divergingSum = SHADOW_DIVERGENCE_CATEGORIES.filter(
    (category) => !SHADOW_NON_DIVERGING.includes(category),
  ).reduce((sum, category) => sum + (doc.category_counts[category] ?? 0), 0);
  if (divergingSum !== doc.diverged_comparisons) {
    problems.push(
      `diverging categories sum to ${divergingSum}, but diverged_comparisons is ${doc.diverged_comparisons}`,
    );
  }

  // A published rate must agree with the counts it was derived from; a stale or
  // hand-edited metric would otherwise let a consumer believe a rate the
  // categories contradict.
  const rate = doc.metrics?.["shadow.divergence.rate_pct"];
  if (rate) {
    const expected =
      doc.comparable_comparisons === 0
        ? 0
        : (doc.diverged_comparisons / doc.comparable_comparisons) * 100;
    const matches =
      Math.abs(rate.value - expected) < 1e-9 &&
      rate.sample_count === doc.comparable_comparisons;
    if (!matches) {
      problems.push(
        `rate_pct is ${rate.value} over ${rate.sample_count} samples, but diverged/comparable is ${expected} over ${doc.comparable_comparisons}`,
      );
    }
  }

  // #58 reads the per-category metrics, not `category_counts`, so a metric that
  // disagrees with the counts it was derived from publishes a false number
  // while every headline total still looks right. Both halves are pinned:
  // `value` must equal the category count, and `sample_count` must be the
  // number of records that count was taken over.
  for (const category of SHADOW_DIVERGENCE_CATEGORIES) {
    const metric = doc.metrics?.[`shadow.divergence.${category}_count`];
    if (!metric) continue;
    const expected = doc.category_counts[category] ?? 0;
    if (metric.value !== expected) {
      problems.push(
        `shadow.divergence.${category}_count is ${metric.value}, but category_counts.${category} is ${expected}`,
      );
    }
    if (metric.sample_count !== doc.total_comparisons) {
      problems.push(
        `shadow.divergence.${category}_count is measured over ${metric.sample_count} samples, but total_comparisons is ${doc.total_comparisons}`,
      );
    }
  }

  // The operational counters describe the same batch, so their sample_count is
  // the batch size too. Their values are measured rather than derived from the
  // records, so there is nothing here to compare them against.
  for (const [name, metric] of Object.entries(doc.metrics ?? {})) {
    if (!name.startsWith("shadow.operations.")) continue;
    if (metric.sample_count !== doc.total_comparisons) {
      problems.push(
        `${name} is measured over ${metric.sample_count} samples, but total_comparisons is ${doc.total_comparisons}`,
      );
    }
  }

  // Evidence may reference divergent cases, never agreement or a comparison
  // that could not be made.
  const badRef = (doc.divergent_cases ?? []).find((reference) =>
    SHADOW_NON_DIVERGING.includes(reference.category),
  );
  if (badRef) {
    problems.push(
      `divergent_cases references ${badRef.event_id} with non-diverging category ${badRef.category}`,
    );
  }

  // The truncation flag must agree with the retained references, otherwise a
  // partial report reads as a complete one.
  const retained = (doc.divergent_cases ?? []).length;
  if ((retained < doc.diverged_comparisons) !== doc.divergent_cases_truncated) {
    problems.push(
      `divergent_cases_truncated is ${doc.divergent_cases_truncated} but retained ${retained} of ${doc.diverged_comparisons}`,
    );
  }

  return problems;
}

for (const { file, doc } of shadowDivergenceDocuments) {
  const problems = shadowDivergenceCrossFieldProblems(doc);
  if (problems.length === 0) {
    passes += 1;
    console.log(`ok        ${file}: cross-field evidence is self-consistent`);
  } else {
    failures += problems.length;
    for (const problem of problems) {
      console.error(`FAIL      ${file}: ${problem}`);
    }
  }
}

// Documents that satisfy the schema but contradict themselves. These are not in
// `invalid/`, which is the schema-rejection corpus: a self-contradictory report
// *is* structurally valid, it is simply untrustworthy, which is exactly what
// these cross-field checks exist to catch.
for (const file of listJson("examples/evaluation/shadow-divergence/inconsistent")) {
  if (!shadowDivergence) break;
  const doc = loadJson(join(root, file));
  if (!shadowDivergence(doc)) {
    failures += 1;
    console.error(
      `FAIL      ${file} (expected to pass the schema so the cross-field checks can judge it)`,
    );
    continue;
  }
  const problems = shadowDivergenceCrossFieldProblems(doc);
  if (problems.length === 0) {
    failures += 1;
    console.error(`FAIL      ${file} (expected a contradiction, but every check passed)`);
  } else {
    passes += 1;
    console.log(`ok        ${file} (rejected as intended: ${problems.join("; ")})`);
  }
}


// ---------------------------------------------------------------------------
// Issue #77: shadow moderation evaluation must be measurable offline, with no
// path from a label to an automatic destructive action.
// ---------------------------------------------------------------------------
const MODERATION_CATEGORIES = [
  "none",
  "spam",
  "flood",
  "harassment",
  "scam",
  "hate_speech",
  "threat",
  "sexual_content",
  "impersonation",
];
// Destructive in the sense of #77: acting removes or restricts something the
// author can observe. `human_review` is deliberately absent, because escalating
// to a human is the escape hatch that keeps `ban` human-approved.
const MODERATION_DESTRUCTIVE = ["delete", "timeout", "ban"];
const MODERATION_DESTRUCTIVE_METRICS = new Set(
  MODERATION_DESTRUCTIVE.map((action) => action),
);

const moderationEvaluationDocuments = [];
for (const file of listJson("examples/evaluation/moderation-evaluation")) {
  if (!moderationEvaluation) break;
  const doc = loadJson(join(root, file));
  const valid = moderationEvaluation(doc);
  report(valid, file, moderationEvaluation);
  if (valid) moderationEvaluationDocuments.push({ file, doc });
}

for (const file of listJson("examples/evaluation/moderation-evaluation/invalid")) {
  if (!moderationEvaluation) break;
  const valid = moderationEvaluation(loadJson(join(root, file)));
  if (valid) {
    failures += 1;
    console.error(`FAIL      ${file} (expected rejection, but it validated)`);
  } else {
    passes += 1;
    console.log(`ok        ${file} (rejected as intended)`);
  }
}

// Cross-field checks a JSON Schema cannot express, returned as messages. The
// same predicate serves the valid corpus (expected clean) and the
// `inconsistent/` corpus (expected to trip at least one), so the two cannot
// drift apart.
function moderationEvaluationCrossFieldProblems(doc) {
  const problems = [];
  const aggregate = doc.aggregate ?? {};
  const cases = doc.cases ?? [];
  const metrics = aggregate.metrics ?? {};

  // Every case needs at least one reviewer. An unreviewed case cannot be
  // scored, so counting it is reporting evidence nobody gave.
  for (const testCase of cases) {
    if ((testCase.reviews ?? []).length === 0) {
      problems.push(`${testCase.case_id} has no reviewer label`);
    }
  }

  // A review's own label must appear in its acceptable set, and a review that
  // declares several acceptable outcomes must list more than one. Without this,
  // "the policy legitimately allows multiple answers" becomes a claim nobody has
  // to honour.
  for (const testCase of cases) {
    for (const review of testCase.reviews ?? []) {
      if (!review.acceptable_actions.includes(review.label)) {
        problems.push(
          `${testCase.case_id}: ${review.reviewer_id} labels ${review.label} but does not list it as acceptable`,
        );
      }
      if (
        review.ambiguity === "multiple_acceptable" &&
        review.acceptable_actions.length < 2
      ) {
        problems.push(
          `${testCase.case_id}: ${review.reviewer_id} marks multiple_acceptable but lists only ${review.acceptable_actions.length} acceptable action(s)`,
        );
      }
    }
  }

  // The headline totals must follow from the cases: a moderation rate is read
  // as "how often the policy was wrong", so a stale count reads as a measured
  // fact it is not.
  if (aggregate.total_cases !== cases.length) {
    problems.push(
      `total_cases is ${aggregate.total_cases}, but the corpus holds ${cases.length} case(s)`,
    );
  }

  const recommendationCounts = {};
  for (const testCase of cases) {
    const key = `${testCase.recommendation.category}/${testCase.recommendation.action}`;
    recommendationCounts[key] = (recommendationCounts[key] ?? 0) + 1;
  }
  for (const category of MODERATION_CATEGORIES) {
    const expected = cases.filter(
      (testCase) => testCase.recommendation.category === category,
    ).length;
    const stated = aggregate.category_counts?.[category];
    if (stated !== undefined && stated !== expected) {
      problems.push(
        `category_counts.${category} is ${stated}, but ${expected} case(s) recommend that category`,
      );
    }
  }
  for (const [key, count] of Object.entries(aggregate.outcome_counts ?? {})) {
    const expected = recommendationCounts[key];
    if (expected === undefined) {
      problems.push(
        `outcome_counts.${key} counts a recommendation no case made`,
      );
    } else if (count !== expected) {
      problems.push(
        `outcome_counts.${key} is ${count}, but ${expected} case(s) recommend it`,
      );
    }
  }
  for (const [key, count] of Object.entries(recommendationCounts)) {
    if ((aggregate.outcome_counts ?? {})[key] === undefined) {
      problems.push(
        `outcome_counts is missing ${key}, which ${count} case(s) recommend`,
      );
    }
  }

  // #58 reads the flat metric map rather than the counts, so a metric that
  // disagrees with the count it came from publishes a false number while every
  // headline total still looks right.
  for (const [name, metric] of Object.entries(metrics)) {
    if (!name.startsWith("shadow.moderation.")) continue;
    if (metric.sample_count !== aggregate.total_cases) {
      problems.push(
        `${name} is measured over ${metric.sample_count} samples, but total_cases is ${aggregate.total_cases}`,
      );
    }
  }
  for (const category of MODERATION_CATEGORIES) {
    const metric = metrics[`shadow.moderation.${category}_count`];
    if (!metric) continue;
    const expected = aggregate.category_counts?.[category] ?? 0;
    if (metric.value !== expected) {
      problems.push(
        `shadow.moderation.${category}_count is ${metric.value}, but category_counts.${category} is ${expected}`,
      );
    }
  }
  for (const [name, key] of [
    ["agreed", "agreed_cases"],
    ["overridden", "overridden_cases"],
    ["reviewer_disagreement", "reviewer_disagreement_cases"],
    ["benign_suppressed", "benign_suppressed_cases"],
    ["destructive_overridden", "destructive_overridden_cases"],
    ["total", "total_cases"],
  ]) {
    const metric = metrics[`shadow.moderation.${name}_count`];
    if (!metric) continue;
    if (metric.value !== aggregate[key]) {
      problems.push(
        `shadow.moderation.${name}_count is ${metric.value}, but ${key} is ${aggregate[key]}`,
      );
    }
  }
  const rate = metrics["shadow.moderation.override_rate_pct"];
  if (rate) {
    const expected =
      aggregate.total_cases === 0
        ? 0
        : (aggregate.overridden_cases / aggregate.total_cases) * 100;
    if (
      Math.abs(rate.value - expected) >= 1e-9 ||
      rate.sample_count !== aggregate.total_cases
    ) {
      problems.push(
        `override_rate_pct is ${rate.value} over ${rate.sample_count} samples, but overridden/total is ${expected} over ${aggregate.total_cases}`,
      );
    }
  }

  // Suppressing a benign case is the asymmetric harm, so it can never exceed
  // the number of overridden cases. Counting it as anything else would let a
  // classifier look clean by mislabelling its own errors.
  if (aggregate.benign_suppressed_cases > aggregate.overridden_cases) {
    problems.push(
      `benign_suppressed_cases is ${aggregate.benign_suppressed_cases}, but only ${aggregate.overridden_cases} case(s) were overridden`,
    );
  }
  if (aggregate.destructive_overridden_cases > aggregate.overridden_cases) {
    problems.push(
      `destructive_overridden_cases is ${aggregate.destructive_overridden_cases}, but only ${aggregate.overridden_cases} case(s) were overridden`,
    );
  }
  if (aggregate.agreed_cases + aggregate.overridden_cases !== aggregate.total_cases) {
    problems.push(
      `agreed_cases + overridden_cases is ${aggregate.agreed_cases + aggregate.overridden_cases}, not total_cases ${aggregate.total_cases}`,
    );
  }
  if (aggregate.reviewer_disagreement_cases > aggregate.total_cases) {
    problems.push(
      `reviewer_disagreement_cases is ${aggregate.reviewer_disagreement_cases}, but the corpus holds only ${aggregate.total_cases} case(s)`,
    );
  }

  // Both counters are bounded by what the cases can actually support, so a
  // report cannot be flattered by inflating them.
  const destructive = cases.filter((testCase) =>
    MODERATION_DESTRUCTIVE_METRICS.has(testCase.recommendation.action),
  ).length;
  if (aggregate.destructive_overridden_cases > destructive) {
    problems.push(
      `destructive_overridden_cases is ${aggregate.destructive_overridden_cases}, but only ${destructive} case(s) recommend a destructive action`,
    );
  }
  // Benign-ness is the *reviewers'* call, not the policy's: a policy that
  // labels criticism as harassment would otherwise never be counted as
  // suppressing benign content, which is the harm this metric exists to show.
  const suppressibleBenign = cases.filter(
    (testCase) =>
      testCase.recommendation.action !== "allow" &&
      (testCase.reviews ?? []).length > 0 &&
      testCase.reviews.every((review) => review.category === "none"),
  ).length;
  if (aggregate.benign_suppressed_cases > suppressibleBenign) {
    problems.push(
      `benign_suppressed_cases is ${aggregate.benign_suppressed_cases}, but only ${suppressibleBenign} reviewer-benign case(s) received a suppressive recommendation`,
    );
  }

  return problems;
}

for (const { file, doc } of moderationEvaluationDocuments) {
  const problems = moderationEvaluationCrossFieldProblems(doc);
  if (problems.length === 0) {
    passes += 1;
    console.log(`ok        ${file}: cross-field evidence is self-consistent`);
  } else {
    failures += problems.length;
    for (const problem of problems) {
      console.error(`FAIL      ${file}: ${problem}`);
    }
  }
}

// Schema-valid but self-contradictory documents. A contradictory evaluation is
// structurally fine and simply untrustworthy, which is exactly what these
// cross-field checks exist to catch.
for (const file of listJson("examples/evaluation/moderation-evaluation/inconsistent")) {
  if (!moderationEvaluation) break;
  const doc = loadJson(join(root, file));
  if (!moderationEvaluation(doc)) {
    failures += 1;
    console.error(
      `FAIL      ${file} (expected to pass the schema so the cross-field checks can judge it)`,
    );
    continue;
  }
  const problems = moderationEvaluationCrossFieldProblems(doc);
  if (problems.length === 0) {
    failures += 1;
    console.error(`FAIL      ${file} (expected a contradiction, but every check passed)`);
  } else {
    passes += 1;
    console.log(`ok        ${file} (rejected as intended: ${problems.join("; ")})`);
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
