//! End-to-end checks for the `slo-baseline` #58 aggregate-history feed
//! (issue #71): rows shaped like the `benchmark-data` history must pool into
//! target-free latency evidence through the CLI, and must never produce a
//! citable baseline, because the aggregate row carries no denominators.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

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
