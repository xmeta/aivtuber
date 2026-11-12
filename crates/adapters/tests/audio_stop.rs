//! Issue #81 S2: `AudioPlayer::stop` on the process backend, with a real child
//! process and no audio device (the "player" is a script that sleeps).

use aivtuber_adapters::{
    AudioPlayRequest, AudioPlayer, AudioStopOutcome, ProcessAudioConfig, ProcessAudioPlayer,
};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn player(name: &str, body_unix: &str, body_windows: &str) -> ProcessAudioPlayer {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("audio-stop-{name}"));
    fs::create_dir_all(&root).expect("root");
    fs::write(root.join("clip.opus"), b"not audio").expect("clip");
    let script = if cfg!(windows) {
        let path = root.join("player.cmd");
        fs::write(&path, body_windows).expect("script");
        path
    } else {
        let path = root.join("player.sh");
        fs::write(&path, body_unix).expect("script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path
    };
    ProcessAudioPlayer::new(ProcessAudioConfig {
        root,
        program: script.to_string_lossy().into_owned(),
        args: vec!["{audio}".to_owned()],
    })
    .expect("player")
}

fn request() -> AudioPlayRequest {
    AudioPlayRequest {
        audio_ref: "audio://clip.opus".to_owned(),
        speed_factor: 1.0,
        continuation: false,
    }
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// An external player cannot be faded from outside: stop kills it, says so,
/// and returns at once (the child is reaped off-thread).
#[test]
fn stop_ends_a_running_player_process_and_reports_a_hard_stop() {
    let player = player(
        "running",
        "#!/bin/sh\nsleep 30\n",
        "@ping -n 30 127.0.0.1 >nul\r\n",
    );
    player.play(&request()).expect("play");
    assert_eq!(player.tracked_processes(), 1);

    let started = Instant::now();
    let outcome = AudioPlayer::stop(&player, 120).expect("stop");
    let took = started.elapsed();
    eprintln!("process_audio_stop: call_micros={}", took.as_micros());
    assert_eq!(outcome, AudioStopOutcome::HardStopped);
    assert!(took < Duration::from_millis(150), "{took:?}");
    assert_eq!(player.tracked_processes(), 0);
    assert_eq!(
        AudioPlayer::stop(&player, 120).expect("again"),
        AudioStopOutcome::NothingPlaying
    );
}

/// A player that already finished is not stopped and not reported as stopped,
/// and finished processes do not pile up in the tracking list.
#[test]
fn a_finished_player_is_forgotten_and_stop_says_nothing_was_playing() {
    let player = player("finished", "#!/bin/sh\nexit 0\n", "@exit /b 0\r\n");
    for _ in 0..3 {
        player.play(&request()).expect("play");
    }
    assert!(
        wait_until(Duration::from_secs(10), || player.tracked_processes() == 0),
        "finished processes are reaped"
    );
    assert_eq!(
        player.stop(0).expect("stop"),
        AudioStopOutcome::NothingPlaying
    );
}

/// A backend with no stop says so instead of pretending.
#[test]
fn the_default_stop_is_unsupported() {
    struct NoStop;
    impl AudioPlayer for NoStop {
        fn play(&self, _request: &AudioPlayRequest) -> Result<(), aivtuber_domain::EngineError> {
            Ok(())
        }
    }
    assert_eq!(
        NoStop.stop(100).expect("stop"),
        AudioStopOutcome::Unsupported
    );
}

/// Native backend fade (Windows, `native-audio-spike`): needs a real output
/// device, so it is opt-in (`--ignored`). Plays 2 s of silent PCM.
#[cfg(feature = "native-audio-spike")]
#[test]
#[ignore = "needs a default audio output device"]
fn the_native_backend_fades_and_returns_at_once() {
    use aivtuber_adapters::RodioAudioPlayer;
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("audio-stop-native");
    fs::create_dir_all(&root).expect("root");
    let data_bytes = 48_000_u32 * 2 * 2;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&48_000_u32.to_le_bytes());
    wav.extend_from_slice(&96_000_u32.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    wav.extend(std::iter::repeat_n(0_u8, data_bytes as usize));
    fs::write(root.join("silence.wav"), wav).expect("wav");
    let player = RodioAudioPlayer::new(&root).expect("device");
    let play = || {
        player
            .play(&AudioPlayRequest {
                audio_ref: "audio://silence.wav".to_owned(),
                speed_factor: 1.0,
                continuation: false,
            })
            .expect("play")
    };
    play();
    let started = Instant::now();
    let outcome = AudioPlayer::stop(&player, 120).expect("stop");
    eprintln!(
        "native_audio_stop: call_micros={}",
        started.elapsed().as_micros()
    );
    assert_eq!(outcome, AudioStopOutcome::Faded { fade_ms: 120 });
    assert_eq!(
        AudioPlayer::stop(&player, 120).expect("again"),
        AudioStopOutcome::NothingPlaying
    );
    // Over-long fades are bounded, zero is a hard stop.
    play();
    assert_eq!(
        AudioPlayer::stop(&player, 9_999).expect("clamped"),
        AudioStopOutcome::Faded { fade_ms: 150 }
    );
    play();
    assert_eq!(
        AudioPlayer::stop(&player, 0).expect("hard"),
        AudioStopOutcome::HardStopped
    );
}
