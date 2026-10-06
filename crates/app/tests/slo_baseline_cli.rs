//! End-to-end checks for the `slo-baseline` #58 aggregate-history feed
//! (issue #71): rows shaped like the `benchmark-data` history must pool into
//! target-free latency evidence through the CLI, and must never produce a
//! citable baseline, because the aggregate row carries no denominators.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use aivtuber_telemetry::{
    BenchmarkReport, CacheLevel, ComparisonMode, EventObservation, ReproducibilityMetadata,
    RouteClass, SloEvaluationConfig, SloTargets, evaluate,
};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("slo-baseline-cli-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir created");
    dir
}

/// One #58 aggregate history row, shaped like `schemas/benchmark-result`
/// documents the `benchmark-data` branch records.
fn aggregate_row(run_id: &str, commit: &str, p95_ms: u64) -> String {
    serde_json::json!({
        "schema_version": "1",
        "benchmark_suite": "replay-comparison",
        "mode": "deterministic_only",
        "dataset_id": "starter-replay-comparison-v1",
        "git": { "commit": commit },
        "environment": {
            "os": "linux",
            "architecture": "x86_64",
            "rust_version": "rustc 1.98.1 (48a229cea 2026-09-01)",
            "cargo_profile": "debug"
        },
        "configuration": {
            "config_version": "replay-benchmark-v1",
            "asset_version": "starter-v1",
            "seed": 4242,
            "stream_duration_ms": 180000
        },
        "metrics": {
            "cached.first_audio.p50_ms": { "value": p95_ms.saturating_sub(20), "sample_count": 3 },
            "cached.first_audio.p95_ms": { "value": p95_ms, "sample_count": 3 },
            "cached.first_audio.p99_ms": { "value": p95_ms, "sample_count": 3 }
        },
        "invariants": {},
        "recording": { "run_id": run_id, "recorded_at": "2026-09-30T13:06:12Z", "attempt": 1 }
    })
    .to_string()
}

#[test]
fn the_history_flag_pools_latency_evidence_and_never_a_citable_baseline() {
    let dir = temp_dir("pool");
    let jsonl = dir.join("history.jsonl");
    fs::write(
        &jsonl,
        format!(
            "{}\n{}\n",
            aggregate_row("run-a", "aaaa1111", 100),
            aggregate_row("run-b", "bbbb2222", 200)
        ),
    )
    .expect("history written");
    let out = dir.join("proposals.json");
    let markdown = dir.join("proposals.md");

    // History-only invocation: no positional SLO reports at all.
    let output = Command::new(env!("CARGO_BIN_EXE_slo-baseline"))
        .args([
            "--benchmark-history",
            jsonl.to_str().expect("utf8 path"),
            "--out",
            out.to_str().expect("utf8 path"),
            "--markdown",
            markdown.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("slo-baseline runs");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let proposals: serde_json::Value =
        serde_json::from_slice(&fs::read(&out).expect("proposal file")).expect("valid json");
    let latency = &proposals["proposals"]["availability.event_to_first_audio_within_target"];
    assert_eq!(
        latency["latency"]["p95_ms"], 200,
        "worst observed percentile"
    );
    assert_eq!(latency["latency"]["runs"], 2);
    assert_eq!(
        latency["latency"]["max_ms"],
        serde_json::Value::Null,
        "no per-run maximum was recorded, so none is serialized"
    );
    assert!(
        latency["baseline"].is_null(),
        "an aggregate history never publishes a citable baseline"
    );
    assert_eq!(latency["eligible"], serde_json::Value::Null);
    assert!(
        proposals["limitations"]
            .to_string()
            .contains("aggregate #58 history row(s)"),
        "the aggregate contribution is declared: {}",
        proposals["limitations"]
    );

    assert!(!fs::read(&markdown).expect("markdown file").is_empty());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Operational SLO baseline proposal"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_full_report_and_the_aggregate_row_of_the_same_run_count_once() {
    // The #224 migration overlap: run-a exists as a durable full report *and*
    // as a row of the old aggregate history; run-b is an aggregate-only run
    // whose percentile evidence must keep pooling alongside it.
    let dir = temp_dir("mixed");
    let jsonl = dir.join("history.jsonl");
    fs::write(
        &jsonl,
        format!(
            "{}\n{}\n",
            aggregate_row("run-a", "aaaa1111", 100),
            aggregate_row("run-b", "bbbb2222", 200)
        ),
    )
    .expect("history written");
    let full = dir.join("run-a.json");
    fs::write(&full, overlapping_full_report("run-a", "aaaa1111")).expect("report written");
    let out = dir.join("proposals.json");

    let output = Command::new(env!("CARGO_BIN_EXE_slo-baseline"))
        .args([
            full.to_str().expect("utf8 path"),
            "--benchmark-history",
            jsonl.to_str().expect("utf8 path"),
            "--out",
            out.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("slo-baseline runs");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let proposals: serde_json::Value =
        serde_json::from_slice(&fs::read(&out).expect("proposal file")).expect("valid json");

    // The overlapped run counts once, in either artifact's company.
    let runs = proposals["contributing_runs"]
        .as_array()
        .expect("run manifest");
    let run_ids: Vec<&str> = runs
        .iter()
        .filter_map(|run| run["run_id"].as_str())
        .collect();
    assert_eq!(run_ids.len(), 2, "one run per identity: {run_ids:?}");
    assert_eq!(
        run_ids.iter().filter(|id| **id == "run-a").count(),
        1,
        "run-a arrived as two artifacts but is one run: {run_ids:?}"
    );

    // The old aggregate-only run still contributes its percentile evidence,
    // and the full report's per-run maximum survives the collapse.
    let latency =
        &proposals["proposals"]["availability.event_to_first_audio_within_target"]["latency"];
    assert_eq!(latency["runs"], 2);
    assert_eq!(latency["samples"], 6);
    assert_eq!(latency["p95_ms"], 200, "worst observed percentile");
    assert_eq!(
        latency["max_ms"], 100,
        "the full report's per-run maximum survives the collapse"
    );
    assert!(
        proposals["limitations"]
            .to_string()
            .contains("aggregate-history shell"),
        "the collapse is declared: {}",
        proposals["limitations"]
    );

    let _ = fs::remove_dir_all(&dir);
}

/// The full `slo-report` artifact for the run the aggregate history's first
/// row also records, with session percentiles (80 / 100 / 100 over 3 samples)
/// matching what `aggregate_row(_, _, 100)` says about that same run.
fn overlapping_full_report(run_id: &str, commit: &str) -> String {
    let metadata = ReproducibilityMetadata {
        dataset_id: "starter-replay-comparison-v1".to_owned(),
        git_commit: commit.to_owned(),
        rust_toolchain: "rustc 1.98.1 (48a229cea 2026-09-01)".to_owned(),
        bun_toolchain: None,
        config_version: "replay-benchmark-v1".to_owned(),
        asset_version: "starter-v1".to_owned(),
        index_version: None,
        jev_model: None,
        thinking_model: None,
        tts_model: None,
        seed: 4242,
        stream_duration_ms: Some(180_000),
    };
    let events = [60_u64, 80, 100]
        .iter()
        .enumerate()
        .map(|(index, latency)| {
            let mut event = EventObservation::new(
                format!("evt-{index}"),
                ComparisonMode::DeterministicOnly,
                RouteClass::Deterministic,
            );
            event.routing_latency_us = 10;
            event.cache_lookup = true;
            event.cache_hit = true;
            event.cache_level = Some(CacheLevel::Memory);
            event.stream_offset_ms = Some((index as u64 + 1) * 1_000);
            event.event_to_first_audio_ms = Some(*latency);
            event
        })
        .collect();
    let report = BenchmarkReport::from_events(metadata, ComparisonMode::DeterministicOnly, events)
        .expect("benchmark report");
    let slo = evaluate(
        &report,
        &SloTargets::default(),
        SloEvaluationConfig {
            run_id: Some(run_id.to_owned()),
            ..SloEvaluationConfig::default()
        },
    )
    .expect("SLO report");
    serde_json::to_string_pretty(&slo).expect("serialized report")
}

#[test]
fn a_raw_json_integer_beyond_exact_representation_is_refused() {
    // 2^53+1 rounds to 2^53 while the row is parsed, before validation can
    // see it: the CLI must fail closed instead of emitting 9007199254740992
    // as a measured latency (re-review of PR #225).
    let dir = temp_dir("jsonl-precision");
    let jsonl = dir.join("history.jsonl");
    let mut row: serde_json::Value =
        serde_json::from_str(&aggregate_row("run-a", "aaaa1111", 100)).expect("json");
    row["metrics"]["cached.first_audio.p95_ms"]["value"] =
        serde_json::json!(9_007_199_254_740_993_u64);
    fs::write(&jsonl, format!("{row}\n")).expect("history written");
    let out = dir.join("proposals.json");

    let output = Command::new(env!("CARGO_BIN_EXE_slo-baseline"))
        .args([
            "--benchmark-history",
            jsonl.to_str().expect("utf8 path"),
            "--out",
            out.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("slo-baseline runs");
    assert!(
        !output.status.success(),
        "a JSON integer that does not survive parsing must fail closed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("2^53"), "{stderr}");
    assert!(
        !out.exists(),
        "no proposal may be emitted from a measurement that was rounded"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rows_from_two_benchmark_suites_are_refused() {
    // Two otherwise-identical rows differing only in benchmark_suite must not
    // reach one pool as one calibration (review of PR #225).
    let dir = temp_dir("suites");
    let jsonl = dir.join("mixed-suites.jsonl");
    let mut other: serde_json::Value =
        serde_json::from_str(&aggregate_row("run-b", "bbbb2222", 200)).expect("json");
    other["benchmark_suite"] = serde_json::json!("resource-soak");
    fs::write(
        &jsonl,
        format!("{}\n{other}\n", aggregate_row("run-a", "aaaa1111", 100)),
    )
    .expect("history written");

    let output = Command::new(env!("CARGO_BIN_EXE_slo-baseline"))
        .args(["--benchmark-history", jsonl.to_str().expect("utf8 path")])
        .output()
        .expect("slo-baseline runs");
    assert!(!output.status.success(), "a foreign suite must fail closed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("benchmark_suite"), "{stderr}");
    assert!(stderr.contains("replay-comparison"), "{stderr}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn running_without_any_input_is_refused() {
    let output = Command::new(env!("CARGO_BIN_EXE_slo-baseline"))
        .output()
        .expect("slo-baseline runs");
    assert!(!output.status.success(), "no inputs must fail closed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("at least one SLO report or --benchmark-history"),
        "{stderr}"
    );
}

#[test]
fn an_empty_history_file_is_refused() {
    let dir = temp_dir("empty");
    let jsonl = dir.join("empty.jsonl");
    fs::write(&jsonl, "\n  \n").expect("empty history written");
    let output = Command::new(env!("CARGO_BIN_EXE_slo-baseline"))
        .args(["--benchmark-history", jsonl.to_str().expect("utf8 path")])
        .output()
        .expect("slo-baseline runs");
    assert!(!output.status.success(), "no rows must fail closed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("contains no benchmark rows"), "{stderr}");

    let _ = fs::remove_dir_all(&dir);
}
