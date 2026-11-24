//! Test double for `local-tts-sidecar` (#220 S2b). Same flags, same server,
//! same ready line and stdin-EOF exit, with a model-free tone synthesizer in
//! place of the engine (a quiet tone, 100 ms per character). It speaks nothing
//! and must never be used as a voice; it exists so the app's supervision and
//! the contract can be tested with no model and no engine libraries.
//!
//! Extra flags: `--mode normal|exit|hang|never-ready|fail|gated`, `--after-ms N`
//! (delay before `exit`/`hang`), `--synth-delay-ms N`.
//! * `normal`      serve.
//! * `exit`        become ready, then exit with code 1 after `--after-ms`.
//! * `hang`        become ready, then stop answering (the socket stays open).
//! * `never-ready` print nothing and wait.
//! * `fail`        serve, but every synthesis fails.
//! * `gated`       signal entry and wait for the test's release marker in the audio root.

use aivtuber_adapters::speech_tts::runner::{parse_flags, run, server_config_from_flags};
use aivtuber_adapters::speech_tts::server::TtsServer;
use aivtuber_adapters::speech_tts::{
    GenerationSettings, Pcm, SynthError, Synthesizer, ToneSynthesizer,
};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

// Only the test double supports this synchronization protocol. The caller
// owns a fresh scratch directory; no production server or engine is changed.
struct GatedTone {
    root: PathBuf,
    tone: ToneSynthesizer,
}

impl Synthesizer for GatedTone {
    fn synthesize(
        &self,
        text: &str,
        settings: &GenerationSettings,
        deadline: Instant,
    ) -> Result<Pcm, SynthError> {
        let release = self.root.join("test-synth-release");
        let io_error = |_| SynthError::Engine("test synthesis gate I/O failed".to_owned());
        if !release.try_exists().map_err(io_error)? {
            // Entry here proves the actual server holds its synthesis guard.
            std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(self.root.join("test-synth-entered"))
                .map_err(io_error)?;
            while !release.try_exists().map_err(io_error)? {
                if Instant::now() >= deadline {
                    return Err(SynthError::DeadlineExceeded);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if Instant::now() >= deadline {
            return Err(SynthError::DeadlineExceeded);
        }
        self.tone.synthesize(text, settings, deadline)
    }

    fn bundle_sha256(&self) -> &str {
        self.tone.bundle_sha256()
    }
}

fn main() {
    let mut flags = parse_flags(std::env::args().skip(1)).unwrap_or_else(|e| {
        eprintln!("local-tts-sidecar-double: {e}");
        std::process::exit(2);
    });
    let mode = flags
        .remove("--mode")
        .unwrap_or_else(|| "normal".to_owned());
    let after_ms: u64 = flags
        .remove("--after-ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let synth_delay_ms: u64 = flags
        .remove("--synth-delay-ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // The double ignores the real engine's flags.
    for name in ["--model", "--model-dir", "--threads"] {
        flags.remove(name);
    }
    let config = server_config_from_flags(&mut flags).unwrap_or_else(|e| {
        eprintln!("local-tts-sidecar-double: {e}");
        std::process::exit(2);
    });
    if mode == "never-ready" {
        std::thread::sleep(Duration::from_secs(600));
        return;
    }
    let synth = ToneSynthesizer {
        delay: Duration::from_millis(synth_delay_ms),
        fail: mode == "fail",
        ..ToneSynthesizer::default()
    };
    let synth: Arc<dyn Synthesizer> = if mode == "gated" {
        Arc::new(GatedTone {
            root: config.audio_root.clone(),
            tone: synth,
        })
    } else {
        Arc::new(synth)
    };
    let server = TtsServer::bind(config, synth).unwrap_or_else(|e| {
        eprintln!("local-tts-sidecar-double: {e}");
        std::process::exit(2);
    });
    match mode.as_str() {
        "exit" => {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(after_ms));
                std::process::exit(1);
            });
        }
        "hang" => {
            // Ready, then never accept: connections queue in the backlog.
            let ready = aivtuber_adapters::speech_tts::supervisor::TtsReady {
                listen: server.local_addr(),
                model_id: "tone-double".to_owned(),
                model_version: "double".to_owned(),
            };
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(
                aivtuber_adapters::speech_tts::supervisor::encode_ready_line(&ready).as_bytes(),
            );
            let _ = stdout.flush();
            let _keep = server;
            std::thread::sleep(Duration::from_secs(600));
            return;
        }
        _ => {}
    }
    run(server, "tone-double", "double");
}
