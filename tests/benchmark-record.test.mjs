import { describe, expect, test } from "bun:test";

import { __testables } from "../scripts/benchmark-record.mjs";

const { identityOf, dedupe, compatKey, seriesFileStem, seriesLabel, renderTrend } = __testables;

function result(commit, overrides = {}) {
  return {
    schema_version: "1",
    benchmark_suite: "resource-soak",
    mode: "resource_soak",
    dataset_id: "hardening-soak-v1",
    git: { commit, base_commit: null },
    environment: {
      os: "linux",
      architecture: "x86_64",
      cpu: null,
      rust_version: "rustc 1.98.1",
      bun_version: null,
      cargo_profile: "release",
    },
    configuration: {
      config_version: "resource-bench-v1;events=576000;interval_ms=50",
      runtime_profile: "full",
      asset_version: "generated-dynamic-fixture-v1",
      index_version: null,
      retriever_version: null,
      jev_model: null,
      thinking_model: null,
      tts_model: null,
      cost_model_version: null,
      seed: 40,
      stream_duration_ms: 28800000,
    },
    metrics: {
      "resource.throughput_events_per_s": { value: 50000, sample_count: 576000 },
      "resource.telemetry_events_retained_count": { value: 1024, sample_count: 576000 },
    },
    invariants: {
      "resource.retention_bound_violation_count": { value: 0, detail: null },
      "resource.non_plateau_state_count": { value: 0, detail: null },
    },
    ...overrides,
  };
}

describe("benchmark-record identity", () => {
  test("identity keys on suite, mode, commit, and run id", () => {
    expect(identityOf(result("aaa"), "run-1")).toBe("resource-soak|resource_soak|aaa|run-1");
  });

  test("without an explicit run id the recording provenance is used", () => {
    const row = result("aaa", { recording: { run_id: "run-7", recorded_at: "2026-09-30T00:00:00Z" } });
    expect(identityOf(row)).toBe("resource-soak|resource_soak|aaa|run-7");
    expect(identityOf(result("aaa"))).toBe("resource-soak|resource_soak|aaa|-");
  });

  test("the same commit under two run ids has distinct identities", () => {
    expect(identityOf(result("aaa"), "run-1")).not.toBe(identityOf(result("aaa"), "run-2"));
  });
});

describe("benchmark-record dedupe", () => {
  test("same identity keeps the last row (retry is idempotent)", () => {
    const first = result("aaa");
    const second = result("aaa", {
      metrics: {
        ...first.metrics,
        "resource.throughput_events_per_s": { value: 55555, sample_count: 576000 },
      },
    });
    const merged = dedupe([first, second]);
    expect(merged).toHaveLength(1);
    expect(merged[0].metrics["resource.throughput_events_per_s"].value).toBe(55555);
  });

  test("repeated observations of one revision accumulate across runs", () => {
    const merged = dedupe([
      result("aaa", { recording: { run_id: "run-1", recorded_at: "2026-09-30T00:00:00Z" } }),
      result("aaa", { recording: { run_id: "run-2", recorded_at: "2026-10-01T00:00:00Z" } }),
      result("aaa", { recording: { run_id: "run-3", recorded_at: "2026-10-02T00:00:00Z" } }),
    ]);
    expect(merged).toHaveLength(3);
  });

  test("a retry of the same run replaces its earlier observation", () => {
    const original = result("aaa", { recording: { run_id: "run-1", recorded_at: "2026-09-30T00:00:00Z" } });
    const retried = result("aaa", { recording: { run_id: "run-1", recorded_at: "2026-09-30T01:00:00Z" } });
    expect(dedupe([original, retried])).toHaveLength(1);
  });

  test("different commits and modes both survive", () => {
    const merged = dedupe([
      result("aaa"),
      result("bbb"),
      result("ccc", { mode: "full_generative" }),
    ]);
    expect(merged).toHaveLength(3);
  });

  test("merged list is sorted for stable history files", () => {
    const merged = dedupe([
      result("ccc", { recording: { run_id: "run-1", recorded_at: "x" } }),
      result("aaa", { recording: { run_id: "run-1", recorded_at: "x" } }),
      result("bbb", { recording: { run_id: "run-1", recorded_at: "x" } }),
    ]);
    expect(merged.map((row) => row.git.commit)).toEqual(["aaa", "bbb", "ccc"]);
  });
});

describe("benchmark-record compatibility series", () => {
  test("same mode/dataset/config/seed share a series", () => {
    const key = compatKey(result("aaa"));
    expect(key).toBe(compatKey(result("bbb")));
  });

  test("different dataset, config_version, seed, or mode split series", () => {
    const base = compatKey(result("aaa"));
    expect(compatKey(result("bbb", { dataset_id: "hardening-soak-v2" }))).not.toBe(base);
    expect(
      compatKey(
        result("bbb", {
          configuration: {
            ...result("bbb").configuration,
            config_version: "resource-bench-v2;events=576000;interval_ms=50",
          },
        }),
      ),
    ).not.toBe(base);
    expect(
      compatKey(
        result("bbb", {
          configuration: { ...result("bbb").configuration, seed: 41 },
        }),
      ),
    ).not.toBe(base);
    expect(compatKey(result("bbb", { mode: "full_generative" }))).not.toBe(base);
  });

  test("missing dataset_id and null dataset_id stay comparable", () => {
    const absent = result("aaa");
    delete absent.dataset_id;
    const explicitNull = result("bbb", { dataset_id: null });
    expect(compatKey(absent)).toBe(compatKey(explicitNull));
    // Optional boundaries collapse to the empty segment.
    expect(compatKey(absent)).toBe("resource_soak||resource-bench-v1;events=576000;interval_ms=50|40");
  });

  test("series file stems are deterministic, filesystem-safe, and collision-free", () => {
    const key = compatKey(result("aaa"));
    const stem = seriesFileStem(key);
    expect(stem).toBe(seriesFileStem(key));
    expect(stem).toMatch(/^[a-zA-Z0-9._-]+$/);
    // Distinct keys never collapse onto one stem (hash suffix disambiguates
    // slugs truncated to the same prefix).
    expect(seriesFileStem(compatKey(result("bbb", { dataset_id: "other-dataset" })))).not.toBe(stem);
  });

  test("series labels expose the compatibility boundary", () => {
    expect(seriesLabel(compatKey(result("aaa")))).toBe(
      "resource_soak @ hardening-soak-v1 (resource-bench-v1;events=576000;interval_ms=50, seed 40)",
    );
  });
});

describe("benchmark-record trend rendering", () => {
  test("renders the compatibility boundary and per-run rows", () => {
    const rows = [
      result("aaa", {
        recording: { run_id: "run-1", recorded_at: "2026-09-30T00:00:00Z" },
        metrics: {
          "resource.throughput_events_per_s": { value: 51000, sample_count: 576000 },
        },
        invariants: {
          "resource.retention_bound_violation_count": { value: 0, detail: null },
        },
      }),
      result("bbb"),
    ];
    const markdown = renderTrend(seriesLabel(compatKey(result("aaa"))), rows);
    expect(markdown).toContain("# Benchmark trend: resource_soak @ hardening-soak-v1");
    expect(markdown).toContain("(resource-bench-v1;events=576000;interval_ms=50, seed 40)");
    expect(markdown).toContain("## mode: resource_soak");
    expect(markdown).toContain("resource.throughput_events_per_s");
    expect(markdown).toContain("| `aaa` | `run-1` |");
    expect(markdown).toContain("pass");
  });

  test("flags failed invariants in the status column", () => {
    const rows = [
      result("bad", {
        invariants: {
          "resource.non_plateau_state_count": {
            value: 2,
            detail: "telemetry_events(midpoint=1 final=9)",
          },
        },
      }),
    ];
    const markdown = renderTrend(seriesLabel(compatKey(result("bad"))), rows);
    expect(markdown).toContain("FAIL(resource.non_plateau_state_count)");
  });

  test("unrelated metrics do not appear", () => {
    const rows = [
      result("aaa", {
        metrics: { "routing.jev.p95_us": { value: 42, sample_count: 3 } },
      }),
    ];
    const markdown = renderTrend(seriesLabel(compatKey(result("aaa"))), rows);
    expect(markdown).not.toContain("routing.jev.p95_us");
  });
});
