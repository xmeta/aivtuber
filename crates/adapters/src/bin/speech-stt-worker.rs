//! Local Japanese speech-to-text worker (#92 S3b).
//!
//! Spawned and fed by `speech-vad-sidecar`; not meant to be run by hand.
//! stdin: 16 kHz mono PCM framing plus segment start/end control (see
//! `aivtuber_adapters::speech_stt::protocol`). stdout: one
//! `speech-transcript` batch (schemas/speech-transcript.schema.json) JSON line
//! per revision, and a heartbeat line about every second once the model is
//! loaded. Diagnostics go to stderr only.
//!
//! ```text
//! speech-stt-worker --model k2v2-int8 --model-dir D:/models/k2v2 --language ja
//! ```
//!
//! The model files and the engine libraries beside this binary must match the
//! SHA-256 pins compiled in; otherwise the worker exits without loading
//! anything. Transcript text is the engine's output, unmodified. Audio is held
//! in memory for the open segment only and is never written anywhere.
//!
//! Exit codes: 0 clean, 2 bad arguments or failed pin check, 3 a decode ran
//! past its deadline, 4 malformed audio pipe, 5 the recognizer failed.

use aivtuber_adapters::speech_stt::pins::{
    bundle_sha256, model_by_name, verify_engine_libs, verify_model_dir,
};
use aivtuber_adapters::speech_stt::sherpa::SherpaDecoder;
use aivtuber_adapters::speech_stt::worker::{WorkerConfig, run_worker};
use aivtuber_adapters::speech_stt::{STT_PROVIDER_ID, STT_PROVIDER_VERSION};
use aivtuber_domain::TranscriptEngine;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

struct Args {
    model: String,
    model_dir: PathBuf,
    language: String,
    speaker_id: String,
    threads: i32,
    partial_step_ms: u64,
    max_segment_ms: u64,
    decode_deadline_ms: u64,
    heartbeat_ms: u64,
}

fn usage() -> String {
    "usage: speech-stt-worker --model k2v2-int8|parakeet-ja --model-dir DIR --language TAG \
     [--speaker-id ID] [--threads N] [--partial-step-ms N] [--max-segment-ms N] \
     [--decode-deadline-ms N] [--heartbeat-ms N]"
        .to_owned()
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        model: String::new(),
        model_dir: PathBuf::new(),
        language: String::new(),
        speaker_id: "mic-local".to_owned(),
        threads: 4,
        partial_step_ms: 500,
        max_segment_ms: 30_000,
        decode_deadline_ms: 10_000,
        heartbeat_ms: 1_000,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = |name: &str| iter.next().ok_or_else(|| format!("{name} needs a value"));
        let int = |text: String, name: &str| {
            text.parse::<u64>()
                .map_err(|_| format!("{name} must be a non-negative integer"))
        };
        match flag.as_str() {
            "--model" => a.model = value("--model")?,
            "--model-dir" => a.model_dir = PathBuf::from(value("--model-dir")?),
            "--language" => a.language = value("--language")?,
            "--speaker-id" => a.speaker_id = value("--speaker-id")?,
            "--threads" => a.threads = int(value("--threads")?, "--threads")?.min(64) as i32,
            "--partial-step-ms" => {
                a.partial_step_ms = int(value("--partial-step-ms")?, "--partial-step-ms")?
            }
            "--max-segment-ms" => {
                a.max_segment_ms = int(value("--max-segment-ms")?, "--max-segment-ms")?
            }
            "--decode-deadline-ms" => {
                a.decode_deadline_ms = int(value("--decode-deadline-ms")?, "--decode-deadline-ms")?
            }
            "--heartbeat-ms" => a.heartbeat_ms = int(value("--heartbeat-ms")?, "--heartbeat-ms")?,
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
    }
    if a.language.is_empty() {
        return Err("--language is required (declared, never inferred)".to_owned());
    }
    if a.model.is_empty() || a.model_dir.as_os_str().is_empty() {
        return Err(format!("--model and --model-dir are required\n{}", usage()));
    }
    Ok(a)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("speech-stt-worker: {message}");
            return ExitCode::from(2);
        }
    };
    let Some(pin) = model_by_name(&args.model) else {
        eprintln!(
            "speech-stt-worker: unknown --model {:?} (k2v2-int8, parakeet-ja)",
            args.model
        );
        return ExitCode::from(2);
    };
    // The engine libraries are loaded from this executable's own directory
    // (rpath $ORIGIN on Linux, the application directory on Windows; see
    // build.rs), so this is the very set of files the loader used. The loader
    // has already run their initializers, so this catches a wrong or corrupt
    // library before it is *used*; the sidecar checks the same directory
    // before it spawns this process, which is the check that can still
    // prevent a load.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    if let Err(error) = verify_engine_libs(&exe_dir) {
        eprintln!(
            "speech-stt-worker: engine libraries beside the executable ({}): {error}",
            exe_dir.display()
        );
        return ExitCode::from(2);
    }
    if let Err(error) = verify_model_dir(&pin, &args.model_dir) {
        eprintln!("speech-stt-worker: model files: {error}");
        return ExitCode::from(2);
    }
    let config = WorkerConfig {
        provider_id: STT_PROVIDER_ID.to_owned(),
        provider_version: STT_PROVIDER_VERSION.to_owned(),
        language: args.language.clone(),
        speaker_id: args.speaker_id.clone(),
        engine: TranscriptEngine {
            model_id: pin.model_id.to_owned(),
            sha256: bundle_sha256(&pin),
        },
        partial_step_ms: args.partial_step_ms,
        max_segment_ms: args.max_segment_ms,
        decode_deadline: Duration::from_millis(args.decode_deadline_ms),
        heartbeat_interval: Duration::from_millis(args.heartbeat_ms),
    };
    if let Err(error) = config.validate() {
        eprintln!("speech-stt-worker: {error}");
        return ExitCode::from(2);
    }
    eprintln!(
        "speech-stt-worker: loading model={} language={} threads={}",
        pin.model_id, args.language, args.threads
    );
    let (model_dir, threads) = (args.model_dir.clone(), args.threads);
    let started = std::time::Instant::now();
    let (exit, stats) = run_worker(
        std::io::stdin(),
        std::io::stdout(),
        move || {
            let decoder = SherpaDecoder::load(&pin, &model_dir, threads)?;
            eprintln!(
                "speech-stt-worker: ready load_ms={}",
                started.elapsed().as_millis()
            );
            Ok(decoder)
        },
        &config,
    );
    eprintln!("speech-stt-worker: exit {exit:?} stats={stats:?}");
    // A decode that outlived its deadline cannot be cancelled, so end the
    // whole process rather than wait for it.
    std::process::exit(exit.exit_code());
}
