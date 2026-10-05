//! Operational SLO report CLI — issue #71.
//!
//! Reads a replay/benchmark report produced by `replay-benchmark` (optionally
//! with an operational target file), evaluates the project-owned indicator
//! catalog against it, and writes the SLO report plus a Markdown summary.
//!
//! This is deliberately *not* `benchmark-compare`. That tool answers "did this
//! change regress against the base revision" and is wired into the PR gate;
//! this one answers "is the runtime delivering an acceptable level of service
//! over this session / stream hour". Sharing a file between the two would make
//! a relative judgement look like an operational target.
//!
//! With no target file the report is still useful: it publishes the measured
//! values and target-free latency percentiles that a target is calibrated
//! from, and the verdict stays `UNCALIBRATED` because nothing has been decided
//! yet. That is the honest state of #71 today.
//!
//! Usage:
//! ```text
//! slo-report <benchmark-report.json> [targets.json] [output.json]
//! slo-report --report <benchmark-report.json> [--targets <targets.json>]
//!            [--out <output.json>] [--markdown <summary.md>]
//!            [--provenance scenario-replay|replay-fixture|live-session]
//!            [--plane active|experimental]
//!            [--rolling-window-hours N] [--persistent-miss-windows N]
//!            [--fail-on-breach]
//! ```

#![forbid(unsafe_code)]

use aivtuber_telemetry::{
    BenchmarkReport, SloError, SloEvaluationConfig, SloProvenance, SloTargets, SloVerdict,
    TrafficPlane, evaluate,
};
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: slo-report <benchmark-report.json> [targets.json] [output.json]\n       or: slo-report --report <benchmark-report.json> [--targets <targets.json>] [--out <output.json>] [--markdown <summary.md>] [--provenance <provenance>] [--plane <plane>] [--rolling-window-hours N] [--persistent-miss-windows N] [--fail-on-breach]";

fn main() -> ExitCode {
    match run() {
        Ok(ExitCode::SUCCESS) => ExitCode::SUCCESS,
        Ok(code) => code,
        Err(error) => {
            eprintln!("slo-report: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn Error>> {
    let arguments = parse_args(std::env::args().skip(1).collect())?;
    let report_bytes = fs::read(&arguments.report)?;
    let report: BenchmarkReport = serde_json::from_slice(&report_bytes).map_err(|error| {
        SloError::new(format!(
            "{} is not a replay benchmark report: {error}",
            arguments.report.display()
        ))
    })?;

    let targets = match &arguments.targets {
        Some(path) => SloTargets::from_json(&fs::read(path)?)?,
        None => SloTargets::default(),
    };

    let slo_report = evaluate(&report, &targets, arguments.config)?;

    if let Some(path) = &arguments.output {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, slo_report.to_json_pretty()?)?;
    }
    if let Some(path) = &arguments.markdown {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, slo_report.markdown_summary())?;
    }

    print!("{}", slo_report.markdown_summary());

    if arguments.fail_on_breach && slo_report.verdict == SloVerdict::Breach {
        eprintln!("slo-report: operational SLO breach");
        return Ok(ExitCode::from(2));
    }
    Ok(ExitCode::SUCCESS)
}

struct Arguments {
    report: PathBuf,
    targets: Option<PathBuf>,
    output: Option<PathBuf>,
    markdown: Option<PathBuf>,
    config: SloEvaluationConfig,
    fail_on_breach: bool,
}

fn parse_args(args: Vec<String>) -> Result<Arguments, Box<dyn Error>> {
    let mut report: Option<PathBuf> = None;
    let mut targets: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut markdown: Option<PathBuf> = None;
    let mut provenance: Option<SloProvenance> = None;
    let mut plane: Option<TrafficPlane> = None;
    let mut rolling_window_hours: Option<u32> = None;
    let mut persistent_miss_windows: Option<u64> = None;
    let mut fail_on_breach = false;
    let mut positional: Vec<PathBuf> = Vec::new();

    let mut index = 0;
    while index < args.len() {
        let flag = args[index].clone();
        let next = || -> Result<&String, Box<dyn Error>> {
            args.get(index + 1)
                .ok_or_else(|| format!("{flag} requires a value\n{USAGE}").into())
        };
        match args[index].as_str() {
            "--report" => {
                report = Some(PathBuf::from(next()?));
                index += 2;
            }
            "--targets" => {
                targets = Some(PathBuf::from(next()?));
                index += 2;
            }
            "--out" => {
                output = Some(PathBuf::from(next()?));
                index += 2;
            }
            "--markdown" => {
                markdown = Some(PathBuf::from(next()?));
                index += 2;
            }
            "--provenance" => {
                provenance = Some(match next()?.as_str() {
                    "scenario_replay" | "scenario-replay" => SloProvenance::ScenarioReplay,
                    "replay_fixture" | "replay-fixture" => SloProvenance::ReplayFixture,
                    "live_session" | "live-session" => SloProvenance::LiveSession,
                    other => return Err(format!("unknown provenance {other:?}\n{USAGE}").into()),
                });
                index += 2;
            }
            "--plane" => {
                plane = Some(match next()?.as_str() {
                    "active" => TrafficPlane::Active,
                    "experimental" => TrafficPlane::Experimental,
                    other => return Err(format!("unknown plane {other:?}\n{USAGE}").into()),
                });
                index += 2;
            }
            "--rolling-window-hours" => {
                let raw = next()?.clone();
                rolling_window_hours = Some(raw.parse::<u32>().map_err(|_| {
                    format!("--rolling-window-hours expects a positive integer, got {raw:?}")
                })?);
                index += 2;
            }
            "--persistent-miss-windows" => {
                let raw = next()?.clone();
                persistent_miss_windows = Some(raw.parse::<u64>().map_err(|_| {
                    format!("--persistent-miss-windows expects a non-negative integer, got {raw:?}")
                })?);
                index += 2;
            }
            "--fail-on-breach" => {
                fail_on_breach = true;
                index += 1;
            }
            unknown if unknown.starts_with("--") => {
                return Err(format!("unknown argument {unknown:?}\n{USAGE}").into());
            }
            value => {
                positional.push(PathBuf::from(value));
                index += 1;
            }
        }
    }

    if positional.len() > 3 {
        return Err(format!("too many positional arguments\n{USAGE}").into());
    }
    if report.is_none() {
        report = positional.first().cloned();
    }
    if targets.is_none() {
        targets = positional.get(1).cloned();
    }
    if output.is_none() {
        output = positional.get(2).cloned();
    }
    let Some(report) = report else {
        return Err(format!("a replay benchmark report is required\n{USAGE}").into());
    };
    if rolling_window_hours == Some(0) {
        return Err(format!("--rolling-window-hours must be at least 1\n{USAGE}").into());
    }

    let defaults = SloEvaluationConfig::default();
    Ok(Arguments {
        report,
        targets,
        output,
        markdown,
        config: SloEvaluationConfig {
            plane: plane.unwrap_or(defaults.plane),
            provenance: provenance.unwrap_or(defaults.provenance),
            rolling_window_hours: rolling_window_hours.unwrap_or(defaults.rolling_window_hours),
            persistent_miss_windows: persistent_miss_windows
                .unwrap_or(defaults.persistent_miss_windows),
        },
        fail_on_breach,
    })
}
