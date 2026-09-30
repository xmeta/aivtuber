import { describe, expect, test } from "bun:test";

import { __testables } from "../scripts/benchmark-record.mjs";

const { identityOf, dedupe, renderTrend } = __testables;

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
  test("identity keys on suite, mode, and commit", () => {
    expect(identityOf(result("aaa"))).toBe("resource-soak|resource_soak|aaa");
  });
});

describe("benchmark-record dedupe", () => {
  test("same identity keeps the last row (re-record is idempotent)", () => {
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

  test("different commits and modes both survive", () => {
    const merged = dedupe([
      result("aaa"),
      result("bbb"),
      result("ccc", { mode: "full_generative" }),
    ]);
    expect(merged).toHaveLength(3);
  });

  test("merged list is sorted for stable history files", () => {
    const merged = dedupe([result("ccc"), result("aaa"), result("bbb")]);
    expect(merged.map((row) => row.git.commit)).toEqual(["aaa", "bbb", "ccc"]);
  });
});

describe("benchmark-record trend rendering", () => {
  test("renders known metrics and invariant status", () => {
    const rows = [
      result("aaa", {
        metrics: {
          "resource.throughput_events_per_s": { value: 51000, sample_count: 576000 },
        },
        invariants: {
          "resource.retention_bound_violation_count": { value: 0, detail: null },
        },
      }),
      result("bbb"),
    ];
    const markdown = renderTrend("resource-soak", rows);
    expect(markdown).toContain("# Benchmark trend: resource-soak");
    expect(markdown).toContain("## mode: resource_soak");
    expect(markdown).toContain("resource.throughput_events_per_s");
    expect(markdown).toContain("| `aaa` |");
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
    const markdown = renderTrend("resource-soak", rows);
    expect(markdown).toContain("FAIL(resource.non_plateau_state_count)");
  });

  test("unrelated metrics do not appear", () => {
    const rows = [
      result("aaa", {
        metrics: { "routing.jev.p95_us": { value: 42, sample_count: 3 } },
      }),
    ];
    const markdown = renderTrend("resource-soak", rows);
    expect(markdown).not.toContain("routing.jev.p95_us");
  });
});
