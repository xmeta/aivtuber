//! Command-line plumbing shared by `local-tts-sidecar` and its test double
//! (#220 S2b): flag parsing into a [`ServerConfig`], and the run loop that
//! announces readiness and exits when the supervising app goes away.

use super::VoiceConfig;
use super::server::{DEFAULT_PATH, ServerConfig, TtsServer};
use super::supervisor::{TtsReady, encode_ready_line};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// `--flag value` pairs. Every flag takes a value; a repeated flag is an error.
pub fn parse_flags(args: impl Iterator<Item = String>) -> Result<HashMap<String, String>, String> {
    let mut flags = HashMap::new();
    let mut iter = args;
    while let Some(flag) = iter.next() {
        if !flag.starts_with("--") {
            return Err(format!("unexpected argument {flag:?}"));
        }
        let value = iter.next().ok_or_else(|| format!("{flag} needs a value"))?;
        if flags.insert(flag.clone(), value).is_some() {
            return Err(format!("{flag} given twice"));
        }
    }
    Ok(flags)
}

fn take_number<T: std::str::FromStr>(
    flags: &mut HashMap<String, String>,
    name: &str,
) -> Result<Option<T>, String> {
    flags
        .remove(name)
        .map(|text| {
            text.parse::<T>()
                .map_err(|_| format!("{name} must be a number"))
        })
        .transpose()
}

/// Consume the server flags from `flags`; whatever is left is for the caller.
///
/// `--listen HOST:PORT` (default `127.0.0.1:8770`), `--audio-root DIR`
/// (required), `--viseme-root DIR`, `--path /v1/synthesize`, `--speaker-id N`,
/// `--steps N`, `--speed F`, `--synth-deadline-ms N`, `--io-timeout-ms N`,
/// `--max-text-chars N`, `--max-body-bytes N`, `--max-connections N`,
/// `--max-cache-mb N`.
pub fn server_config_from_flags(
    flags: &mut HashMap<String, String>,
) -> Result<ServerConfig, String> {
    let listen: SocketAddr = flags
        .remove("--listen")
        .unwrap_or_else(|| "127.0.0.1:8770".to_owned())
        .parse()
        .map_err(|_| "--listen must be an IP:PORT, e.g. 127.0.0.1:8770".to_owned())?;
    let audio_root = flags
        .remove("--audio-root")
        .map(PathBuf::from)
        .ok_or_else(|| "--audio-root is required (the app's AIVTUBER_AUDIO_ROOT)".to_owned())?;
    let mut config = ServerConfig::new(listen, audio_root);
    config.viseme_root = flags.remove("--viseme-root").map(PathBuf::from);
    if let Some(path) = flags.remove("--path") {
        config.path = path;
    } else {
        config.path = DEFAULT_PATH.to_owned();
    }
    let mut voice = VoiceConfig::default();
    if let Some(value) = take_number::<i32>(flags, "--speaker-id")? {
        voice.speaker_id = value;
    }
    if let Some(value) = take_number::<i32>(flags, "--steps")? {
        voice.num_steps = value;
    }
    if let Some(value) = take_number::<f32>(flags, "--speed")? {
        voice.speed = value;
    }
    config.voice = voice;
    if let Some(ms) = take_number::<u64>(flags, "--synth-deadline-ms")? {
        config.synth_deadline = Duration::from_millis(ms);
    }
    if let Some(ms) = take_number::<u64>(flags, "--io-timeout-ms")? {
        config.io_timeout = Duration::from_millis(ms);
    }
    if let Some(value) = take_number::<usize>(flags, "--max-text-chars")? {
        config.max_text_chars = value;
    }
    if let Some(value) = take_number::<usize>(flags, "--max-body-bytes")? {
        config.max_body_bytes = value;
    }
    if let Some(value) = take_number::<usize>(flags, "--max-connections")? {
        config.max_connections = value;
    }
    if let Some(mb) = take_number::<u64>(flags, "--max-cache-mb")? {
        config.max_cache_bytes = mb.saturating_mul(1024 * 1024);
    }
    config.validate()?;
    Ok(config)
}

/// Print the ready line on stdout and serve until stdin reaches EOF (the
/// supervising app exited or closed the pipe). Returns when it is time to exit.
pub fn run(server: TtsServer, model_id: &str, model_version: &str) {
    let ready = TtsReady {
        listen: server.local_addr(),
        model_id: model_id.to_owned(),
        model_version: model_version.to_owned(),
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        std::thread::spawn(move || {
            let mut sink = [0_u8; 256];
            let mut stdin = std::io::stdin();
            while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
            shutdown.store(true, Ordering::Release);
        });
    }
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(encode_ready_line(&ready).as_bytes());
    let _ = stdout.flush();
    server.serve(&shutdown);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(args: &[&str]) -> Result<HashMap<String, String>, String> {
        parse_flags(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn flags_must_be_pairs_and_unique() {
        assert!(flags(&["--a", "1", "--b", "2"]).is_ok());
        assert!(flags(&["--a"]).is_err());
        assert!(flags(&["a", "1"]).is_err());
        assert!(flags(&["--a", "1", "--a", "2"]).is_err());
    }

    #[test]
    fn server_flags_build_a_validated_loopback_config() {
        let dir = std::env::temp_dir();
        let dir = dir.to_str().unwrap();
        let mut parsed = flags(&[
            "--audio-root",
            dir,
            "--listen",
            "127.0.0.1:9000",
            "--speaker-id",
            "3",
            "--synth-deadline-ms",
            "8000",
            "--model-dir",
            "x",
        ])
        .unwrap();
        let config = server_config_from_flags(&mut parsed).unwrap();
        assert_eq!(config.listen.port(), 9000);
        assert_eq!(config.voice.speaker_id, 3);
        assert_eq!(config.synth_deadline, Duration::from_secs(8));
        // Unknown-to-this-function flags are left for the binary.
        assert_eq!(parsed.keys().collect::<Vec<_>>(), ["--model-dir"]);
    }

    #[test]
    fn a_non_loopback_listen_address_or_a_missing_audio_root_is_refused() {
        let dir = std::env::temp_dir();
        let dir = dir.to_str().unwrap();
        let mut remote = flags(&["--audio-root", dir, "--listen", "0.0.0.0:9000"]).unwrap();
        assert!(
            server_config_from_flags(&mut remote)
                .unwrap_err()
                .contains("loopback")
        );
        let mut missing = flags(&["--listen", "127.0.0.1:9000"]).unwrap();
        assert!(
            server_config_from_flags(&mut missing)
                .unwrap_err()
                .contains("--audio-root")
        );
        let mut bad_voice = flags(&["--audio-root", dir, "--speaker-id", "99"]).unwrap();
        assert!(server_config_from_flags(&mut bad_voice).is_err());
    }
}
