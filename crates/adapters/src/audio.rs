use aivtuber_domain::{EngineError, EngineErrorKind};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(all(windows, feature = "native-audio-spike"))]
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player};
#[cfg(all(windows, feature = "native-audio-spike"))]
use std::fs::File;

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

/// Largest fade a stop request may ask for (ADR-0005 S-3).
pub const MAX_AUDIO_STOP_FADE_MS: u32 = 150;

/// What an [`AudioPlayer::stop`] request did, so the caller can report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioStopOutcome {
    /// Nothing was audible or tracked.
    NothingPlaying,
    /// The volume ramps to silence over `fade_ms`, then playback ends. The
    /// call itself returned at once.
    Faded { fade_ms: u32 },
    /// Playback was ended abruptly: the backend cannot ramp (an external
    /// player process) or no fade was requested.
    HardStopped,
    /// The backend has no stop; audio already started plays out.
    Unsupported,
}

pub trait AudioPlayer: Send + Sync {
    fn play(&self, request: &AudioPlayRequest) -> Result<(), EngineError>;

    /// Stop what this player started, fading over at most `fade_ms` when the
    /// backend can. It carries no authority: callers (the app's own cancel
    /// path) decide when to call it. The default is "cannot stop".
    fn stop(&self, fade_ms: u32) -> Result<AudioStopOutcome, EngineError> {
        let _ = fade_ms;
        Ok(AudioStopOutcome::Unsupported)
    }
}

/// Playback processes still alive, so a stop can end them. Bounded.
const MAX_TRACKED_PROCESSES: usize = 16;
const REAP_POLL: Duration = Duration::from_millis(20);

#[derive(Debug)]
struct Tracked {
    id: u64,
    child: Child,
}

#[derive(Debug, Clone)]
pub struct ProcessAudioPlayer {
    config: ProcessAudioConfig,
    running: Arc<Mutex<Vec<Tracked>>>,
    next_id: Arc<AtomicU64>,
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
        Ok(Self {
            config,
            running: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn config(&self) -> &ProcessAudioConfig {
        &self.config
    }

    pub fn resolve_reference(&self, audio_ref: &str) -> Result<PathBuf, EngineError> {
        resolve_audio_reference(&self.config.root, audio_ref)
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
        validate_audio_path(&self.config.root, path)
    }

    /// Start the configured process player and return the child handle.
    ///
    /// Production `play()` detaches a waiter exactly as before. The tracked form
    /// exists so the #101 benchmark can measure process startup and stop latency
    /// without duplicating command construction or weakening path validation.
    pub fn start_tracked(&self, request: &AudioPlayRequest) -> Result<Child, EngineError> {
        let candidate = self.resolve_reference(&request.audio_ref)?;
        let path = self.validate_resolved_path(&candidate)?;
        let args = self.expanded_args(&path, request.speed_factor)?;
        Command::new(&self.config.program)
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
            })
    }
}

impl AudioPlayer for ProcessAudioPlayer {
    fn play(&self, request: &AudioPlayRequest) -> Result<(), EngineError> {
        let child = self.start_tracked(request)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut running = lock_running(&self.running);
            running.retain_mut(|tracked| matches!(tracked.child.try_wait(), Ok(None)));
            // Never more than the bound: the oldest is ended rather than
            // left running untracked.
            while running.len() >= MAX_TRACKED_PROCESSES {
                let mut oldest = running.remove(0);
                let _ = oldest.child.kill();
                reap(oldest.child);
            }
            running.push(Tracked { id, child });
        }
        // The waiter that used to own the child now only reaps it when it
        // exits on its own; a stop takes it out of the list first.
        let running = Arc::clone(&self.running);
        thread::spawn(move || {
            loop {
                thread::sleep(REAP_POLL);
                let mut running = lock_running(&running);
                let Some(position) = running.iter().position(|tracked| tracked.id == id) else {
                    return;
                };
                if !matches!(running[position].child.try_wait(), Ok(None)) {
                    running.remove(position);
                    return;
                }
            }
        });
        Ok(())
    }

    /// An external player process cannot be faded from outside: it is killed
    /// (measured 2.0 ms p50 for kill and wait, docs/audio-backend-spike.adoc)
    /// and reaped off-thread, and the outcome says so.
    fn stop(&self, fade_ms: u32) -> Result<AudioStopOutcome, EngineError> {
        let _ = fade_ms;
        let taken: Vec<Tracked> = std::mem::take(&mut *lock_running(&self.running));
        let mut ended = false;
        for mut tracked in taken {
            if matches!(tracked.child.try_wait(), Ok(None)) {
                let _ = tracked.child.kill();
                ended = true;
            }
            reap(tracked.child);
        }
        Ok(if ended {
            AudioStopOutcome::HardStopped
        } else {
            AudioStopOutcome::NothingPlaying
        })
    }
}

fn lock_running(running: &Mutex<Vec<Tracked>>) -> std::sync::MutexGuard<'_, Vec<Tracked>> {
    running.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn reap(mut child: Child) {
    thread::spawn(move || {
        let _ = child.wait();
    });
}

#[cfg(all(windows, feature = "native-audio-spike"))]
pub struct RodioAudioPlayer {
    root: PathBuf,
    _stream: MixerDeviceSink,
    current: Mutex<Option<Player>>,
}

#[cfg(all(windows, feature = "native-audio-spike"))]
impl std::fmt::Debug for RodioAudioPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RodioAudioPlayer")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

#[cfg(all(windows, feature = "native-audio-spike"))]
impl RodioAudioPlayer {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, EngineError> {
        let root = root.into();
        if root.as_os_str().is_empty() {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "audio root must not be empty",
            ));
        }
        let stream = DeviceSinkBuilder::open_default_sink().map_err(|error| {
            engine_error(
                EngineErrorKind::Unavailable,
                format!("failed to open default native audio stream: {error}"),
            )
        })?;
        Ok(Self {
            root,
            _stream: stream,
            current: Mutex::new(None),
        })
    }

    pub fn resolve_reference(&self, audio_ref: &str) -> Result<PathBuf, EngineError> {
        resolve_audio_reference(&self.root, audio_ref)
    }

    pub fn stop(&self) {
        if let Ok(mut current) = self.current.lock()
            && let Some(sink) = current.take()
        {
            sink.stop();
        }
    }

    pub fn is_idle(&self) -> bool {
        self.current
            .lock()
            .map(|current| current.as_ref().is_none_or(Player::empty))
            .unwrap_or(true)
    }
}

#[cfg(all(windows, feature = "native-audio-spike"))]
impl AudioPlayer for RodioAudioPlayer {
    fn play(&self, request: &AudioPlayRequest) -> Result<(), EngineError> {
        if !request.speed_factor.is_finite() || request.speed_factor <= 0.0 {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "audio speed_factor must be finite and positive",
            ));
        }
        let candidate = self.resolve_reference(&request.audio_ref)?;
        let path = validate_audio_path(&self.root, &candidate)?;
        let file = File::open(&path).map_err(|error| {
            engine_error(
                EngineErrorKind::Unavailable,
                format!("failed to open native audio file: {error}"),
            )
        })?;
        let source = Decoder::try_from(file).map_err(|error| {
            engine_error(
                EngineErrorKind::InvalidRequest,
                format!("failed to decode native audio file: {error}"),
            )
        })?;
        let sink = Player::connect_new(self._stream.mixer());
        sink.set_speed((1.0 / request.speed_factor) as f32);
        sink.append(source);

        let mut current = self.current.lock().map_err(|_| {
            engine_error(
                EngineErrorKind::Unavailable,
                "native audio state lock was poisoned",
            )
        })?;
        if let Some(previous) = current.replace(sink) {
            previous.stop();
        }
        Ok(())
    }

    /// A real fade: the volume ramps linearly to zero in 10 ms steps on a
    /// short-lived thread, then the player stops. The call returns at once;
    /// a new `play` is independent of the fading player.
    fn stop(&self, fade_ms: u32) -> Result<AudioStopOutcome, EngineError> {
        let player = {
            let mut current = self.current.lock().map_err(|_| {
                engine_error(
                    EngineErrorKind::Unavailable,
                    "native audio state lock was poisoned",
                )
            })?;
            current.take()
        };
        let Some(player) = player else {
            return Ok(AudioStopOutcome::NothingPlaying);
        };
        if player.empty() {
            player.stop();
            return Ok(AudioStopOutcome::NothingPlaying);
        }
        let fade_ms = fade_ms.min(MAX_AUDIO_STOP_FADE_MS);
        if fade_ms == 0 {
            player.stop();
            return Ok(AudioStopOutcome::HardStopped);
        }
        thread::spawn(move || {
            const STEP_MS: u32 = 10;
            let steps = (fade_ms / STEP_MS).max(1);
            let start = player.volume();
            for step in 1..=steps {
                thread::sleep(Duration::from_millis(u64::from(fade_ms / steps)));
                player.set_volume(start * (1.0 - step as f32 / steps as f32));
            }
            player.stop();
        });
        Ok(AudioStopOutcome::Faded { fade_ms })
    }
}

fn resolve_audio_reference(root: &Path, audio_ref: &str) -> Result<PathBuf, EngineError> {
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
    Ok(root.join(relative_path))
}

fn validate_audio_path(root: &Path, path: &Path) -> Result<PathBuf, EngineError> {
    let root = root.canonicalize().map_err(|error| {
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
