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
        "--profile",
        "--llm-endpoint",
        "--from-target",
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
        "gemma-4-E4B-it-qat-UD-Q4_K_XL.gguf",
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
fn the_perception_dry_run_names_the_vision_files_the_vram_and_the_extra_duty() {
    let root = fake_release("cli-dry-perception");
    let before = listing(&root);
    let assets = root.parent().unwrap().join("assets");
    let o = run(&[
        "local",
        "--dry-run",
        "--profile",
        "perception",
        "--root",
        root.to_str().unwrap(),
        "--assets",
        assets.to_str().unwrap(),
    ]);
    let t = text(&o);
    assert!(o.status.success(), "{t}");
    for needle in [
        "profile  perception",
        "Qwen3VL-4B-Instruct-Q4_K_M.gguf",
        "mmproj-Qwen3VL-4B-Instruct-Q8_0.gguf",
        "at least about 12 GB of VRAM",
        "Qwen3-VL-4B-Instruct",
        "screen",
    ] {
        assert!(t.contains(needle), "plan lacks {needle:?}: {t}");
    }
    assert!(
        !t.contains("gemma-4-E4B-it-qat-UD-Q4_K_XL.gguf"),
        "the default model is not part of this plan: {t}"
    );
    assert!(!assets.exists());
    assert_eq!(listing(&root), before);
}

#[test]
fn an_unknown_profile_and_a_remote_llm_endpoint_are_refused() {
    let root = fake_release("cli-bad-args");
    let o = run(&[
        "local",
        "--dry-run",
        "--profile",
        "huge",
        "--root",
        root.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1));
    let t = text(&o);
    assert!(
        t.contains("unknown profile") && t.contains("perception"),
        "{t}"
    );
    let o = run(&[
        "local",
        "--dry-run",
        "--llm-endpoint",
        "https://api.example.com/v1/responses",
        "--root",
        root.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("--llm-endpoint"), "{}", text(&o));
}

#[test]
fn a_dry_run_from_a_target_directory_says_what_is_missing() {
    let root = fake_release("cli-target");
    let o = run(&[
        "local",
        "--dry-run",
        "--from-target",
        root.join("nonexistent-target").to_str().unwrap(),
        "--root",
        root.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("not a built checkout"), "{}", text(&o));
    // The same directory holding the programs works.
    let o = run(&[
        "local",
        "--dry-run",
        "--from-target",
        root.to_str().unwrap(),
        "--root",
        root.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("--from-target"));
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
fn start_help_lists_verbose() {
    let o = run(&["start", "--help"]);
    assert!(o.status.success());
    let t = text(&o);
    assert!(t.contains("--verbose") && t.contains("AIVTUBER_LOG"), "{t}");
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

// ---- aivtuber-setup hybrid (#366) ----

/// A recognisable stand-in; never a real key.
const FAKE_KEY: &str = "sk-or-v1-0000test0000fake0000key0000";

fn run_with_key(args: &[&str], key: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aivtuber-setup"));
    cmd.env_remove("HTTPS_PROXY")
        .env_remove("HTTP_PROXY")
        .env_remove("OPENROUTER_API_KEY")
        // No terminal: the prompt must never be reached.
        .stdin(std::process::Stdio::null());
    if let Some(key) = key {
        cmd.env("OPENROUTER_API_KEY", key);
    }
    cmd.args(args).output().unwrap()
}

#[test]
fn hybrid_help_lists_its_flags_and_no_key_flag() {
    let t = text(&run(&["hybrid", "--help"]));
    for flag in [
        "--model",
        "--check-key",
        "--dry-run",
        "--force",
        "--env-file",
    ] {
        assert!(t.contains(flag), "{flag} missing from: {t}");
    }
    assert!(t.contains("OPENROUTER_API_KEY"), "{t}");
    for forbidden in ["--api-key", "--key", "--token"] {
        assert!(!t.contains(forbidden), "{forbidden} must not exist: {t}");
    }
}

#[test]
fn the_key_is_not_accepted_on_the_command_line() {
    let root = fake_release("cli-hybrid-argv");
    let o = run_with_key(
        &[
            "hybrid",
            "--root",
            root.to_str().unwrap(),
            "--api-key",
            FAKE_KEY,
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(!text(&o).contains(FAKE_KEY), "{}", text(&o));
    assert!(!root.join("hybrid-profile.env").exists());
}

#[test]
fn without_a_terminal_and_without_the_variable_it_fails_with_guidance_and_writes_nothing() {
    let root = fake_release("cli-hybrid-nokey");
    let before = listing(&root);
    let o = run_with_key(&["hybrid", "--root", root.to_str().unwrap()], None);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    let t = text(&o);
    assert!(t.contains("OPENROUTER_API_KEY"), "{t}");
    assert_eq!(listing(&root), before);
}

#[test]
fn the_model_list_is_bounded_and_validated_before_anything_else() {
    let root = fake_release("cli-hybrid-models");
    let nine: Vec<String> = (0..9).map(|i| format!("v/m{i}")).collect();
    let mut args = vec!["hybrid", "--dry-run", "--root", root.to_str().unwrap()];
    for m in &nine {
        args.push("--model");
        args.push(m);
    }
    let o = run_with_key(&args, Some(FAKE_KEY));
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("at most 8"), "{}", text(&o));

    let o = run_with_key(
        &[
            "hybrid",
            "--dry-run",
            "--root",
            root.to_str().unwrap(),
            "--model",
            "not-an-id",
        ],
        Some(FAKE_KEY),
    );
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("vendor/model"), "{}", text(&o));
}

#[test]
fn a_hybrid_dry_run_needs_no_key_downloads_no_llama_and_writes_nothing() {
    let root = fake_release("cli-hybrid-dry");
    let before = listing(&root);
    let assets = root.parent().unwrap().join("assets");
    let o = run_with_key(
        &[
            "hybrid",
            "--dry-run",
            "--root",
            root.to_str().unwrap(),
            "--assets",
            assets.to_str().unwrap(),
        ],
        None,
    );
    let t = text(&o);
    assert!(o.status.success(), "{t}");
    for needle in [
        "aivtuber-setup hybrid",
        "OpenRouter",
        "openrouter.ai",
        "--env-file",
    ] {
        assert!(t.contains(needle), "plan lacks {needle:?}: {t}");
    }
    for absent in ["llama.cpp", "gguf", "Vulkan"] {
        assert!(!t.contains(absent), "hybrid plan mentions {absent:?}: {t}");
    }
    assert!(!assets.exists());
    assert_eq!(listing(&root), before);
}

#[test]
fn the_key_in_the_environment_never_reaches_the_output() {
    let root = fake_release("cli-hybrid-env");
    let assets = root.parent().unwrap().join("assets");
    // A real (non dry) offline run cannot finish, but it must still not leak.
    let o = run_with_key(
        &[
            "hybrid",
            "--offline",
            "--root",
            root.to_str().unwrap(),
            "--assets",
            assets.to_str().unwrap(),
        ],
        Some(FAKE_KEY),
    );
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(!text(&o).contains(FAKE_KEY), "{}", text(&o));
    assert!(!root.join("hybrid-profile.env").exists());
}

#[test]
fn an_existing_hybrid_env_file_is_refused_before_the_key_is_asked_for() {
    let root = fake_release("cli-hybrid-exists");
    fs::write(root.join("hybrid-profile.env"), "A=1\n").unwrap();
    let o = run_with_key(&["hybrid", "--root", root.to_str().unwrap()], None);
    assert_eq!(o.status.code(), Some(1));
    let t = text(&o);
    assert!(
        t.contains("--force") && !t.contains("OPENROUTER_API_KEY"),
        "{t}"
    );
    assert_eq!(
        fs::read_to_string(root.join("hybrid-profile.env")).unwrap(),
        "A=1\n"
    );
}

#[test]
fn check_key_cannot_be_combined_with_offline() {
    let root = fake_release("cli-hybrid-offline-check");
    let o = run_with_key(
        &[
            "hybrid",
            "--offline",
            "--check-key",
            "--root",
            root.to_str().unwrap(),
        ],
        Some(FAKE_KEY),
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("--offline"), "{}", text(&o));
    assert!(!text(&o).contains(FAKE_KEY));
}
