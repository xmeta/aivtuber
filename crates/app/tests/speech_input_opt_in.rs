//! #397: the real `aivtuber-app` binary starts the speech sidecar only when
//! the voice-input state file says so. The protocol double stands in for the
//! sidecar, so no microphone or model is involved. `AIVTUBER_LOG=verbose`
//! makes the app print `speech_sidecar: started` when it spawns the program.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const APP: &str = env!("CARGO_BIN_EXE_aivtuber-app");
const DOUBLE: &str = env!("CARGO_BIN_EXE_speech-sidecar-double");

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-voice-input-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs the app until its stdin closes (its clean-shutdown signal) and returns
/// its stderr. `settings` is the value of the state-file variable, if any.
fn run_app(settings: Option<&std::path::Path>, log: &str, extra: &[(&str, &str)]) -> String {
    let mut command = Command::new(APP);
    static RUNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    command
        // The default pack root is relative to the repository root.
        .current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .env_clear()
        .env("AIVTUBER_MEMORY_PSEUDONYM_KEY_HEX", "11".repeat(32))
        .env(
            "AIVTUBER_CONTROL_ENDPOINT",
            format!("voice-input-test-{}-{run}", std::process::id()),
        )
        .env("AIVTUBER_LOG", log)
        .env("AIVTUBER_SPEECH_SIDECAR", DOUBLE)
        .env("AIVTUBER_SPEECH_SIDECAR_ARGS", "clock")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (name, value) in extra {
        command.env(name, value);
    }
    if let Some(path) = settings {
        command.env("AIVTUBER_SPEECH_INPUT_SETTINGS", path);
    }
    // PATH and the system root are needed by the OS loader on Windows.
    for key in ["PATH", "SystemRoot", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command.spawn().expect("spawn aivtuber-app");
    let mut stderr = child.stderr.take().unwrap();
    // The reader runs on its own thread so a full pipe never blocks the app.
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    // Give start-up (including the sidecar spawn) time, then close stdin.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    drop(child.stdin.take());
    let status = child.wait().expect("wait");
    let text = reader.join().unwrap();
    assert!(status.success(), "{status:?}\n{text}");
    text
}

#[test]
fn the_sidecar_is_not_started_when_voice_input_is_not_enabled() {
    let dir = scratch("off");
    let missing = dir.join("local-speech-input.json");
    let _ = std::fs::remove_file(&missing);
    let text = run_app(Some(&missing), "verbose", &[]);
    assert!(
        text.contains("voice input: off (microphone not opened)"),
        "{text}"
    );
    assert!(!text.contains("speech_sidecar:"), "{text}");

    let disabled = dir.join("disabled.json");
    std::fs::write(
        &disabled,
        "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":false}\n",
    )
    .unwrap();
    let text = run_app(Some(&disabled), "verbose", &[]);
    assert!(text.contains("voice input: off"), "{text}");
    assert!(!text.contains("speech_sidecar:"), "{text}");

    let garbage = dir.join("garbage.json");
    std::fs::write(&garbage, b"{\"enabled\":true}").unwrap();
    let text = run_app(Some(&garbage), "verbose", &[]);
    assert!(text.contains("voice input: off"), "{text}");
    assert!(!text.contains("speech_sidecar: started"), "{text}");
}

#[test]
fn the_sidecar_is_started_when_the_file_says_enabled() {
    let dir = scratch("on");
    let enabled = dir.join("local-speech-input.json");
    std::fs::write(
        &enabled,
        "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":true}\n",
    )
    .unwrap();
    let text = run_app(Some(&enabled), "verbose", &[]);
    assert!(
        text.contains("voice input: on (listening on the default capture device)"),
        "{text}"
    );
    assert!(text.contains("speech_sidecar: started"), "{text}");
}

#[test]
fn without_the_variable_a_configured_sidecar_starts_as_before() {
    let text = run_app(None, "verbose", &[]);
    assert!(text.contains("speech_sidecar: started"), "{text}");
    assert!(!text.contains("voice input:"), "{text}");
}

/// The environment `aivtuber-setup` writes for the speech stack: conversation,
/// floor and barge-in on, with a transcript worker and its model directory.
/// With voice input off (#400) the trackers, conversation route, floor and
/// barge-in are composed at start so it can be switched on later, but nothing
/// is spawned: no sidecar and no worker, and the app starts and shuts down
/// cleanly (typed chat is unaffected). The worker and model paths do not
/// exist: spawning them would be an error.
#[test]
fn the_setup_speech_environment_spawns_nothing_while_voice_input_is_off() {
    let dir = scratch("setup-env");
    let missing = dir.join("local-speech-input.json");
    let _ = std::fs::remove_file(&missing);
    let setup_env = [
        ("AIVTUBER_SPEECH_STT_WORKER", "no-such-stt-worker"),
        ("AIVTUBER_SPEECH_STT_MODEL_DIR", "no-such-model-dir"),
        ("AIVTUBER_SPEECH_CONVERSATION", "true"),
        ("AIVTUBER_SPEECH_FLOOR", "true"),
        ("AIVTUBER_SPEECH_BARGE_IN", "true"),
    ];
    let text = run_app(Some(&missing), "verbose", &setup_env);
    assert!(text.contains("voice input: off"), "{text}");
    for forbidden in ["speech_sidecar", "speech-stt-worker"] {
        assert!(!text.contains(forbidden), "{forbidden}: {text}");
    }
    // Composed for a later switch-on, though nothing runs.
    for composed in ["speech_floor: enabled", "speech_conversation: enabled"] {
        assert!(text.contains(composed), "{composed}: {text}");
    }
    assert!(
        text.contains("raw_ingress: stdin closed; shutting down"),
        "{text}"
    );
}

/// The voice-input line is privacy-relevant, so quiet mode prints it too, in
/// the Japanese-first form.
#[test]
fn the_voice_input_line_is_printed_under_quiet() {
    let dir = scratch("quiet");
    let missing = dir.join("local-speech-input.json");
    let _ = std::fs::remove_file(&missing);
    let text = run_app(Some(&missing), "quiet", &[]);
    assert!(
        text.contains("マイクはオフです。") && text.contains("Voice input is off"),
        "{text}"
    );
    assert!(!text.contains("voice input: off"), "{text}");
    assert!(!text.contains("speech_sidecar"), "{text}");

    let enabled = dir.join("enabled.json");
    std::fs::write(
        &enabled,
        "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":true}
",
    )
    .unwrap();
    let text = run_app(Some(&enabled), "quiet", &[]);
    assert!(text.contains("マイクで聞き取り中です"), "{text}");
}
