//! The real binary, offline: the same checks the release smoke test runs on an
//! unpacked archive, cheap enough for every CI run (issue #294 S2).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

fn fake_release(tag: &str) -> PathBuf {
    let root = common::scratch(tag).join("release");
    fs::create_dir_all(&root).unwrap();
    for name in [
        "aivtuber-app",
        "speech-vad-sidecar",
        "speech-stt-worker",
        "local-tts-sidecar",
    ] {
        fs::write(root.join(exe(name)), b"bin").unwrap();
    }
    fs::create_dir_all(root.join("renderer/local-avatar/dist")).unwrap();
    fs::write(root.join("renderer/local-avatar/dist/index.html"), "<html>").unwrap();
    let pack = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/starter-reaction-pack/descriptors");
    let dst = root.join("examples/starter-reaction-pack/descriptors");
    fs::create_dir_all(&dst).unwrap();
    for entry in fs::read_dir(pack).unwrap().flatten() {
        fs::copy(entry.path(), dst.join(entry.file_name())).unwrap();
    }
    root
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aivtuber-setup"))
        // No proxy or token from the developer's environment can matter offline.
        .env_remove("HTTPS_PROXY")
        .env_remove("HTTP_PROXY")
        .args(args)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn listing(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            out.push(e.path());
            if e.path().is_dir() {
                walk(&e.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

#[test]
fn help_lists_the_subcommands() {
    let o = run(&["--help"]);
    assert!(o.status.success());
    let t = text(&o);
    assert!(t.contains("local") && t.contains("start"), "{t}");
    let o = run(&["local", "--help"]);
    let t = text(&o);
    for flag in [
        "--dry-run",
        "--offline",
        "--force",
        "--llama-flavor",
        "--skip",
        "--env-file",
    ] {
        assert!(t.contains(flag), "{flag} missing from: {t}");
    }
}

#[test]
fn dry_run_prints_the_plan_and_the_duties_without_writing_anything() {
    let root = fake_release("cli-dry");
    let before = listing(&root);
    let assets = root.parent().unwrap().join("assets");
    let o = run(&[
        "local",
        "--dry-run",
        "--root",
        root.to_str().unwrap(),
        "--assets",
        assets.to_str().unwrap(),
    ]);
    let t = text(&o);
    assert!(o.status.success(), "{t}");
    for needle in [
        "dry run",
        "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        "machine-generated",
        "Article 30-4",
        "aivtuber-setup",
        "start",
    ] {
        assert!(t.contains(needle), "plan lacks {needle:?}: {t}");
    }
    assert!(!assets.exists());
    assert_eq!(listing(&root), before);
}

#[test]
fn offline_with_nothing_fetched_fails_closed() {
    let root = fake_release("cli-offline");
    let assets = root.parent().unwrap().join("assets");
    let o = run(&[
        "local",
        "--offline",
        "--root",
        root.to_str().unwrap(),
        "--assets",
        assets.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("--offline: missing"));
    assert!(!root.join("local-profile.env").exists());
}

#[test]
fn start_without_an_env_file_says_to_run_setup_first() {
    let root = fake_release("cli-start");
    let o = run(&[
        "start",
        "--dry-run",
        "--env-file",
        root.join("local-profile.env").to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("aivtuber-setup local"), "{}", text(&o));
}

#[test]
fn an_unknown_skip_step_is_refused() {
    let root = fake_release("cli-skip");
    let o = run(&[
        "local",
        "--dry-run",
        "--skip",
        "nonsense",
        "--root",
        root.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("unknown step"));
}
