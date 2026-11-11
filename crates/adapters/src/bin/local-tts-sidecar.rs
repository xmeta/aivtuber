//! Local Japanese TTS sidecar (#220 S2b).
//!
//! A loopback HTTP server that speaks the existing normalized TTS contract and
//! synthesizes with Supertonic 3 int8 through the official sherpa-onnx crate.
//! The app's `AIVTUBER_TTS_SIDECAR` settings spawn and supervise it; it can
//! also be started by hand:
//!
//! ```text
//! local-tts-sidecar --model-dir D:/models/supertonic --audio-root D:/aivtuber/audio \
//!     --listen 127.0.0.1:8770 [--speaker-id 0] [--threads 4] [--viseme-root PACK_ROOT]
//! ```
//!
//! The model files and the engine libraries beside this binary must match the
//! SHA-256 pins compiled in; otherwise it exits without loading anything. It
//! prints the voice licence notice at start, one JSON ready line on stdout once
//! it listens, and diagnostics on stderr (never the spoken text). It serves
//! until its stdin reaches EOF, so a supervising app that exits ends it.
//!
//! Flags: see `aivtuber_adapters::speech_tts::runner::server_config_from_flags`
//! for the server flags; `--model supertonic-3-int8`, `--model-dir DIR`,
//! `--threads N` are the engine's.
//!
//! Exit codes: 0 clean, 2 bad arguments or a failed pin check, 5 the engine
//! could not be created.

use aivtuber_adapters::speech_stt::pins::{bundle_sha256, verify_engine_libs, verify_model_dir};
use aivtuber_adapters::speech_tts::pins::{VOICE_LICENCE_NOTICE, model_by_name};
use aivtuber_adapters::speech_tts::runner::{parse_flags, run, server_config_from_flags};
use aivtuber_adapters::speech_tts::server::TtsServer;
use aivtuber_adapters::speech_tts::sherpa::SherpaSynthesizer;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

fn usage() -> &'static str {
    "usage: local-tts-sidecar --model-dir DIR --audio-root DIR [--model supertonic-3-int8] \
     [--listen 127.0.0.1:8770] [--path /v1/synthesize] [--viseme-root DIR] [--speaker-id N] \
     [--steps N] [--speed F] [--threads N] [--synth-deadline-ms N] [--io-timeout-ms N] \
     [--max-text-chars N] [--max-body-bytes N] [--max-connections N] [--max-cache-mb N]"
}

fn fail(code: u8, message: impl std::fmt::Display) -> ExitCode {
    eprintln!("local-tts-sidecar: {message}");
    ExitCode::from(code)
}

fn main() -> ExitCode {
    let mut flags = match parse_flags(std::env::args().skip(1)) {
        Ok(flags) => flags,
        Err(message) => return fail(2, format!("{message}\n{}", usage())),
    };
    let model_name = flags
        .remove("--model")
        .unwrap_or_else(|| "supertonic-3-int8".to_owned());
    let Some(pin) = model_by_name(&model_name) else {
        return fail(
            2,
            format!("unknown --model {model_name:?} (supertonic-3-int8)"),
        );
    };
    let Some(model_dir) = flags.remove("--model-dir").map(PathBuf::from) else {
        return fail(2, format!("--model-dir is required\n{}", usage()));
    };
    let threads: i32 = match flags.remove("--threads").map(|t| t.parse::<i32>()) {
        None => 4,
        Some(Ok(threads)) => threads,
        Some(Err(_)) => return fail(2, "--threads must be a number"),
    };
    let config = match server_config_from_flags(&mut flags) {
        Ok(config) => config,
        Err(message) => return fail(2, format!("{message}\n{}", usage())),
    };
    if let Some(unknown) = flags.keys().next() {
        return fail(2, format!("unknown argument {unknown}\n{}", usage()));
    }

    // The engine libraries are loaded from this executable's own directory
    // (rpath $ORIGIN on Linux, the application directory on Windows; see
    // build.rs), so this is the very set of files the loader used. The app
    // checks the same directory before it spawns this process, which is the
    // check that can still prevent a load.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    // Reading and hashing the pinned files is disk-bound and is the part of a
    // start that varies most with a cold file cache, so it is timed apart from
    // the engine load (`load_ms` covers only the load).
    let verify_started = std::time::Instant::now();
    if let Err(error) = verify_engine_libs(&exe_dir) {
        return fail(
            2,
            format!(
                "engine libraries beside the executable ({}): {error}",
                exe_dir.display()
            ),
        );
    }
    if let Err(error) = verify_model_dir(&pin, &model_dir) {
        return fail(2, format!("model files: {error}"));
    }

    let verify_ms = verify_started.elapsed().as_millis();
    eprintln!("local-tts-sidecar: {VOICE_LICENCE_NOTICE}");
    eprintln!(
        "local-tts-sidecar: loading model={} language=ja speaker_id={} steps={} threads={threads} style=ignored",
        pin.model_id, config.voice.speaker_id, config.voice.num_steps
    );
    let started = std::time::Instant::now();
    let synth = match SherpaSynthesizer::load(&pin, &model_dir, threads) {
        Ok(synth) => synth,
        Err(error) => return fail(5, error),
    };
    if config.voice.speaker_id >= synth.num_speakers() {
        return fail(
            2,
            format!(
                "--speaker-id {} but the model has {} voices",
                config.voice.speaker_id,
                synth.num_speakers()
            ),
        );
    }
    let server = match TtsServer::bind(config.clone(), Arc::new(synth)) {
        Ok(server) => server,
        Err(error) => return fail(2, error),
    };
    eprintln!(
        "local-tts-sidecar: ready listen={} verify_ms={verify_ms} load_ms={} audio_root={} visemes={}",
        server.local_addr(),
        started.elapsed().as_millis(),
        config.audio_root.display(),
        if config.viseme_root.is_some() {
            "amplitude"
        } else {
            "off"
        }
    );
    run(server, pin.model_id, &bundle_sha256(&pin));
    ExitCode::SUCCESS
}
