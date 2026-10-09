//! Deterministic failure/soak CLI (issue #40).
//!
//! Two modes:
//!
//! * the historical synthetic resource soak, `failure-soak [output.json]
//!   [logical-events]`;
//! * the issue #70 shared workload, `failure-soak --scenario <path> --overlay
//!   <path> [output.json]`, which runs the same generated trace the #58 benchmark
//!   plays and overlays the versioned fault plan bound to that scenario.
//!
//! Exit status is non-zero when a configured retained-state bound is violated or
//! emergency control fails during the content flood.

use aivtuber_domain::StreamScenario;
use aivtuber_hardening::{FaultOverlay, SoakConfig, run_core_soak, run_scenario_soak};
use aivtuber_telemetry::ReproducibilityMetadata;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    if let Err(error) = run() {
        eprintln!("failure-soak: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let mut scenario_path: Option<PathBuf> = None;
    let mut overlay_path: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--scenario" => {
                scenario_path = Some(PathBuf::from(arguments.get(index + 1).ok_or_else(
                    || invalid_input("--scenario requires a path to a stream scenario document"),
                )?));
                index += 2;
            }
            "--overlay" => {
                overlay_path = Some(PathBuf::from(arguments.get(index + 1).ok_or_else(
                    || invalid_input("--overlay requires a path to a fault-overlay document"),
                )?));
                index += 2;
            }
            other => {
                positional.push(other.to_owned());
                index += 1;
            }
        }
    }

    let output = positional
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/hardening-soak.json"));

    match scenario_path {
        Some(scenario_path) => {
            if positional.len() > 1 {
                return Err(invalid_input(
                    "usage: failure-soak --scenario <path> --overlay <path> [output.json]",
                )
                .into());
            }
            let overlay_path = overlay_path.ok_or_else(|| {
                invalid_input("--scenario requires --overlay: a scenario soak must name the faults it overlays")
            })?;
            run_scenario_mode(&scenario_path, &overlay_path, &output)
        }
        None => {
            if overlay_path.is_some() {
                return Err(invalid_input("--overlay requires --scenario").into());
            }
            let mut config = SoakConfig::default();
            if let Some(events) = positional.get(1) {
                config.logical_events = events.parse().map_err(|error| {
                    invalid_input(format!(
                        "logical event count must be an unsigned integer: {error}"
                    ))
                })?;
            }
            if positional.len() > 2 {
                return Err(
                    invalid_input("usage: failure-soak [output.json] [logical-events]").into(),
                );
            }
            run_synthetic_mode(config, &output)
        }
    }
}

fn run_synthetic_mode(config: SoakConfig, output: &PathBuf) -> Result<(), Box<dyn Error>> {
    let metadata = ReproducibilityMetadata {
        // The synthetic soak runs the same link-enabled `run_core_soak`
        // workload the resource bench times; v2 keeps pre-link reports a
        // separate identity so the two workload shapes are never confused
        // (see `RESOURCE_BENCH_DATASET`).
        dataset_id: "hardening-soak-v2".to_owned(),
        git_commit: git_revision(),
        rust_toolchain: command_output("rustc", &["--version"])
            .unwrap_or_else(|| "unknown-rustc".to_owned()),
        bun_toolchain: command_output("bun", &["--version"]),
        config_version: "hardening-soak-v2".to_owned(),
        asset_version: "generated-dynamic-fixture-v1".to_owned(),
        index_version: None,
        jev_model: None,
        thinking_model: None,
        tts_model: None,
        seed: 40,
        stream_duration_ms: Some(config.logical_duration_ms()),
    };

    let report = run_core_soak(config, metadata)?;
    write_report(output, &report.to_json_pretty()?)?;

    for finding in &report.growth {
        println!(
            "{}: midpoint={} final={} lifetime_growth={} limit_exceeded={} related_issue={}",
            finding.metric,
            finding.midpoint,
            finding.final_count,
            finding.lifetime_growth_detected,
            finding.configured_limit_exceeded,
            finding
                .related_issue
                .map(|issue| format!("#{issue}"))
                .unwrap_or_else(|| "-".to_owned()),
        );
    }
    println!(
        "flood: queued={} dropped={} queue={} stop_cancelled={} muted={}",
        report.flood.queued,
        report.flood.dropped_backpressure,
        report.flood.final_queue_len,
        report.flood.stop_cancelled,
        report.flood.mute_succeeded
    );
    println!("report={}", output.display());

    if report
        .growth
        .iter()
        .any(|finding| finding.configured_limit_exceeded)
    {
        return Err(io::Error::other("configured retained-state bound exceeded").into());
    }
    if report.flood.stop_cancelled == 0 || !report.flood.mute_succeeded {
        return Err(io::Error::other("emergency control failed during content flood").into());
    }
    Ok(())
}

fn run_scenario_mode(
    scenario_path: &PathBuf,
    overlay_path: &PathBuf,
    output: &PathBuf,
) -> Result<(), Box<dyn Error>> {
    let scenario: StreamScenario = serde_json::from_slice(&fs::read(scenario_path)?)
        .map_err(|error| invalid_input(format!("scenario document is invalid: {error}")))?;
    let overlay: FaultOverlay = serde_json::from_slice(&fs::read(overlay_path)?)
        .map_err(|error| invalid_input(format!("fault overlay is invalid: {error}")))?;

    let config = SoakConfig::default();
    // The fault plan, not its seed, determines behaviour, so it must determine
    // the recorded config identity too.
    let fault_plan_id = overlay.plan_id()?;
    let dataset_id = format!("{}/faults-{fault_plan_id}", scenario.dataset_id());
    let metadata = ReproducibilityMetadata {
        dataset_id: dataset_id.clone(),
        git_commit: git_revision(),
        rust_toolchain: command_output("rustc", &["--version"])
            .unwrap_or_else(|| "unknown-rustc".to_owned()),
        bun_toolchain: command_output("bun", &["--version"]),
        config_version: format!(
            "scenario-soak-v1;scenario={};version={};fault_plan={fault_plan_id}",
            scenario.scenario_id, scenario.scenario_version
        ),
        asset_version: "generated-dynamic-fixture-v1".to_owned(),
        index_version: None,
        jev_model: None,
        thinking_model: None,
        tts_model: None,
        seed: scenario.seed,
        stream_duration_ms: Some(scenario.stream_duration_ms),
    };

    let report = run_scenario_soak(&scenario, &overlay, config, metadata)?;
    write_report(output, &report.to_json_pretty()?)?;

    println!(
        "scenario={} version={} class={} dataset={} events={} faults_planned={} faults_observed={} ingress_degraded={} generative_degraded={} asset_store_degraded={}",
        report.scenario_id,
        report.scenario_version,
        report.scenario_class,
        report.dataset_id,
        report.trace_events,
        report.faults.planned,
        report.faults.observed(),
        report.faults.content_ingress_degraded,
        report.faults.generative_degraded,
        report.faults.asset_store_degraded,
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
    println!("report={}", output.display());

    if report
        .growth
        .iter()
        .any(|finding| finding.configured_limit_exceeded)
    {
        return Err(io::Error::other("configured retained-state bound exceeded").into());
    }
    Ok(())
}

fn write_report(output: &PathBuf, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(output, bytes)?;
    Ok(())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
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
