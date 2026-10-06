import { describe, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { __testables as benchmark } from "../scripts/benchmark-record.mjs";
import {
  benchmarkPathOf,
  contractSegmentsOf,
  reportPathOf,
  requirePairable,
  requireRecordable,
  runFileStem,
  seriesKeyOf,
  sourceDigestOf,
  versionFileStem,
} from "../scripts/slo-record.mjs";

const script = join(dirname(fileURLToPath(import.meta.url)), "..", "scripts", "slo-record.mjs");

/// A minimal `replay-benchmark` mode report: the raw per-event source a
/// `slo-report` is evaluated from, and the only durable artifact the latency
/// probe step can be re-run against.
function benchmarkReport(overrides = {}) {
  const metadata = {
    dataset_id: "replay-comparison-v1",
    git_commit: "deadbeef",
    rust_toolchain: "rustc 1.98.1",
    bun_toolchain: null,
    config_version: "replay-comparison-v1",
    asset_version: "starter-v1",
    index_version: null,
    jev_model: null,
    thinking_model: null,
    tts_model: null,
    cost_model_version: null,
    seed: 7,
    stream_duration_ms: 28_800_000,
    ...(overrides.metadata ?? {}),
  };
  return {
    metadata,
    mode: "full_generative",
    events: [{ event_id: "evt-1", event_to_first_audio_ms: 120 }],
    summary: {},
    ...Object.fromEntries(Object.entries(overrides).filter(([key]) => key !== "metadata")),
  };
}

/// A minimal `slo-report` document: exactly the shape the recorder has to be
/// able to route (schema/catalog versions, an identity block with a run id and
/// the four #58 series fields).
function sloReport(source = {}, rest = {}, pairedBenchmark = benchmarkReport()) {
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
      // The digest of the replay report this fixture is paired with —
      // exactly what `evaluate` computes and the recorder verifies.
      source_digest: sourceDigestOf(pairedBenchmark),
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
  test("one path per contract, series, and run identity", () => {
    const path = reportPathOf(sloReport());
    expect(path).toBe(reportPathOf(sloReport({ git_commit: "cafebabe" })));
    expect(path.startsWith("reports/1/slo-catalog-v1/")).toBe(true);
    expect(path.endsWith(".json")).toBe(true);
    // A different run id in the same partition is a different artifact.
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

  test("version segments stay readable, and never become a path traversal", () => {
    expect(versionFileStem("1")).toBe("1");
    expect(versionFileStem("slo-catalog-v1")).toBe("slo-catalog-v1");
    // A document-declared version must not escape the history worktree.
    for (const hostile of ["..", ".", "../../etc", "a/b", "x".repeat(200)]) {
      const stem = versionFileStem(hostile);
      expect(stem).not.toBe("..");
      expect(stem).not.toBe(".");
      expect(stem).not.toContain("/");
      expect(stem).not.toContain("\\");
    }
    // Distinct versions that sanitize to one slug must not collapse.
    expect(versionFileStem("a/b")).not.toBe(versionFileStem("a-b"));
  });
});

describe("slo-record partitions the history by SLO contract", () => {
  test("the contract segments are the two versions slo-baseline refuses to pool", () => {
    expect(contractSegmentsOf(sloReport())).toEqual({
      schema: "1",
      catalog: "slo-catalog-v1",
    });
  });

  // A catalog or schema bump is a normal, contractually-required evolution.
  // If it did not move the document to a new partition, the documented
  // per-directory wildcard retrieval would become permanently unpoolable.
  test("same #58 series, different catalog version, lands in distinct partitions", () => {
    const v1 = sloReport();
    const v2 = sloReport({}, { catalog_version: "slo-catalog-v2" });
    const pathV1 = reportPathOf(v1);
    const pathV2 = reportPathOf(v2);

    expect(pathV1).not.toBe(pathV2);
    // The #58 workload series is identical: it is the SLO contract that
    // splits, not the workload boundary.
    expect(seriesKeyOf(v1)).toBe(seriesKeyOf(v2));
    expect(pathV1.split("/").at(-2)).toBe(pathV2.split("/").at(-2));
    expect(pathV1).toContain("/1/slo-catalog-v1/");
    expect(pathV2).toContain("/1/slo-catalog-v2/");
  });

  test("same #58 series, different report schema, lands in distinct partitions", () => {
    const v1 = sloReport();
    const v2 = sloReport({}, { schema_version: "2" });
    expect(seriesKeyOf(v1)).toBe(seriesKeyOf(v2));
    expect(reportPathOf(v1)).not.toBe(reportPathOf(v2));
    expect(reportPathOf(v2)).toContain("/2/slo-catalog-v1/");
  });
});

describe("slo-record pairs the replay report a latency probe needs", () => {
  test("the raw report lives in a parallel tree, never in the pooled one", () => {
    const report = sloReport();
    const slo = reportPathOf(report);
    const raw = benchmarkPathOf(report);
    expect(raw.startsWith("benchmark-reports/")).toBe(true);
    expect(slo.startsWith("reports/")).toBe(true);
    // The documented retrieval is a wildcard over the reports/ leaf; a replay
    // report there would be refused by `slo-baseline` as not an SLO report.
    expect(raw).not.toContain("/reports/");
    // Same contract, series, and run identity — one path segment apart.
    expect(raw.replace("benchmark-reports/", "reports/")).toBe(slo);
    expect(raw.split("/").slice(1, -1)).toEqual(slo.split("/").slice(1, -1));
    expect(benchmarkPathOf(sloReport({ git_commit: "cafebabe" }))).toBe(raw);
    expect(benchmarkPathOf(sloReport({ run_id: "run-1002" }))).not.toBe(raw);
  });

  test("a pair from a different workload series is refused", () => {
    const report = sloReport();
    expect(() => requirePairable(benchmarkReport(), report, "raw.json")).not.toThrow();
    for (const [field, override] of [
      ["dataset_id", { metadata: { dataset_id: "other" } }],
      ["config_version", { metadata: { config_version: "other" } }],
      ["seed", { metadata: { seed: 9 } }],
      ["mode", { mode: "deterministic_only" }],
    ]) {
      expect(() => requirePairable(benchmarkReport(override), report, "raw.json")).toThrow(
        /compatibility series/,
      );
    }
  });

  test("a document that is not a replay report is refused", () => {
    const report = sloReport();
    expect(() => requirePairable(sloReport(), report, "raw.json")).toThrow(/metadata/);
    expect(() => requirePairable(null, report, "raw.json")).toThrow(/not a replay/);
  });

  // A series may span several revisions — that is how a baseline pools two
  // commits — but one stored pair may not: the probe re-measures the stored
  // run from *this* raw report, so a pair from another revision would let it
  // produce a ratio the stored run never measured.
  test("a pair from a different revision is refused", () => {
    expect(() =>
      requirePairable(
        benchmarkReport({ metadata: { git_commit: "cafebabe" } }),
        sloReport(),
        "raw.json",
      ),
    ).toThrow(/git_commit/);
  });

  test("a pair with a different represented duration is refused", () => {
    expect(() =>
      requirePairable(
        benchmarkReport({ metadata: { stream_duration_ms: 14_400_000 } }),
        sloReport(),
        "raw.json",
      ),
    ).toThrow(/stream_duration_ms/);
  });

  // The identity fields cannot distinguish two different observations of one
  // revision, series, and duration; the digest is what can.
  test("a pair whose source digest does not match is refused", () => {
    expect(() =>
      requirePairable(
        benchmarkReport({ events: [{ event_id: "evt-1", event_to_first_audio_ms: 121 }] }),
        sloReport(),
        "raw.json",
      ),
    ).toThrow(/source digest/);
  });

  test("an SLO report without a source digest is refused", () => {
    const report = sloReport();
    delete report.source.source_digest;
    expect(() => requirePairable(benchmarkReport(), report, "raw.json")).toThrow(
      /no `source\.source_digest`/,
    );
  });

  // Pinned in `crates/telemetry/src/operational_slo.rs` as well: the Rust
  // evaluation digests this exact document while producing the SLO report,
  // the recorder recomputes the digest from the stored file, and the two
  // implementations must agree byte-for-byte or every pair is refused.
  test("the JavaScript digest matches the Rust evaluation's digest on one document", () => {
    expect(
      sourceDigestOf({ a: 1, b: 1.0, c: 1e-7, d: [0.5, -0.0], e: "x", f: null, g: true, h: 1e21 }),
    ).toBe("67ab96b3cba323e2e72493aed8212bce68f7c432660d27492f338ab4cc477802");
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

  function record(root, files, extra = []) {
    return execFileSync(process.execPath, [script, "--root", root, "--files", ...files, ...extra], {
      encoding: "utf8",
    });
  }

  /// A throwaway repo plus a bare `origin`, i.e. the shape the trusted
  /// push-to-main job sees.
  function scratchRepo(label) {
    const root = mkdtempSync(join(tmpdir(), `slo-record-${label}-`));
    git(["init", "--quiet", "--initial-branch=main"], root);
    git(
      ["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "--quiet", "--allow-empty", "-m", "init"],
      root,
    );
    const origin = join(root, "origin.git");
    git(["init", "--bare", "--quiet", origin], root);
    git(["remote", "add", "origin", origin], root);
    return { root, origin };
  }

  /// Write a report document into the scratch repo's input directory.
  function inputFile(root, name, report) {
    const dir = join(root, "in");
    mkdirSync(dir, { recursive: true });
    const path = join(dir, name);
    writeFileSync(path, `${JSON.stringify(report, null, 2)}\n`);
    return path;
  }

  test("records, replaces a re-run idempotently, and accumulates distinct runs", () => {
    const { root, origin } = scratchRepo("history");
    try {
      const inputDir = join(root, "in");
      const benchmarks = join(root, "bench");
      mkdirSync(benchmarks, { recursive: true });

      const first = inputFile(root, "run-1001.json", sloReport());
      writeFileSync(join(benchmarks, "run-1001.json"), `${JSON.stringify(benchmarkReport(), null, 2)}\n`);
      record(root, [first], ["--benchmarks", benchmarks]);
      git(["push", "--quiet", "origin", "slo-data"], root);

      const path = reportPathOf(sloReport());
      const recorded = JSON.parse(git(["show", `slo-data:${path}`], origin));
      expect(recorded).toEqual(sloReport());
      expect(git(["show", "slo-data:README.md"], origin)).toContain("slo-baseline");

      // A re-run of one workflow run replaces its artifact: the branch never
      // holds two attempts of one run, so `slo-baseline` never has to choose.
      // The retry is a different revision, so its pair moves with it.
      const retryRaw = benchmarkReport({ metadata: { git_commit: "cafebabe" } });
      const retry = join(inputDir, "run-1001-retry.json");
      writeFileSync(
        retry,
        `${JSON.stringify(sloReport({ git_commit: "cafebabe" }, {}, retryRaw), null, 2)}\n`,
      );
      writeFileSync(join(benchmarks, "run-1001-retry.json"), `${JSON.stringify(retryRaw, null, 2)}\n`);
      record(root, [retry], ["--benchmarks", benchmarks]);
      git(["push", "--quiet", "origin", "slo-data"], root);

      expect(git(["ls-tree", "-r", "--name-only", "slo-data", "reports"], origin)).toBe(path);
      expect(JSON.parse(git(["show", `slo-data:${path}`], origin)).source.git_commit).toBe(
        "cafebabe",
      );

      // A distinct run of the same revision accumulates: repeated measurements
      // are the raw material a baseline is calibrated from.
      const second = join(inputDir, "run-1002.json");
      writeFileSync(second, `${JSON.stringify(sloReport({ run_id: "run-1002" }), null, 2)}\n`);
      writeFileSync(join(benchmarks, "run-1002.json"), `${JSON.stringify(benchmarkReport(), null, 2)}\n`);
      record(root, [second], ["--benchmarks", benchmarks]);
      git(["push", "--quiet", "origin", "slo-data"], root);

      const files = git(["ls-tree", "-r", "--name-only", "slo-data", "reports"], origin).split("\n");
      expect(files).toHaveLength(2);
      expect(files).toContain(reportPathOf(sloReport({ run_id: "run-1002" })));
      // Every recorded run kept its probe source beside it.
      const rawFiles = git(["ls-tree", "-r", "--name-only", "slo-data", "benchmark-reports"], origin)
        .split("\n");
      expect(rawFiles).toHaveLength(2);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  // The reported defect: latency calibration could not be finished from the
  // durable history, because the probe step needs the raw per-event report that
  // was never persisted.
  test("a paired run persists both the SLO report and its probe source", () => {
    const { root, origin } = scratchRepo("probe");
    try {
      const reports = join(root, "bench");
      mkdirSync(reports, { recursive: true });
      const slo = inputFile(root, "04-full-generative.json", sloReport());
      writeFileSync(join(reports, "04-full-generative.json"), `${JSON.stringify(benchmarkReport(), null, 2)}\n`);

      record(root, [slo], ["--benchmarks", reports]);
      git(["push", "--quiet", "origin", "slo-data"], root);

      // The pooled tree holds only SLO reports...
      const pooled = git(["ls-tree", "-r", "--name-only", "slo-data", "reports"], origin);
      expect(pooled).toBe(reportPathOf(sloReport()));
      // ...and the probe source is retrievable beside it, under the same
      // identity, so the stored run stays probeable after the job ends.
      const raw = git(["ls-tree", "-r", "--name-only", "slo-data", "benchmark-reports"], origin);
      expect(raw).toBe(benchmarkPathOf(sloReport()));
      expect(JSON.parse(git(["show", `slo-data:${raw}`], origin)).events).toHaveLength(1);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("a run whose replay report is missing is refused, not half-recorded", () => {
    const { root } = scratchRepo("nopair");
    try {
      const reports = join(root, "bench");
      mkdirSync(reports, { recursive: true });
      const slo = inputFile(root, "04-full-generative.json", sloReport());
      // No `04-full-generative.json` in the benchmark directory: storing the
      // SLO report alone would leave a run that can never be probed.
      expect(() => record(root, [slo], ["--benchmarks", reports])).toThrow();
      expect(git(["ls-remote", "--heads", "origin"], root)).toBe("");
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  // The reported defect: a catalog bump is a normal, contractually-required
  // evolution. Sharing one series directory with the old contract made the
  // documented per-directory wildcard retrieval permanently unpoolable.
  test("a catalog bump partitions the history instead of poisoning the series", () => {
    const { root, origin } = scratchRepo("bump");
    try {
      const benchmarks = join(root, "bench");
      mkdirSync(benchmarks, { recursive: true });
      for (const run of ["run-1001", "run-1002"]) {
        writeFileSync(join(benchmarks, `${run}.json`), `${JSON.stringify(benchmarkReport(), null, 2)}\n`);
      }
      const old = inputFile(root, "run-1001.json", sloReport());
      const bumped = inputFile(
        root,
        "run-1002.json",
        sloReport({ run_id: "run-1002" }, { catalog_version: "slo-catalog-v2" }),
      );
      record(root, [old, bumped], ["--benchmarks", benchmarks]);
      git(["push", "--quiet", "origin", "slo-data"], root);

      const files = git(["ls-tree", "-r", "--name-only", "slo-data", "reports"], origin)
        .split("\n")
        .filter(Boolean);
      expect(files).toHaveLength(2);
      expect(files).toContain(reportPathOf(sloReport()));
      expect(files).toContain(reportPathOf(sloReport({ run_id: "run-1002" }, { catalog_version: "slo-catalog-v2" })));

      // Each leaf directory holds exactly one contract, which is the property
      // `slo-baseline` needs: the wildcard over one of them pools cleanly, and
      // the old history stays retrievable under its own partition.
      const leafOf = (path) => path.split("/").slice(0, -1).join("/");
      const leaves = new Set(files.map(leafOf));
      expect(leaves.size).toBe(2);
      for (const leaf of leaves) {
        expect(files.filter((file) => leafOf(file) === leaf)).toHaveLength(1);
      }
      // Same #58 workload series on both sides; only the contract moved them.
      expect(leafOf(reportPathOf(sloReport())).split("/").at(-1)).toBe(
        leafOf(reportPathOf(sloReport({}, { catalog_version: "slo-catalog-v2" }))).split("/").at(-1),
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("an unrecordable input fails closed and writes nothing", () => {
    const { root } = scratchRepo("refuse");
    try {
      const benchmarks = join(root, "bench");
      mkdirSync(benchmarks, { recursive: true });
      const input = inputFile(root, "anonymous.json", sloReport({ run_id: undefined }));

      let status = 0;
      let stderr = "";
      try {
        record(root, [input], ["--benchmarks", benchmarks]);
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

  // The reviewer's repro: the recorder used to define the durable history
  // could be invoked without the replay sources it documents, creating the
  // unprobeable half-record the pairing rules were supposed to make
  // impossible. The source is now a hard requirement of every write.
  test("recording without a replay source is refused and writes nothing", () => {
    const { root } = scratchRepo("nobench");
    try {
      const slo = inputFile(root, "run-1001.json", sloReport());
      expect(() => record(root, [slo])).toThrow(/--benchmarks/);
      expect(git(["ls-remote", "--heads", "origin"], root)).toBe("");
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
