import { describe, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { __testables as benchmark } from "../scripts/benchmark-record.mjs";
import { reportPathOf, requireRecordable, runFileStem, seriesKeyOf } from "../scripts/slo-record.mjs";

const script = join(dirname(fileURLToPath(import.meta.url)), "..", "scripts", "slo-record.mjs");

/// A minimal `slo-report` document: exactly the shape the recorder has to be
/// able to route (schema/catalog versions, an identity block with a run id and
/// the four #58 series fields).
function sloReport(source = {}, rest = {}) {
  return {
    schema_version: "1",
    catalog_version: "slo-catalog-v1",
    counts_toward_active_slo: true,
    source: {
      dataset_id: "replay-comparison-v1",
      git_commit: "deadbeef",
      mode: "full_generative",
      config_version: "replay-comparison-v1",
      seed: 7,
      stream_duration_ms: 28_800_000,
      provenance: "replay_fixture",
      plane: "active",
      run_id: "run-1001",
      ...source,
    },
    indicators: [],
    latency_calibration: [],
    ...rest,
  };
}

describe("slo-record series identity", () => {
  test("the series key is the #58 compatibility boundary, read from the report's own source", () => {
    const key = seriesKeyOf(sloReport());
    expect(key).toBe("full_generative|replay-comparison-v1|replay-comparison-v1|7");
    // And it is the *same* key the benchmark history derives, so the two
    // histories cannot disagree about what one series is.
    expect(key).toBe(
      benchmark.compatKey({
        mode: "full_generative",
        dataset_id: "replay-comparison-v1",
        configuration: { config_version: "replay-comparison-v1", seed: 7 },
      }),
    );
  });

  test("each boundary field splits the series", () => {
    const base = seriesKeyOf(sloReport());
    expect(seriesKeyOf(sloReport({ mode: "deterministic_only" }))).not.toBe(base);
    expect(seriesKeyOf(sloReport({ dataset_id: "other" }))).not.toBe(base);
    expect(seriesKeyOf(sloReport({ config_version: "replay-comparison-v2" }))).not.toBe(base);
    expect(seriesKeyOf(sloReport({ seed: 8 }))).not.toBe(base);
  });

  test("a revision change stays inside the series (a baseline may pool two commits)", () => {
    expect(seriesKeyOf(sloReport({ git_commit: "cafebabe" }))).toBe(seriesKeyOf(sloReport()));
  });
});

describe("slo-record paths", () => {
  test("one path per series and run identity", () => {
    const path = reportPathOf(sloReport());
    expect(path).toBe(reportPathOf(sloReport({ git_commit: "cafebabe" })));
    expect(path.startsWith("reports/")).toBe(true);
    expect(path.endsWith(".json")).toBe(true);
    // A different run id in the same series is a different artifact.
    expect(reportPathOf(sloReport({ run_id: "run-1002" }))).not.toBe(path);
  });

  test("run ids that are not filename-safe still get distinct, stable stems", () => {
    const stem = runFileStem("run-1001/attempt 2");
    expect(stem).toBe(runFileStem("run-1001/attempt 2"));
    expect(stem).toMatch(/^[a-zA-Z0-9._-]+$/);
    expect(runFileStem("run-1001")).not.toBe(stem);
    // Sanitizing two distinct ids to one slug must not collapse them.
    expect(runFileStem("run/a")).not.toBe(runFileStem("run-a"));
  });
});

describe("slo-record refuses anything that is not calibration history", () => {
  test("a benchmark result is not an SLO report", () => {
    const benchmarkResult = {
      schema_version: "1",
      benchmark_suite: "replay-comparison",
      mode: "full_generative",
      git: { commit: "deadbeef" },
      recording: { run_id: "run-1001" },
    };
    expect(() => requireRecordable(benchmarkResult, "04-full-generative-result.json")).toThrow(
      /catalog_version/,
    );
  });

  test("a report without a run identity cannot join a history", () => {
    const report = sloReport({ run_id: undefined });
    delete report.source.run_id;
    expect(() => requireRecordable(report, "anonymous.json")).toThrow(/no run identity/);
    expect(() => requireRecordable(sloReport({ run_id: "   " }), "blank.json")).toThrow(
      /no run identity/,
    );
  });

  test("a report that names no compatibility series is refused", () => {
    expect(() => requireRecordable(sloReport({ dataset_id: "" }), "x.json")).toThrow(
      /source\.dataset_id/,
    );
    expect(() => requireRecordable(sloReport({ mode: "" }), "x.json")).toThrow(/source\.mode/);
  });

  test("non-object documents are refused", () => {
    expect(() => requireRecordable(null, "x.json")).toThrow(/not an SLO report/);
    expect(() => requireRecordable([], "x.json")).toThrow(/not an SLO report/);
  });
});

/// The full record -> retrieve path, against a real git remote. GitHub itself
/// is the caller in production; this exercises the same mechanics (an orphan
/// first push, an idempotent re-record, accumulation across runs) offline.
describe("slo-record persists a retrievable history", () => {
  function git(args, cwd) {
    return execFileSync("git", args, { cwd, encoding: "utf8" }).trim();
  }

  function record(root, files) {
    return execFileSync(process.execPath, [script, "--root", root, "--files", ...files], {
      encoding: "utf8",
    });
  }

  test("records, replaces a re-run idempotently, and accumulates distinct runs", () => {
    const root = mkdtempSync(join(tmpdir(), "slo-record-"));
    try {
      git(["init", "--quiet", "--initial-branch=main"], root);
      git(["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "--quiet", "--allow-empty", "-m", "init"], root);
      const origin = join(root, "origin.git");
      git(["init", "--bare", "--quiet", origin], root);
      git(["remote", "add", "origin", origin], root);

      const inputDir = join(root, "in");
      mkdirSync(inputDir, { recursive: true });
      const first = join(inputDir, "run-1001.json");
      writeFileSync(first, `${JSON.stringify(sloReport(), null, 2)}\n`);

      record(root, [first]);
      git(["push", "--quiet", "origin", "slo-data"], root);

      const path = reportPathOf(sloReport());
      const recorded = JSON.parse(git(["show", `slo-data:${path}`], origin));
      expect(recorded).toEqual(sloReport());
      expect(git(["show", "slo-data:README.md"], origin)).toContain("slo-baseline");

      // A re-run of one workflow run replaces its artifact: the branch never
      // holds two attempts of one run, so `slo-baseline` never has to choose.
      const retry = join(inputDir, "run-1001-retry.json");
      writeFileSync(
        retry,
        `${JSON.stringify(sloReport({ git_commit: "cafebabe" }), null, 2)}\n`,
      );
      record(root, [retry]);
      git(["push", "--quiet", "origin", "slo-data"], root);

      expect(git(["ls-tree", "-r", "--name-only", "slo-data", "reports"], origin)).toBe(path);
      expect(JSON.parse(git(["show", `slo-data:${path}`], origin)).source.git_commit).toBe(
        "cafebabe",
      );

      // A distinct run of the same revision accumulates: repeated measurements
      // are the raw material a baseline is calibrated from.
      const second = join(inputDir, "run-1002.json");
      writeFileSync(second, `${JSON.stringify(sloReport({ run_id: "run-1002" }), null, 2)}\n`);
      record(root, [second]);
      git(["push", "--quiet", "origin", "slo-data"], root);

      const files = git(["ls-tree", "-r", "--name-only", "slo-data", "reports"], origin).split("\n");
      expect(files).toHaveLength(2);
      expect(files).toContain(reportPathOf(sloReport({ run_id: "run-1002" })));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("an unrecordable input fails closed and writes nothing", () => {
    const root = mkdtempSync(join(tmpdir(), "slo-record-refuse-"));
    try {
      git(["init", "--quiet", "--initial-branch=main"], root);
      git(["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "--quiet", "--allow-empty", "-m", "init"], root);
      const origin = join(root, "origin.git");
      git(["init", "--bare", "--quiet", origin], root);
      git(["remote", "add", "origin", origin], root);

      const input = join(root, "anonymous.json");
      writeFileSync(input, JSON.stringify(sloReport({ run_id: undefined })));

      let status = 0;
      let stderr = "";
      try {
        record(root, [input]);
      } catch (error) {
        status = error.status;
        stderr = String(error.stderr);
      }
      expect(status).not.toBe(0);
      expect(stderr).toContain("no run identity");
      // No branch was created, so nothing was persisted.
      expect(git(["ls-remote", "--heads", "origin"], root)).toBe("");
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
