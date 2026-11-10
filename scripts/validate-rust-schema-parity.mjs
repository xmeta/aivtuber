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
const avatarCommandSchema = ajv.compile(loadJson("schemas/avatar-playback-command.schema.json"));
const speechObservationSchema = ajv.compile(loadJson("schemas/speech-observation.schema.json"));
const speechTranscriptSchema = ajv.compile(loadJson("schemas/speech-transcript.schema.json"));
const perceptionObservationSchema = ajv.compile(loadJson("schemas/perception-observation.schema.json"));

const output = execFileSync(
  "cargo",
  ["run", "--locked", "--quiet", "-p", "aivtuber-domain", "--example", "schema_parity"],
  { cwd: root, encoding: "utf8" },
);
const emitted = JSON.parse(output);

const avatarOutput = execFileSync(
  "cargo",
  ["run", "--locked", "--quiet", "-p", "aivtuber-runtime", "--example", "avatar_command_parity"],
  { cwd: root, encoding: "utf8" },
);
const avatarEmitted = JSON.parse(avatarOutput);

const speechOutput = execFileSync(
  "cargo",
  ["run", "--locked", "--quiet", "-p", "aivtuber-domain", "--example", "speech_observation_parity"],
  { cwd: root, encoding: "utf8" },
);
const speechEmitted = JSON.parse(speechOutput);

const transcriptOutput = execFileSync(
  "cargo",
  ["run", "--locked", "--quiet", "-p", "aivtuber-domain", "--example", "speech_transcript_parity"],
  { cwd: root, encoding: "utf8" },
);
const transcriptEmitted = JSON.parse(transcriptOutput);

const perceptionOutput = execFileSync(
  "cargo",
  ["run", "--locked", "--quiet", "-p", "aivtuber-domain", "--example", "perception_observation_parity"],
  { cwd: root, encoding: "utf8" },
);
const perceptionEmitted = JSON.parse(perceptionOutput);

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

for (const [index, command] of avatarEmitted.commands.entries()) {
  check(`Rust AvatarPlaybackCommand[${index}] (${command.kind}) -> JSON Schema`, avatarCommandSchema, command);
}
for (const [index, batch] of speechEmitted.batches.entries()) {
  check(`Rust SpeechObservationBatch[${index}] -> JSON Schema`, speechObservationSchema, batch);
}
if (speechEmitted.batches.length < 4) {
  failures += 1;
  console.error("FAIL      expected at least 4 Rust SpeechObservationBatch samples");
}

for (const [index, batch] of transcriptEmitted.batches.entries()) {
  check(`Rust SpeechTranscriptBatch[${index}] -> JSON Schema`, speechTranscriptSchema, batch);
}
if (transcriptEmitted.batches.length < 6) {
  failures += 1;
  console.error("FAIL      expected at least 6 Rust SpeechTranscriptBatch samples");
}

for (const [index, batch] of perceptionEmitted.batches.entries()) {
  check(`Rust PerceptionObservationBatch[${index}] -> JSON Schema`, perceptionObservationSchema, batch);
}
if (perceptionEmitted.batches.length < 7) {
  failures += 1;
  console.error("FAIL      expected at least 7 Rust PerceptionObservationBatch samples");
}

if (!avatarEmitted.commands.some((command) => command.kind === "reset")) {
  failures += 1;
  console.error("FAIL      expected a Rust AvatarPlaybackCommand::Reset sample");
}

if (emitted.events.length !== 8) {
  failures += 1;
  console.error(`FAIL      expected 8 Rust EventEnvelope samples, got ${emitted.events.length}`);
}
if (emitted.reflex_decisions.length < 1) {
  failures += 1;
  console.error("FAIL      expected at least one Rust ReflexDecision sample");
}

const total = emitted.events.length + emitted.reflex_decisions.length + avatarEmitted.commands.length + speechEmitted.batches.length + transcriptEmitted.batches.length + perceptionEmitted.batches.length;
console.log(`\n${total - failures} passed, ${failures} failed`);
process.exit(failures > 0 ? 1 : 0);
