import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Ajv2020 } from "ajv/dist/2020.js";
import addFormats from "ajv-formats";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const loadJson = (path) => JSON.parse(readFileSync(join(root, path), "utf8"));

const ajv = new Ajv2020({
  allErrors: true,
  strict: true,
  strictRequired: false,
  allowUnionTypes: true,
});
addFormats(ajv);

const eventSchema = ajv.compile(loadJson("schemas/event-envelope.schema.json"));
const reflexSchema = ajv.compile(loadJson("schemas/reflex-decision.schema.json"));

const output = execFileSync(
  "cargo",
  ["run", "--locked", "--quiet", "-p", "aivtuber-domain", "--example", "schema_parity"],
  { cwd: root, encoding: "utf8" },
);
const emitted = JSON.parse(output);

let failures = 0;
function check(label, validator, value) {
  if (validator(value)) {
    console.log(`ok        ${label}`);
    return;
  }
  failures += 1;
  console.error(`FAIL      ${label}`);
  for (const error of validator.errors ?? []) {
    console.error(`          ${error.instancePath || "/"} ${error.message}`);
  }
}

for (const [index, event] of emitted.events.entries()) {
  check(`Rust EventEnvelope[${index}] -> JSON Schema`, eventSchema, event);
}
for (const [index, decision] of emitted.reflex_decisions.entries()) {
  check(`Rust ReflexDecision[${index}] -> JSON Schema`, reflexSchema, decision);
}

if (emitted.events.length !== 8) {
  failures += 1;
  console.error(`FAIL      expected 8 Rust EventEnvelope samples, got ${emitted.events.length}`);
}
if (emitted.reflex_decisions.length < 1) {
  failures += 1;
  console.error("FAIL      expected at least one Rust ReflexDecision sample");
}

console.log(`\n${emitted.events.length + emitted.reflex_decisions.length - failures} passed, ${failures} failed`);
process.exit(failures > 0 ? 1 : 0);
