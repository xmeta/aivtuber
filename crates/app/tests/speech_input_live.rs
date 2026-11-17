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

fn device_controller(lock: &Path, file: &Path, started: Instant) -> SpeechInputController {
    controller(DOUBLE, "devices", lock, file, started)
}

fn log_lines(lock: &Path) -> Vec<String> {
    let mut log = lock.as_os_str().to_owned();
    log.push(".log");
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// #396: the sidecar reports its devices and the opened one; choosing another
/// stops the old sidecar and starts a new one on it (the double holds an
/// exclusive lock, so a second instance could not even report while the first
/// lived), and the choice is saved with the rest of the file.
#[test]
fn the_capture_device_is_reported_and_a_switch_restarts_the_sidecar_without_a_leftover() {
    let dir = scratch("device-switch");
    let (lock, file) = (dir.join("alive.lock"), dir.join("local-speech-input.json"));
    let started = Instant::now();
    let mut input = device_controller(&lock, &file, started);
    let (mut observations, mut transcripts) = trackers();
    let announce = |_: bool, _: bool| {};
    input.enable(announce).expect("enable");
    poll_until(
        &mut input,
        &mut observations,
        &mut transcripts,
        started,
        "the device report",
        |_, input| input.status().device.is_some(),
    );
    let status = input.status();
    assert_eq!(status.device.as_deref(), Some("Mic A"));
    assert_eq!(status.devices, ["Mic A", "Mic B", "Line (AG06)"]);
    assert_eq!(status.selected_device, None);
    assert_eq!(log_lines(&lock).len(), 1);

    // A device nobody listed is refused; nothing restarts.
    assert!(matches!(
        input.set_device(Some("Nope".to_owned()), announce),
        Err(SpeechInputError::Device(_))
    ));
    assert_eq!(log_lines(&lock).len(), 1);
    assert_eq!(input.status().selected_device, None);

    let detail = input
        .set_device(Some("Line (AG06)".to_owned()), announce)
        .expect("switch");
    assert_eq!(detail, "capture device changed");
    assert!(
        !detail.contains("AG06"),
        "the answer never carries the name"
    );
    poll_until(
        &mut input,
        &mut observations,
        &mut transcripts,
        started,
        "the new device report",
        |_, input| input.status().device.as_deref() == Some("Line (AG06)"),
    );
    let log = log_lines(&lock);
    assert_eq!(log.len(), 2, "{log:?}");
    let pids: Vec<&str> = log.iter().filter_map(|l| l.split(' ').next()).collect();
    assert_ne!(pids[0], pids[1], "a fresh process");
    assert!(log[1].ends_with("Line (AG06)"), "{log:?}");
    assert!(double_alive(&lock), "exactly the new one runs");
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(
        saved.contains("\"enabled\":true") && saved.contains("Line (AG06)"),
        "{saved}"
    );

    // Back to the system default restarts again and drops the name.
    input.set_device(None, announce).expect("default");
    poll_until(
        &mut input,
        &mut observations,
        &mut transcripts,
        started,
        "the default device report",
        |_, input| input.status().device.as_deref() == Some("Mic A"),
    );
    assert_eq!(log_lines(&lock).len(), 3);
    assert!(!std::fs::read_to_string(&file).unwrap().contains("device"));

    // Off: the process is gone, and the choice is remembered.
    input
        .set_device(Some("Mic B".to_owned()), announce)
        .expect("mic b");
    input.disable(announce);
    let spawned = log_lines(&lock).len();
    assert!(!double_alive(&lock), "the sidecar outlived the disable");
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(
        saved.contains("\"enabled\":false") && saved.contains("Mic B"),
        "{saved}"
    );
    // The list is still offered while it is off; the opened device is not.
    let status = input.status();
    assert_eq!(status.devices.len(), 3);
    assert_eq!(status.device, None);
    // Choosing while off only remembers: nothing starts.
    input
        .set_device(Some("Mic A".to_owned()), announce)
        .expect("off");
    assert!(!double_alive(&lock));
    assert_eq!(log_lines(&lock).len(), spawned, "nothing started while off");
}

/// #396: a remembered device that does not exist keeps voice input off with a
/// reason that names the devices that do; choosing a listed one recovers.
#[test]
fn an_unknown_remembered_device_fails_clearly_and_a_listed_one_recovers() {
    let dir = scratch("device-unknown");
    let (lock, file) = (dir.join("alive.lock"), dir.join("local-speech-input.json"));
    let started = Instant::now();
    let mut input = device_controller(&lock, &file, started).with_settings(
        aivtuber_domain::SpeechInputSettings {
            enabled: true,
            device: Some("Missing Mic".to_owned()),
            ..aivtuber_domain::SpeechInputSettings::default()
        },
    );
    let (mut observations, mut transcripts) = trackers();
    let announce = |_: bool, _: bool| {};
    input.start_initial().expect("the process starts");
    poll_until(
        &mut input,
        &mut observations,
        &mut transcripts,
        started,
        "the failure",
        |_, input| input.status().state == SpeechInputStateName::Failed,
    );
    let status = input.status();
    let reason = status.reason.expect("a reason");
    assert!(reason.contains("Missing Mic"), "{reason}");
    assert!(
        reason.contains("Mic A") && reason.contains("Line (AG06)"),
        "{reason}"
    );
    assert!(
        reason.contains("見つかりません") && reason.contains("not found"),
        "{reason}"
    );
    assert_eq!(status.device, None);
    assert_eq!(status.devices.len(), 3, "the list survives the exit");
    assert!(!double_alive(&lock), "no process is left");
    // Still failed after more polling (no restart), and the file is untouched.
    input.poll(&mut observations, Some(&mut transcripts), 5);
    assert_eq!(input.status().state, SpeechInputStateName::Failed);
    assert!(!file.exists());

    // Choosing a listed device starts a working one and saves the choice.
    input
        .set_device(Some("Mic B".to_owned()), announce)
        .expect("recover");
    poll_until(
        &mut input,
        &mut observations,
        &mut transcripts,
        started,
        "the recovered device",
        |_, input| input.status().device.as_deref() == Some("Mic B"),
    );
    assert_eq!(input.status().state, SpeechInputStateName::On);
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("\"enabled\":true")
    );
    input.shutdown();
    assert!(!double_alive(&lock));
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
        let name = format!(
            "voice-live-test-{}-{}",
            std::process::id(),
            RUNS.fetch_add(1, Ordering::Relaxed)
        );
        // Windows: a named pipe in the namespace, the bare name. Unix: the
        // endpoint is a filesystem path, so a bare name would be resolved
        // against each process's own working directory (the app runs from the
        // repository root, `aivtuberctl` from elsewhere) and never meet. Use
        // one absolute path, short enough for a socket address.
        let endpoint = if cfg!(unix) {
            let path = file
                .parent()
                .expect("state file directory")
                .join(format!("{name}.sock"));
            let _ = std::fs::remove_file(&path);
            path.to_string_lossy().into_owned()
        } else {
            name
        };
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
        let mut app = Self {
            child,
            stdin,
            stderr,
            reader: Some(reader),
            endpoint,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !app.ctl_once(&["status"]).0 {
            let exited = app.child_exited();
            if Instant::now() >= deadline || exited {
                let log = app.log();
                let tail: String = log
                    .chars()
                    .rev()
                    .take(4000)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                panic!(
                    "the control endpoint {} did not become reachable (app exited: {exited}); app stderr (last 4000 chars):
{tail}",
                    app.endpoint
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
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

    fn child_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
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
        "voice_input composed=true state=Off thinking_remote=false reason=None duplex=Half dropped_during_playback=0 device=None selected_device=None"
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

/// #396 over the real control endpoint: the duplex and device choices are
/// accepted, saved whole beside `enabled`, and shown in `status`.
#[test]
fn aivtuberctl_chooses_the_device_and_duplex_and_the_choices_are_saved() {
    let dir = scratch("app-choices");
    let (lock, file) = (dir.join("alive.lock"), dir.join("local-speech-input.json"));
    let app = RunningApp::start(DOUBLE, &file, &lock, "verbose");
    let (ok, text) = app.ctl(&["voice-input", "duplex", "full"]);
    assert!(ok, "{text}");
    let (ok, text) = app.ctl(&["voice-input", "device", "Mic B"]);
    assert!(ok, "{text}");
    assert!(
        !text.contains("Mic B"),
        "the answer never carries the name: {text}"
    );
    let line = app.voice_input_line();
    assert!(
        line.contains("duplex=Full") && line.contains("selected_device=Some(\"Mic B\")"),
        "{line}"
    );
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(
        saved.contains("\"device\":\"Mic B\"") && saved.contains("\"duplex\":\"full\""),
        "{saved}"
    );
    // The audit line of the endpoint does not carry the name either.
    assert!(!app.log().contains("Mic B"), "{}", app.log());
    let (ok, text) = app.ctl(&["voice-input", "device", "--default"]);
    assert!(ok, "{text}");
    assert!(!std::fs::read_to_string(&file).unwrap().contains("device"));
    assert!(app.voice_input_line().contains("selected_device=None"));
    let (ok, _) = app.ctl(&["voice-input", "duplex", "both"]);
    assert!(!ok, "clap refuses it before anything is sent");
    app.finish();
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
