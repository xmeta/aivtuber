//! High-volume logical-time resource benchmark CLI — issue #58 Phase D.
//!
//! Runs the deterministic soak workload (default 576k logical events, about
//! eight stream hours at 20 events/second), reports retained-state plateau
//! behavior, and emits a `schemas/benchmark-result.schema.json`-conforming
//! result so the #58 comparator and history storage can gate and trend it.
//!
//! Exit status is non-zero when a configured retained-state bound is violated
//! or retained state does not plateau after warm-up.
//!
//! Usage:
//! ```text
//! resource-bench [output-dir] [logical-events]
//! ```

#![forbid(unsafe_code)]

use aivtuber_hardening::{
    RESOURCE_BENCH_CONFIG_VERSION, RESOURCE_BENCH_DATASET, RESOURCE_BENCH_MODE,
    RESOURCE_BENCH_SUITE, ResourceBenchTiming, SoakConfig, resource_bench_result, run_core_soak,
};
use aivtuber_telemetry::{BenchmarkEnvironment, ReproducibilityMetadata};
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

/// Default workload: 576,000 logical events at 50 ms intervals = eight
/// logical stream hours at 20 events/second (matches `SoakConfig::default`).
const DEFAULT_LOGICAL_EVENTS: u64 = 576_000;

fn main() {
    if let Err(error) = run() {
        eprintln!("resource-bench: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let output_dir = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/resource-bench"));
    let logical_events = match args.next() {
        Some(value) => {
            let text = value
                .to_str()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 argument"))?;
            text.parse::<u64>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("logical event count must be an unsigned integer: {error}"),
                )
            })?
        }
        None => DEFAULT_LOGICAL_EVENTS,
    };
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: resource-bench [output-dir] [logical-events]",
        )
        .into());
    }

    let config = SoakConfig {
        logical_events,
        ..SoakConfig::default()
    };
    let metadata = ReproducibilityMetadata {
        dataset_id: RESOURCE_BENCH_DATASET.to_owned(),
        git_commit: git_revision(),
        rust_toolchain: command_output("rustc", &["--version"])
            .unwrap_or_else(|| "unknown-rustc".to_owned()),
        bun_toolchain: command_output("bun", &["--version"]),
        config_version: format!(
            "{RESOURCE_BENCH_CONFIG_VERSION};events={};interval_ms={}",
            config.logical_events, config.event_interval_ms
        ),
        asset_version: "generated-dynamic-fixture-v1".to_owned(),
        index_version: None,
        jev_model: None,
        thinking_model: None,
        tts_model: None,
        seed: 40,
        stream_duration_ms: Some(config.logical_duration_ms()),
    };

    let environment = BenchmarkEnvironment {
        os: std::env::consts::OS.to_owned(),
        architecture: std::env::consts::ARCH.to_owned(),
        cpu: None,
        rust_version: metadata.rust_toolchain.clone(),
        bun_version: metadata.bun_toolchain.clone(),
        cargo_profile: if cfg!(debug_assertions) {
            "debug".to_owned()
        } else {
            "release".to_owned()
        },
    };
    let started = Instant::now();
    let report = run_core_soak(config, metadata)?;
    let timing = ResourceBenchTiming {
        wall_clock_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        peak_rss_kib: ResourceBenchTiming::peak_rss_kib_from_proc(),
    };
    let result = resource_bench_result(&report, timing, environment)?;

    fs::create_dir_all(&output_dir)?;
    fs::write(
        output_dir.join("resource-soak-result.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    fs::write(
        output_dir.join("resource-soak-report.json"),
        report.to_json_pretty()?,
    )?;

    println!(
        "suite={suite} mode={mode} dataset={dataset} events={events} wall_clock_ms={wall_clock_ms}",
        suite = RESOURCE_BENCH_SUITE,
        mode = RESOURCE_BENCH_MODE,
        dataset = RESOURCE_BENCH_DATASET,
        events = report.config.logical_events,
        wall_clock_ms = timing.wall_clock_ms,
    );
    for finding in &report.growth {
        println!(
            "{}: midpoint={} final={} lifetime_growth={} limit_exceeded={}",
            finding.metric,
            finding.midpoint,
            finding.final_count,
            finding.lifetime_growth_detected,
            finding.configured_limit_exceeded,
        );
    }
    for (name, invariant) in &result.invariants {
        println!("{name}={}", invariant.value);
    }
    println!(
        "result={}",
        output_dir.join("resource-soak-result.json").display()
    );

    let limit_violations = result
        .invariants
        .get("resource.retention_bound_violation_count")
        .map(|invariant| invariant.value)
        .unwrap_or(0);
    let non_plateau = result
        .invariants
        .get("resource.non_plateau_state_count")
        .map(|invariant| invariant.value)
        .unwrap_or(0);
    if limit_violations > 0 {
        return Err(io::Error::other("configured retained-state bound exceeded").into());
    }
    if non_plateau > 0 {
        return Err(io::Error::other("retained state did not plateau after warm-up").into());
    }
    Ok(())
}

fn git_revision() -> String {
    let revision =
        command_output("git", &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|output| !output.stdout.is_empty());
    if dirty {
        format!("{revision}+dirty")
    } else {
        revision
    }
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}
