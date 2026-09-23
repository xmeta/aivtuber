// Validates repository examples/fixtures against their JSON Schemas.
// - examples/events/*.json must conform to event-envelope.schema.json
// - examples/events/invalid/*.json must REJECT against event-envelope.schema.json
// - examples/reflex-decisions/*.json must conform to reflex-decision.schema.json
// - examples/security/*.json must conform to security-regression-case.schema.json,
//   and their embedded input.event must conform to event-envelope.schema.json
// - examples/assets/*.json must conform to performance-asset.schema.json
//
// Usage: bun scripts/validate.mjs  (or: node scripts/validate.mjs)

import { Ajv2020 } from "ajv/dist/2020.js";
import addFormats from "ajv-formats";
import { readFileSync, readdirSync, existsSync } from "node:fs";
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

for (const file of listJson("examples/security")) {
  const doc = loadJson(join(root, file));
  if (regression) report(regression(doc), file, regression);
  if (envelope && doc?.input?.event) {
    report(envelope(doc.input.event), `${file} -> input.event`, envelope);
  }
}

for (const file of listJson("examples/assets")) {
  if (!asset) break;
  report(asset(loadJson(join(root, file))), file, asset);
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

console.log(`\n${passes} passed, ${failures} failed`);
process.exit(failures > 0 ? 1 : 0);
