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

console.log(`\n${passes} passed, ${failures} failed`);
process.exit(failures > 0 ? 1 : 0);
