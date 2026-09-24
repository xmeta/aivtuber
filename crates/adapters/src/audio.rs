use aivtuber_domain::{EngineError, EngineErrorKind};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

const AUDIO_SCHEME: &str = "audio://";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessAudioConfig {
    pub root: PathBuf,
    pub program: String,
    pub args: Vec<String>,
}

impl Default for ProcessAudioConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            program: "ffplay".to_owned(),
            args: vec![
                "-nodisp".to_owned(),
                "-autoexit".to_owned(),
                "-loglevel".to_owned(),
                "error".to_owned(),
                "-af".to_owned(),
                "atempo={tempo}".to_owned(),
                "{audio}".to_owned(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioPlayRequest {
    pub audio_ref: String,
    pub speed_factor: f64,
}

pub trait AudioPlayer: Send + Sync {
    fn play(&self, request: &AudioPlayRequest) -> Result<(), EngineError>;
}

#[derive(Debug, Clone)]
pub struct ProcessAudioPlayer {
    config: ProcessAudioConfig,
}

impl ProcessAudioPlayer {
    pub fn new(config: ProcessAudioConfig) -> Result<Self, EngineError> {
        if config.root.as_os_str().is_empty() {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "audio root must not be empty",
            ));
        }
        if config.program.trim().is_empty() {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "audio player program must not be empty",
            ));
        }
        if !config.args.iter().any(|arg| arg.contains("{audio}")) {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "audio player args must contain {audio}",
            ));
        }
        Ok(Self { config })
    }

    pub fn config(&self) -> &ProcessAudioConfig {
        &self.config
    }

    pub fn resolve_reference(&self, audio_ref: &str) -> Result<PathBuf, EngineError> {
        let relative = audio_ref.strip_prefix(AUDIO_SCHEME).ok_or_else(|| {
            engine_error(
                EngineErrorKind::InvalidRequest,
                "audio reference must use audio://",
            )
        })?;
        if relative.trim().is_empty() {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "audio reference path must not be empty",
            ));
        }

        let relative_path = Path::new(relative);
        let invalid = relative_path.is_absolute()
            || relative_path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            });
        if invalid {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "audio reference must remain under the configured root",
            ));
        }
        Ok(self.config.root.join(relative_path))
    }

    fn expanded_args(&self, path: &Path, speed_factor: f64) -> Result<Vec<String>, EngineError> {
        if !speed_factor.is_finite() || speed_factor <= 0.0 {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "audio speed_factor must be finite and positive",
            ));
        }
        let audio = path.to_string_lossy();
        let tempo = format!("{:.6}", 1.0 / speed_factor);
        Ok(self
            .config
            .args
            .iter()
            .map(|arg| arg.replace("{audio}", &audio).replace("{tempo}", &tempo))
            .collect())
    }

    fn validate_resolved_path(&self, path: &Path) -> Result<PathBuf, EngineError> {
        let root = self.config.root.canonicalize().map_err(|error| {
            engine_error(
                EngineErrorKind::Unavailable,
                format!("audio root is unavailable: {error}"),
            )
        })?;
        let resolved = path.canonicalize().map_err(|error| {
            engine_error(
                EngineErrorKind::Unavailable,
                format!("audio file is unavailable: {error}"),
            )
        })?;
        if !resolved.starts_with(&root) {
            return Err(engine_error(
                EngineErrorKind::Unauthorized,
                "audio reference resolved outside the configured root",
            ));
        }
        if !resolved.is_file() {
            return Err(engine_error(
                EngineErrorKind::Unavailable,
                "audio reference is not a regular file",
            ));
        }
        Ok(resolved)
    }
}

impl AudioPlayer for ProcessAudioPlayer {
    fn play(&self, request: &AudioPlayRequest) -> Result<(), EngineError> {
        let candidate = self.resolve_reference(&request.audio_ref)?;
        let path = self.validate_resolved_path(&candidate)?;
        let args = self.expanded_args(&path, request.speed_factor)?;
        let mut child = Command::new(&self.config.program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                engine_error(
                    EngineErrorKind::Unavailable,
                    format!("failed to start audio player: {error}"),
                )
            })?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

fn engine_error(kind: EngineErrorKind, message: impl Into<String>) -> EngineError {
    EngineError::new(kind, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("aivtuber-audio-{unique}"));
        fs::create_dir_all(&root).expect("temp root");
        root
    }

    #[test]
    fn audio_reference_is_scoped_to_root() {
        let root = temp_root();
        let player = ProcessAudioPlayer::new(ProcessAudioConfig {
            root: root.clone(),
            program: "player".to_owned(),
            args: vec!["{audio}".to_owned()],
        })
        .expect("player");

        assert_eq!(
            player
                .resolve_reference("audio://starter/reaction.opus")
                .expect("reference"),
            root.join("starter/reaction.opus")
        );
        assert!(player.resolve_reference("audio://../secret").is_err());
        assert!(player.resolve_reference("/tmp/secret").is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn command_arguments_expand_without_shell_interpolation() {
        let player = ProcessAudioPlayer::new(ProcessAudioConfig {
            root: PathBuf::from("/tmp/audio root"),
            program: "player".to_owned(),
            args: vec!["--tempo={tempo}".to_owned(), "--file={audio}".to_owned()],
        })
        .expect("player");

        let args = player
            .expanded_args(Path::new("/tmp/audio root/clip.opus"), 2.0)
            .expect("args");
        assert_eq!(args[0], "--tempo=0.500000");
        assert_eq!(args[1], "--file=/tmp/audio root/clip.opus");
    }

    #[test]
    fn player_requires_audio_placeholder() {
        let error = ProcessAudioPlayer::new(ProcessAudioConfig {
            root: PathBuf::from("."),
            program: "player".to_owned(),
            args: vec!["--quiet".to_owned()],
        })
        .expect_err("placeholder");
        assert_eq!(error.kind, EngineErrorKind::InvalidRequest);
    }
}
