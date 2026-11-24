import { test, expect } from "bun:test";
import Ajv2020 from "ajv/dist/2020.js";
import addFormats from "ajv-formats";
import { readFileSync } from "node:fs";
import { manifest, DATASET, DURATION_MS } from "../scripts/watchable-hour-feed.mjs";

test("watchable-hour manifest is repeatable, ordered, bounded, and schema-valid", () => {
  const schema = JSON.parse(readFileSync(new URL("../schemas/event-envelope.schema.json", import.meta.url), "utf8"));
  const ajv = new Ajv2020({ strict: false });
  addFormats(ajv);
  const validate = ajv.compile(schema);
  const run = manifest("test-a");
  expect(run).toEqual(manifest("test-a"));
  expect(run.dataset).toBe(DATASET);
  expect(run.duration_ms).toBe(3600000);
  expect(run.events.length).toBe(64);
  const ids = new Set();
  let prior = -1;
  for (const [i, {offset_ms, envelope}] of run.events.entries()) {
    expect(validate(envelope)).toBe(true);
    expect(offset_ms).toBeGreaterThan(prior);
    expect(offset_ms).toBeLessThan(DURATION_MS);
    expect(envelope.sequence).toBe(i + 1);
    expect(envelope.source_class).toBe("public_chat");
    expect(envelope.trust_level).toBe("untrusted");
    expect(envelope.plane).toBe("content");
    expect(envelope.kind).toBe("chat.message");
    expect(Object.keys(envelope.payload).sort()).toEqual(["language", "text"]);
    expect(envelope).not.toHaveProperty("authorization");
    expect(envelope.actor_id.startsWith("viewer:synthetic-")).toBe(true);
    ids.add(envelope.event_id);
    prior = offset_ms;
  }
  expect(ids.size).toBe(run.events.length);
  expect(manifest("test-b").events.every(({envelope}) => !ids.has(envelope.event_id))).toBe(true);
  expect(run.events.filter(({offset_ms}) => offset_ms >= 720000 && offset_ms < 1080000)).toHaveLength(0);
  expect(run.events.filter(({offset_ms}) => offset_ms >= 1800000 && offset_ms < 2400000)).toHaveLength(0);
  const wave = run.events.filter(({offset_ms}) => offset_ms >= 360000 && offset_ms < 363000);
  expect(wave).toHaveLength(5);
  expect(new Set(wave.map(({envelope}) => envelope.actor_id)).size).toBe(5);
  expect(wave[1].offset_ms - wave[0].offset_ms).toBe(600);
});

test("invalid run identities are refused", () => {
  for (const id of ["", "../a", "a b", "a".repeat(33), undefined]) {
    expect(() => manifest(id)).toThrow();
  }
});
