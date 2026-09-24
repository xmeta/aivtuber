use aivtuber_hardening::{SoakConfig, run_core_soak};
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
    let mut args = env::args().skip(1);
    let output = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/hardening-soak.json"));
    let mut config = SoakConfig::default();
    if let Some(events) = args.next() {
        config.logical_events = events.parse().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("logical event count must be an unsigned integer: {error}"),
            )
        })?;
    }
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: failure-soak [output.json] [logical-events]",
        )
        .into());
    }

    let metadata = ReproducibilityMetadata {
        dataset_id: "hardening-soak-v1".to_owned(),
        git_commit: git_revision(),
        rust_toolchain: command_output("rustc", &["--version"])
            .unwrap_or_else(|| "unknown-rustc".to_owned()),
        bun_toolchain: command_output("bun", &["--version"]),
        config_version: "hardening-soak-v1".to_owned(),
        asset_version: "generated-dynamic-fixture-v1".to_owned(),
        index_version: None,
        jev_model: None,
        thinking_model: None,
        tts_model: None,
        seed: 40,
        stream_duration_ms: Some(config.logical_duration_ms()),
    };

    let report = run_core_soak(config, metadata)?;
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, report.to_json_pretty()?)?;

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
