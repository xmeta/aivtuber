//! Test double for `speech-stt-worker` (#92 S3b). Speaks the same pipe
//! protocol with a fake decoder (text is `音声<ms>ms`) so the sidecar and app
//! can be tested with no model and no engine libraries. Not for production:
//! it recognises nothing.
//!
//! `--mode` selects a behaviour:
//! * `normal`   run the real worker core with the fake decoder.
//! * `garbage`  become ready, then print only invalid lines.
//! * `silent`   never print anything (never becomes ready).
//! * `exit`     become ready, then exit with code 1 after `--after-ms`.
//! * `hang`     become ready, send heartbeats for `--after-ms`, then go silent
//!   without exiting.
//! * `deadline` the fake decoder takes longer than the decode deadline.

use aivtuber_adapters::speech_stt::worker::{Decoder, WorkerConfig, run_worker};
use aivtuber_adapters::speech_stt::{STT_PROVIDER_ID, STT_PROVIDER_VERSION};
use aivtuber_domain::TranscriptEngine;
use std::io::Write;
use std::time::Duration;

struct Fake {
    delay: Duration,
}

impl Decoder for Fake {
    fn decode(&mut self, samples: &[f32]) -> Result<String, String> {
        std::thread::sleep(self.delay);
        Ok(format!("音声{}ms", samples.len() / 16))
    }
}

fn main() {
    let mut mode = "normal".to_owned();
    let mut after_ms = 300_u64;
    let mut delay_ms = 0_u64;
    let mut language = "ja".to_owned();
    let mut speaker = "mic-local".to_owned();
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let value = iter.next().unwrap_or_default();
        match flag.as_str() {
            "--mode" => mode = value,
            "--after-ms" => after_ms = value.parse().unwrap_or(after_ms),
            "--decode-delay-ms" => delay_ms = value.parse().unwrap_or(0),
            "--language" => language = value,
            "--speaker-id" => speaker = value,
            _ => {}
        }
    }
    let mut out = std::io::stdout();
    let heartbeat = |out: &mut std::io::Stdout, seq: u64| {
        let _ = writeln!(
            out,
            "{{\"heartbeat\":{{\"seq\":{seq},\"decoder_busy\":false}}}}"
        );
        let _ = out.flush();
    };
    match mode.as_str() {
        "silent" => std::thread::sleep(Duration::from_secs(3_600)),
        "exit" => {
            heartbeat(&mut out, 0);
            std::thread::sleep(Duration::from_millis(after_ms));
            std::process::exit(1);
        }
        "hang" => {
            let started = std::time::Instant::now();
            let mut seq = 0;
            while started.elapsed() < Duration::from_millis(after_ms) {
                heartbeat(&mut out, seq);
                seq += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            std::thread::sleep(Duration::from_secs(3_600));
        }
        "garbage" => {
            heartbeat(&mut out, 0);
            for i in 0..u64::MAX {
                let _ = writeln!(out, "garbage line {i}");
                let _ = out.flush();
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        _ => {
            let deadline = if mode == "deadline" {
                Duration::from_millis(300)
            } else {
                Duration::from_secs(10)
            };
            let config = WorkerConfig {
                provider_id: STT_PROVIDER_ID.to_owned(),
                provider_version: STT_PROVIDER_VERSION.to_owned(),
                language,
                speaker_id: speaker,
                engine: TranscriptEngine {
                    model_id: "double:fake".to_owned(),
                    sha256: "0".repeat(64),
                },
                partial_step_ms: 500,
                max_segment_ms: 30_000,
                decode_deadline: deadline,
                heartbeat_interval: Duration::from_millis(100),
            };
            let delay = if mode == "deadline" {
                Duration::from_secs(5)
            } else {
                Duration::from_millis(delay_ms)
            };
            let (exit, _) = run_worker(
                std::io::stdin(),
                std::io::stdout(),
                move || Ok(Fake { delay }),
                &config,
            );
            std::process::exit(exit.exit_code());
        }
    }
}
