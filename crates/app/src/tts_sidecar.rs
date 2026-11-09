//! Settings and argument derivation for the supervised local TTS sidecar
//! (#220 S2b).
//!
//! The `local` profile can spawn `local-tts-sidecar` as a child
//! (`AIVTUBER_TTS_SIDECAR`, off by default) or point at a separately started
//! one through `AIVTUBER_TTS_ENDPOINT` alone. Supervising is the S3b pattern
//! (the app owns the child's lifecycle, the first failure is final, no
//! restart, no `AIVTUBER_*` environment for the child, engine libraries pinned
//! beside it) and has one advantage worth the code: the listen address and the
//! audio root are *derived* from the settings the app itself uses, so the two
//! processes cannot disagree about where a WAV file is, which is the most
//! likely misconfiguration of two hand-started processes.

use aivtuber_adapters::speech_tts::supervisor::TtsSidecarConfig;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct TtsSidecarSettings {
    /// `AIVTUBER_TTS_SIDECAR`: the `local-tts-sidecar` executable.
    pub program: PathBuf,
    /// `AIVTUBER_TTS_SIDECAR_MODEL_DIR`.
    pub model_dir: PathBuf,
    /// The app's own audio root (`AIVTUBER_AUDIO_ROOT`), where the player
    /// resolves `audio://` references.
    pub audio_root: PathBuf,
    /// The pack root the viseme store reads, when amplitude visemes are on
    /// (`AIVTUBER_TTS_SIDECAR_AMPLITUDE_VISEMES`).
    pub viseme_root: Option<PathBuf>,
    /// `AIVTUBER_TTS_ENDPOINT`: where the adapter posts. Must be loopback.
    pub endpoint: String,
    pub speaker_id: u32,
    pub threads: u32,
    pub ready_timeout: Duration,
    /// `AIVTUBER_TTS_SIDECAR_ARGS`, whitespace separated (for the deadline,
    /// caps and the like). A flag the app derives cannot be repeated here.
    pub extra_args: Vec<String>,
}

/// The address and path a loopback `http://` endpoint names. The port is
/// required: the app and the sidecar must agree on it. `localhost` is bound as
/// 127.0.0.1.
pub fn loopback_listen_target(endpoint: &str) -> Result<(SocketAddr, String), String> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or("AIVTUBER_TTS_ENDPOINT must be an http:// loopback URL to supervise a sidecar")?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        return Err("AIVTUBER_TTS_ENDPOINT must not carry credentials".to_owned());
    }
    let address: SocketAddr = match authority.strip_prefix("localhost:") {
        Some(port) => SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            port.parse::<u16>().map_err(|_| "invalid endpoint port")?,
        ),
        None => authority
            .parse()
            .map_err(|_| "AIVTUBER_TTS_ENDPOINT needs a loopback IP or localhost and a port")?,
    };
    if !address.ip().is_loopback() {
        return Err("AIVTUBER_TTS_ENDPOINT is not a loopback address".to_owned());
    }
    if address.port() == 0 {
        return Err("AIVTUBER_TTS_ENDPOINT needs an explicit port".to_owned());
    }
    if path.contains(['?', '#']) {
        return Err("AIVTUBER_TTS_ENDPOINT must not have a query".to_owned());
    }
    Ok((address, path.to_owned()))
}

/// Flags the app sets itself. A repeat in the extra arguments would be
/// rejected by the sidecar; refusing it here names the setting.
const DERIVED_FLAGS: [&str; 8] = [
    "--model-dir",
    "--audio-root",
    "--viseme-root",
    "--listen",
    "--path",
    "--speaker-id",
    "--threads",
    "--model",
];

impl TtsSidecarSettings {
    pub fn args(&self) -> Result<Vec<String>, String> {
        let (listen, path) = loopback_listen_target(&self.endpoint)?;
        let utf8 = |path: &PathBuf, what: &str| {
            path.to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{what} is not valid UTF-8"))
        };
        let mut args = vec![
            "--model-dir".to_owned(),
            utf8(&self.model_dir, "AIVTUBER_TTS_SIDECAR_MODEL_DIR")?,
            "--audio-root".to_owned(),
            utf8(&self.audio_root, "AIVTUBER_AUDIO_ROOT")?,
            "--listen".to_owned(),
            listen.to_string(),
            "--path".to_owned(),
            path,
            "--speaker-id".to_owned(),
            self.speaker_id.to_string(),
            "--threads".to_owned(),
            self.threads.to_string(),
        ];
        if let Some(root) = &self.viseme_root {
            args.push("--viseme-root".to_owned());
            args.push(utf8(root, "AIVTUBER_PACK_ROOT")?);
        }
        if let Some(repeat) = self
            .extra_args
            .iter()
            .find(|arg| DERIVED_FLAGS.contains(&arg.as_str()))
        {
            return Err(format!(
                "AIVTUBER_TTS_SIDECAR_ARGS must not repeat {repeat}: the app derives it from its own settings"
            ));
        }
        args.extend(self.extra_args.iter().cloned());
        Ok(args)
    }

    pub fn sidecar_config(&self) -> Result<TtsSidecarConfig, String> {
        let mut config = TtsSidecarConfig::new(&self.program, self.args()?);
        config.ready_timeout = self.ready_timeout;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> TtsSidecarSettings {
        TtsSidecarSettings {
            program: PathBuf::from("bin/local-tts-sidecar"),
            model_dir: PathBuf::from("models/supertonic"),
            audio_root: PathBuf::from("audio"),
            viseme_root: None,
            endpoint: "http://127.0.0.1:8770/v1/synthesize".to_owned(),
            speaker_id: 0,
            threads: 4,
            ready_timeout: Duration::from_secs(60),
            extra_args: vec![],
        }
    }

    #[test]
    fn listen_address_path_and_audio_root_are_derived_from_the_apps_own_settings() {
        let args = settings().args().unwrap();
        let value_of = |flag: &str| {
            let at = args.iter().position(|a| a == flag).unwrap();
            args[at + 1].clone()
        };
        assert_eq!(value_of("--listen"), "127.0.0.1:8770");
        assert_eq!(value_of("--path"), "/v1/synthesize");
        assert_eq!(value_of("--audio-root"), "audio");
        assert_eq!(value_of("--model-dir"), "models/supertonic");
        assert!(!args.contains(&"--viseme-root".to_owned()));
        let mut with_visemes = settings();
        with_visemes.viseme_root = Some(PathBuf::from("pack"));
        let args = with_visemes.args().unwrap();
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--viseme-root" && w[1] == "pack")
        );
    }

    #[test]
    fn only_a_loopback_http_endpoint_with_a_port_can_be_supervised() {
        for ok in [
            "http://127.0.0.1:8770/v1/synthesize",
            "http://localhost:9000/x",
            "http://[::1]:8770/",
            "http://127.0.0.1:8770",
        ] {
            assert!(loopback_listen_target(ok).is_ok(), "{ok}");
        }
        for bad in [
            "https://127.0.0.1:8770/v1",
            "http://192.168.1.4:8770/v1",
            "http://tts.example:8770/v1",
            "http://127.0.0.1/v1",
            "http://127.0.0.1:0/v1",
            "http://user:pw@127.0.0.1:8770/v1",
            "http://127.0.0.1:8770/v1?x=1",
            "127.0.0.1:8770",
            "",
        ] {
            assert!(loopback_listen_target(bad).is_err(), "{bad}");
        }
        assert_eq!(
            loopback_listen_target("http://localhost:9000/x").unwrap().0,
            "127.0.0.1:9000".parse().unwrap()
        );
    }

    #[test]
    fn extra_arguments_cannot_override_a_derived_setting() {
        let mut with_extra = settings();
        with_extra.extra_args = vec!["--synth-deadline-ms".to_owned(), "8000".to_owned()];
        let args = with_extra.args().unwrap();
        assert!(args.ends_with(&["--synth-deadline-ms".to_owned(), "8000".to_owned()]));
        for flag in DERIVED_FLAGS {
            let mut bad = settings();
            bad.extra_args = vec![flag.to_owned(), "x".to_owned()];
            assert!(bad.args().unwrap_err().contains(flag), "{flag}");
        }
    }

    #[test]
    fn the_sidecar_config_carries_the_ready_timeout() {
        let mut s = settings();
        s.ready_timeout = Duration::from_secs(7);
        let config = s.sidecar_config().unwrap();
        assert_eq!(config.ready_timeout, Duration::from_secs(7));
        assert!(config.verify_engine_libs, "production checks the pins");
    }
}
