//! #400: voice input switched on and off while the app runs. The protocol
//! double stands in for the sidecar, so no microphone or model is involved.
//! The double can hold an exclusive file lock for as long as its process lives
//! (its second argument), which is how these tests tell "running" from "gone"
//! on every platform.

use aivtuber_app::{
    SpeechInputController, SpeechInputError, SpeechInputStateName, SpeechSidecarConfig,
};
use aivtuber_runtime::{
    IngressPolicy, SpeechObservationConfig, SpeechObservationTracker, SpeechTranscriptConfig,
    SpeechTranscriptTracker,
};
use std::cell::RefCell;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const APP: &str = env!("CARGO_BIN_EXE_aivtuber-app");
const CTL: &str = env!("CARGO_BIN_EXE_aivtuberctl");
const DOUBLE: &str = env!("CARGO_BIN_EXE_speech-sidecar-double");

const ON: &str = "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":true}\n";
const OFF: &str = "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":false}\n";
const SECRET: &str = "0123456789abcdef0123456789abcdef";

fn scratch(name: &str) -> PathBuf {
    static RUNS: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-voice-live-{}-{}-{name}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Whether a process holds the lock the double takes: true while it runs.
fn double_alive(lock: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock)
    else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}

fn wait_for(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn trackers() -> (SpeechObservationTracker, SpeechTranscriptTracker) {
    (
        SpeechObservationTracker::new(
            SpeechObservationConfig {
                max_age_ms: 1_000,
                max_future_skew_ms: 1_000,
                ..SpeechObservationConfig::default()
            },
            IngressPolicy::default(),
        )
        .unwrap(),
        SpeechTranscriptTracker::new(SpeechTranscriptConfig::default(), IngressPolicy::default())
            .unwrap(),
    )
}

fn controller(
    program: &str,
    mode: &str,
    lock: &Path,
    file: &Path,
    started: Instant,
) -> SpeechInputController {
    let mut config = SpeechSidecarConfig::new(
        program,
        vec![mode.to_owned(), lock.to_string_lossy().into_owned()],
    );
    config.resync_interval = Duration::from_millis(250);
    SpeechInputController::new(config, started, Some(file.to_path_buf()), true)
}

fn poll_until(
    input: &mut SpeechInputController,
    observations: &mut SpeechObservationTracker,
    transcripts: &mut SpeechTranscriptTracker,
    started: Instant,
    what: &str,
    mut done: impl FnMut(&SpeechObservationTracker, &SpeechInputController) -> bool,
) {
    wait_for(what, Duration::from_secs(10), || {
        let now_ms = started.elapsed().as_millis() as u64;
        input.poll(observations, Some(&mut *transcripts), now_ms);
        done(observations, input)
    });
}

#[test]
fn the_controller_starts_stops_and_restarts_a_fresh_sidecar() {
    let dir = scratch("controller");
    let (lock, file) = (dir.join("alive.lock"), dir.join("local-speech-input.json"));
    let started = Instant::now();
    let mut input = controller(DOUBLE, "clock", &lock, &file, started);
    let (mut observations, mut transcripts) = trackers();
    let announced = RefCell::new(Vec::new());
    let announce = |on: bool, remote: bool| announced.borrow_mut().push((on, remote));

    let status = input.status();
    assert_eq!(
        (status.composed, status.state, status.thinking_remote),
        (true, SpeechInputStateName::Off, true)
    );
    assert!(!double_alive(&lock) && !file.exists());

    // Enable: the double runs, the choice is written after the spawn, the
    // line is announced once (with the cloud flag).
    let detail = input.enable(announce).expect("enable");
    assert_eq!(detail, "voice input is on");
    wait_for("the double to run", Duration::from_secs(10), || {
        double_alive(&lock)
    });
    assert_eq!(std::fs::read_to_string(&file).unwrap(), ON);
    assert_eq!(*announced.borrow(), [(true, true)]);
    poll_until(
        &mut input,
        &mut observations,
        &mut transcripts,
        started,
        "the first observations",
        |tracker, _| tracker.metrics().observations_accepted >= 3,
    );
    assert_eq!(input.status().state, SpeechInputStateName::On);

    // Already on: a no-op, nothing announced again, the same process.
    let detail = input.enable(announce).expect("enable again");
    assert_eq!(detail, "voice input is already on");
    assert_eq!(announced.borrow().len(), 1);

    // Disable: the process is gone before the call returns.
    let detail = input.disable(announce);
    assert_eq!(detail, "voice input is off");
    assert!(!double_alive(&lock), "the double outlived the disable");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), OFF);
    assert_eq!(*announced.borrow(), [(true, true), (false, true)]);
    assert_eq!(input.status().state, SpeechInputStateName::Off);
    assert!(input.stats().is_none(), "no sidecar, no counters");

    // Re-enable after the app cleared its trackers: a fresh instance whose
    // observations are accepted, not judged against the old one.
    observations.clear();
    transcripts.clear();
    let before = observations.metrics();
    input.enable(announce).expect("re-enable");
    wait_for("the new double to run", Duration::from_secs(10), || {
        double_alive(&lock)
    });
    assert_eq!(std::fs::read_to_string(&file).unwrap(), ON);
    poll_until(
        &mut input,
        &mut observations,
        &mut transcripts,
        started,
        "the new instance's observations",
        |tracker, _| tracker.metrics().observations_accepted >= before.observations_accepted + 3,
    );
    let after = observations.metrics();
    assert_eq!(after.rejected_total(), 0, "{after:?}");
    assert_eq!(after.provider_unavailable, 0);
    assert!(input.stats().unwrap().batches_ingested >= 3);

    // The orderly end of the app stops the child and changes nothing else.
    input.shutdown();
    assert!(!double_alive(&lock));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), ON);
}

#[test]
fn a_sidecar_that_cannot_start_is_a_typed_failure_and_the_state_stays_off() {
    let dir = scratch("nospawn");
    let (lock, file) = (dir.join("alive.lock"), dir.join("local-speech-input.json"));
    let mut input = controller(
        "definitely-not-a-real-speech-sidecar",
        "clock",
        &lock,
        &file,
        Instant::now(),
    );
    let announced = RefCell::new(Vec::new());
    let error = input
        .enable(|on, remote| announced.borrow_mut().push((on, remote)))
        .expect_err("no such program");
    assert!(matches!(error, SpeechInputError::Spawn(_)), "{error}");
    assert!(error.to_string().contains("could not start"));
    assert!(
        !error.to_string().contains("definitely-not"),
        "the path is not echoed: {error}"
    );
    assert_eq!(input.status().state, SpeechInputStateName::Off);
    assert!(!file.exists(), "enabled is written only after a spawn");
    assert!(announced.borrow().is_empty());

    // Disable still succeeds, and writes the choice.
    assert_eq!(input.disable(|_, _| {}), "voice input is off");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), OFF);
}

#[test]
fn a_later_failure_is_reported_and_does_not_rewrite_the_file() {
    let dir = scratch("failed");
    let (lock, file) = (dir.join("alive.lock"), dir.join("local-speech-input.json"));
    let started = Instant::now();
    let mut input = controller(DOUBLE, "crash", &lock, &file, started);
    let (mut observations, mut transcripts) = trackers();
    input.enable(|_, _| {}).expect("enable");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), ON);
    poll_until(
        &mut input,
        &mut observations,
        &mut transcripts,
        started,
        "the double to crash",
        |_, input| input.status().state == SpeechInputStateName::Failed,
    );
    let status = input.status();
    let reason = status.reason.expect("a reason");
    assert!(reason.contains("exited"), "{reason}");
    assert!(reason.chars().count() <= 200 && !reason.contains('\n'));
    assert_eq!(observations.metrics().provider_unavailable, 1);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        ON,
        "a failure does not rewrite the choice"
    );
    // Enabling again from the failed state starts a fresh instance; disable
    // from the failed state always works.
    input.enable(|_, _| {}).expect("enable from failed");
    assert_eq!(input.status().state, SpeechInputStateName::On);
    input.disable(|_, _| {});
    assert_eq!(input.status().state, SpeechInputStateName::Off);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), OFF);
}

/// Disabling kills the sidecar without any chance for it to clean up. Its own
/// child (the STT worker in production) must still end: here the double's
/// stand-in worker ends at end of input on the pipe it shares with its parent,
/// which is how the real worker ends (`WorkerExit::InputClosed`). This pins the
/// operating-system behaviour for the platform the tests run on; it does not
/// run the real worker.
#[test]
fn a_worker_child_ends_when_the_killed_sidecar_s_pipes_close() {
    let dir = scratch("worker");
    let (lock, file) = (dir.join("alive.lock"), dir.join("local-speech-input.json"));
    let worker_lock = dir.join("alive.lock.worker");
    let mut input = controller(DOUBLE, "with-worker", &lock, &file, Instant::now());
    input.enable(|_, _| {}).expect("enable");
    wait_for(
        "the sidecar and its worker",
        Duration::from_secs(10),
        || double_alive(&lock) && double_alive(&worker_lock),
    );
    input.disable(|_, _| {});
    assert!(!double_alive(&lock), "the sidecar is gone at once");
    wait_for("the worker to end", Duration::from_secs(10), || {
        !double_alive(&worker_lock)
    });
}

/// The real `aivtuber-app` with the double as its sidecar, driven the way an
/// operator does: `aivtuberctl voice-input on|off`.
struct RunningApp {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<String>>,
    reader: Option<std::thread::JoinHandle<()>>,
    endpoint: String,
}

impl RunningApp {
    fn start(program: &str, file: &Path, lock: &Path, log: &str) -> Self {
        static RUNS: AtomicU32 = AtomicU32::new(0);
        let endpoint = format!(
            "voice-live-test-{}-{}",
            std::process::id(),
            RUNS.fetch_add(1, Ordering::Relaxed)
        );
        let mut command = Command::new(APP);
        command
            // The default pack root is relative to the repository root.
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .env_clear()
            .env("AIVTUBER_MEMORY_PSEUDONYM_KEY_HEX", "11".repeat(32))
            .env("AIVTUBER_CONTROL_ENDPOINT", &endpoint)
            .env("AIVTUBER_CONTROL_SECRET", SECRET)
            .env("AIVTUBER_LOG", log)
            .env("AIVTUBER_SPEECH_SIDECAR", program)
            .env(
                "AIVTUBER_SPEECH_SIDECAR_ARGS",
                format!("clock {}", lock.display()),
            )
            .env("AIVTUBER_SPEECH_INPUT_SETTINGS", file)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for key in ["PATH", "SystemRoot", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut child = command.spawn().expect("spawn aivtuber-app");
        let stdin = child.stdin.take();
        let mut pipe = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = stderr.clone();
        let reader = std::thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            while let Ok(n) = pipe.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buffer[..n]));
            }
        });
        let app = Self {
            child,
            stdin,
            stderr,
            reader: Some(reader),
            endpoint,
        };
        wait_for("the control endpoint", Duration::from_secs(30), || {
            app.ctl(&["status"]).0
        });
        app
    }

    /// `aivtuberctl <args>`: success and its stdout. The endpoint's rate
    /// limit (16 a minute, burst 8) is real, so a limited call waits and
    /// tries again.
    fn ctl(&self, args: &[&str]) -> (bool, String) {
        for _ in 0..30 {
            let (ok, text) = self.ctl_once(args);
            if ok || !text.contains("rate limited") {
                return (ok, text);
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        panic!("still rate limited: {args:?}");
    }

    fn ctl_once(&self, args: &[&str]) -> (bool, String) {
        let output = Command::new(CTL)
            .env_clear()
            .env("AIVTUBER_CONTROL_ENDPOINT", &self.endpoint)
            .env("AIVTUBER_CONTROL_SECRET", SECRET)
            .envs(
                ["PATH", "SystemRoot", "TEMP", "TMP"]
                    .iter()
                    .filter_map(|key| std::env::var_os(key).map(|value| (*key, value))),
            )
            .args(args)
            .output()
            .expect("run aivtuberctl");
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    }

    fn voice_input_line(&self) -> String {
        let (ok, text) = self.ctl(&["status"]);
        assert!(ok, "{text}");
        text.lines()
            .find(|line| line.starts_with("voice_input "))
            .unwrap_or_else(|| panic!("no voice_input line:\n{text}"))
            .to_owned()
    }

    fn log(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Close stdin (the app's clean shutdown signal) and wait for it to end.
    fn finish(mut self) -> String {
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait");
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let text = self.stderr.lock().unwrap().clone();
        assert!(status.success(), "{status:?}\n{text}");
        text
    }
}

impl Drop for RunningApp {
    fn drop(&mut self) {
        // A test that failed halfway must not leave the app behind.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn open_lock(path: &Path) -> File {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .unwrap()
}

#[test]
fn the_panel_and_aivtuberctl_switch_the_real_app_five_times_without_leftovers() {
    let dir = scratch("app");
    let (lock, file) = (dir.join("alive.lock"), dir.join("local-speech-input.json"));
    drop(open_lock(&lock));
    let app = RunningApp::start(DOUBLE, &file, &lock, "verbose");
    assert!(app.log().contains("voice input: off"), "{}", app.log());
    assert!(!double_alive(&lock), "off at start opens nothing");
    assert_eq!(
        app.voice_input_line(),
        "voice_input composed=true state=Off thinking_remote=false reason=None"
    );

    for round in 1..=5 {
        let (ok, text) = app.ctl(&["voice-input", "on"]);
        assert!(ok, "round {round}: {text}");
        wait_for("the double to run", Duration::from_secs(10), || {
            double_alive(&lock)
        });
        assert_eq!(std::fs::read_to_string(&file).unwrap(), ON);
        if round == 1 {
            assert!(app.voice_input_line().contains("state=On"));
            // Already on is a no-op.
            assert!(app.ctl(&["voice-input", "on"]).0);
        }

        let (ok, text) = app.ctl(&["voice-input", "off"]);
        assert!(ok, "round {round}: {text}");
        assert!(
            !double_alive(&lock),
            "round {round}: the double outlived the off request"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), OFF);
    }
    assert!(app.voice_input_line().contains("state=Off"));
    let log = app.log();
    assert_eq!(log.matches("voice input: on (").count(), 5, "{log}");
    assert_eq!(log.matches("voice input: off (").count(), 6, "{log}");

    // The choice survives a restart: leave it on, end the app, start again.
    assert!(app.ctl(&["voice-input", "on"]).0);
    wait_for("the double to run", Duration::from_secs(10), || {
        double_alive(&lock)
    });
    let log = app.finish();
    assert!(log.contains("raw_ingress: stdin closed"), "{log}");
    assert!(!double_alive(&lock), "ending the app ends the sidecar");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), ON);

    let app = RunningApp::start(DOUBLE, &file, &lock, "verbose");
    wait_for(
        "the double to run at start",
        Duration::from_secs(10),
        || double_alive(&lock),
    );
    assert!(app.voice_input_line().contains("state=On"));
    assert!(app.log().contains("voice input: on"), "{}", app.log());
    assert!(app.ctl(&["voice-input", "off"]).0);
    assert!(!double_alive(&lock));
    app.finish();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), OFF);
}

#[test]
fn a_missing_sidecar_program_is_refused_over_the_control_endpoint_and_stays_off() {
    let dir = scratch("app-missing");
    let (lock, file) = (dir.join("alive.lock"), dir.join("local-speech-input.json"));
    let app = RunningApp::start(
        "definitely-not-a-real-speech-sidecar",
        &file,
        &lock,
        "verbose",
    );
    let (ok, text) = app.ctl(&["voice-input", "on"]);
    assert!(!ok, "{text}");
    assert!(text.contains("voice input could not start"), "{text}");
    assert!(!file.exists(), "nothing was written as enabled");
    assert!(app.voice_input_line().contains("state=Off"));
    // Off still works from there.
    assert!(app.ctl(&["voice-input", "off"]).0);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), OFF);
    app.finish();
}
