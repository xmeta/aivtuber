//! #463: the real `aivtuber-app` binary with the session log remembered as on
//! writes one pseudonymised JSON Lines file for a chat line fed on stdin, and
//! `aivtuberctl log summary` reads it offline (no daemon, no secret). With the
//! log off (no remembered choice) the same run creates no directory.

#[path = "../src/test_dir.rs"]
mod test_dir;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const APP: &str = env!("CARGO_BIN_EXE_aivtuber-app");
const CTL: &str = env!("CARGO_BIN_EXE_aivtuberctl");

fn scratch(name: &str) -> PathBuf {
    test_dir::scratch_for_test_thread(&format!("aivtuber-session-log-{name}"))
}

/// Runs the app, feeds `line` on stdin, waits, closes stdin (its clean
/// shutdown signal) and returns its stderr.
fn run_app(dir: &Path, remembered: Option<bool>, line: &str) -> String {
    let settings = dir.join("local-session-log.json");
    if let Some(enabled) = remembered {
        std::fs::write(
            &settings,
            format!("{{\"schema\":\"local-session-log-settings/1\",\"enabled\":{enabled}}}\n"),
        )
        .unwrap();
    }
    let mut command = Command::new(APP);
    command
        // The default pack root is relative to the repository root.
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .env_clear()
        .env("AIVTUBER_MEMORY_PSEUDONYM_KEY_HEX", "11".repeat(32))
        .env(
            "AIVTUBER_CONTROL_ENDPOINT",
            format!(
                "session-log-test-{}-{}",
                std::process::id(),
                dir.display().to_string().len()
            ),
        )
        .env(
            "AIVTUBER_SESSION_LOG_DIR",
            dir.join("logs").join("sessions"),
        )
        .env("AIVTUBER_SESSION_LOG_SETTINGS", &settings)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // PATH and the system root are needed by the OS loader on Windows.
    for key in ["PATH", "SystemRoot", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command.spawn().expect("spawn aivtuber-app");
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{line}").unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2500));
    drop(stdin);
    let status = child.wait().expect("wait");
    let text = reader.join().unwrap();
    assert!(status.success(), "{status:?}\n{text}");
    text
}

fn chat_line() -> String {
    let raw = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/events/chat-message.json"),
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    serde_json::to_string(&value).unwrap()
}

#[test]
fn a_remembered_on_writes_a_pseudonymised_file_that_the_summary_reads() {
    let dir = scratch("on");
    let stderr = run_app(&dir, Some(true), &chat_line());
    assert!(stderr.contains("session log: on"), "{stderr}");
    let files: Vec<_> = std::fs::read_dir(dir.join("logs").join("sessions"))
        .expect("the directory is created when the log is on")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let text = std::fs::read_to_string(&files[0]).unwrap();
    assert!(text.contains("\"t\":\"session\""), "{text}");
    assert!(text.contains("\"t\":\"input\""), "{text}");
    assert!(text.contains("viewer-01"), "{text}");
    assert!(
        !text.contains("viewer:8f84c2"),
        "the raw id is never written: {text}"
    );
    assert!(!text.contains("evt-000001"), "{text}");

    // Offline: no secret in the environment, no daemon running.
    let output = Command::new(CTL)
        .env_clear()
        .arg("log")
        .arg("summary")
        .arg(&files[0])
        .output()
        .expect("run aivtuberctl");
    let summary = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{summary}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(summary.contains("turns total=1 chat=1"), "{summary}");
    assert!(summary.contains("malformed=0"), "{summary}");
    assert!(
        !summary.contains("viewer"),
        "no logged text is printed: {summary}"
    );
}

#[test]
fn without_a_remembered_choice_nothing_is_created() {
    let dir = scratch("off");
    let stderr = run_app(&dir, None, &chat_line());
    assert!(stderr.contains("session log: off"), "{stderr}");
    assert!(!dir.join("logs").exists());

    let dir = scratch("off-explicit");
    run_app(&dir, Some(false), &chat_line());
    assert!(!dir.join("logs").exists());
}

#[test]
fn the_summary_of_a_missing_file_fails_without_a_secret() {
    let output = Command::new(CTL)
        .env_clear()
        .args(["log", "summary", "no-such-session-file.jsonl"])
        .output()
        .expect("run aivtuberctl");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot read"));
}
