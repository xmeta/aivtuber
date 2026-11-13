//! Issue #361: the app supervises the chat sidecars it starts for the panel.
//!
//! The sidecar is a double (`chat-sidecar-double`) that picks its behaviour
//! from the target's first five characters, so each test names the behaviour
//! through the one thing an operator may name. No network is involved.

use aivtuber_adapters::SecretString;
use aivtuber_app::{
    ChatIngressController, ChatIngressEnvironment, ChatPlatform, ChatState, RawIngressConfig,
};
use aivtuber_domain::{SourceClass, TrustLevel};
use aivtuber_runtime::{
    ContentAdmitDecision, IngressPolicy, SecurityRuntime, SecurityRuntimeConfig,
};
use aivtuber_scheduler::SchedulerConfig;
use aivtuber_telemetry::SecretRedactor;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DOUBLE: &str = env!("CARGO_BIN_EXE_chat-sidecar-double");
const KEY: &str = "AIza-SENTINEL-KEY-0123456789abcdef";
const QUEUE: usize = 32;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-361-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn environment(root: &Path, env_key: Option<&str>) -> ChatIngressEnvironment {
    let mut env = ChatIngressEnvironment::new(
        root.join("local-chat.json"),
        root.join("local-youtube-key.txt"),
        env_key.map(SecretString::new),
        Path::new("."),
        None,
        RawIngressConfig {
            max_record_bytes: SecurityRuntimeConfig::default().max_envelope_bytes,
            queue_capacity: QUEUE,
        },
    );
    env.twitch_program = PathBuf::from(DOUBLE);
    env.youtube_program = PathBuf::from(DOUBLE);
    env.restart.initial_delay_ms = 40;
    env.restart.max_delay_ms = 200;
    env.max_failures = 3;
    env.connecting_ms = 100;
    env
}

fn controller(root: &Path, env_key: Option<&str>) -> ChatIngressController {
    ChatIngressController::new(environment(root, env_key))
}

fn alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .expect("tasklist");
        String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

/// Polls like the composition loop and counts what it hands over.
fn collect(
    controller: &mut ChatIngressController,
    limit: Duration,
    want: usize,
) -> Vec<aivtuber_app::ChatRecord> {
    let mut got = Vec::new();
    wait_for(limit, || {
        got.extend(controller.take_records(8));
        got.len() >= want
    });
    got
}

fn runtime(policy: IngressPolicy) -> SecurityRuntime {
    let mut runtime = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig::default(),
        SecretRedactor::default(),
        None,
    )
    .expect("runtime");
    runtime.set_ingress_policy(policy);
    runtime
}

#[test]
fn a_twitch_connection_delivers_untrusted_chat_then_disconnect_leaves_no_child() {
    let root = scratch("twitch");
    let mut chat = controller(&root, None);
    let detail = chat.connect(ChatPlatform::Twitch, " SomeChannel ").unwrap();
    assert_eq!(detail, "connecting to twitch somechannel");
    let pids = chat.child_ids();
    assert_eq!(pids.len(), 1);
    assert!(alive(pids[0]));
    assert_eq!(chat.status().twitch.state, ChatState::Connecting);

    let records = collect(&mut chat, Duration::from_secs(10), 3);
    assert_eq!(records.len(), 3);
    // The same admission as a stdin line, under the documented ceiling.
    let mut runtime = runtime(IngressPolicy::new(
        TrustLevel::Untrusted,
        &[SourceClass::PublicChat],
    ));
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.platform, ChatPlatform::Twitch);
        let decision = runtime
            .admit_content_bytes(&record.raw, index as u64 * 1_000)
            .unwrap();
        assert_eq!(decision, ContentAdmitDecision::Queued);
        let event = runtime.pop_content().unwrap();
        assert_eq!(event.source_class, SourceClass::PublicChat);
        assert_eq!(event.trust_level, TrustLevel::Untrusted);
        assert!(event.authorization.is_none());
        assert!(event.source.starts_with("twitch:somechannel"));
        chat.note_outcome(ChatPlatform::Twitch, true);
    }
    let status = chat.status();
    assert_eq!(status.twitch.messages_accepted, 3);
    assert_eq!(status.twitch.messages_dropped, 0);
    assert!(status.twitch.messages_per_minute >= 3);
    assert_eq!(status.twitch.state, ChatState::Connected);
    assert_eq!(status.twitch.target.as_deref(), Some("somechannel"));
    assert_eq!(status.twitch.last_error, None);

    let detail = chat.disconnect(ChatPlatform::Twitch).unwrap();
    assert_eq!(detail, "disconnected from twitch");
    assert_eq!(chat.status().twitch.state, ChatState::Disconnected);
    assert!(chat.child_ids().is_empty());
    assert!(
        wait_for(Duration::from_secs(10), || !alive(pids[0])),
        "the child must be gone"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn both_platforms_run_at_once_and_dropping_the_controller_ends_every_child() {
    let root = scratch("both");
    let mut chat = controller(&root, Some(KEY));
    chat.connect(ChatPlatform::Twitch, "alpha_one").unwrap();
    chat.connect(
        ChatPlatform::Youtube,
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
    )
    .unwrap();
    let pids = chat.child_ids();
    assert_eq!(pids.len(), 2);
    let records = collect(&mut chat, Duration::from_secs(10), 6);
    let platforms: std::collections::BTreeSet<_> =
        records.iter().map(|r| r.platform.name()).collect();
    assert_eq!(platforms.len(), 2, "{records:?}");
    assert_eq!(chat.status().youtube.target.as_deref(), Some("dQw4w9WgXcQ"));
    drop(chat);
    for pid in pids {
        assert!(
            wait_for(Duration::from_secs(10), || !alive(pid)),
            "{pid} outlived the controller"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_flood_is_dropped_in_a_bounded_queue_and_never_blocks_the_loop() {
    const FLOOD: u64 = 30_000;
    let root = scratch("flood");
    let mut chat = controller(&root, None);
    chat.connect(ChatPlatform::Twitch, "flood_chan").unwrap();
    // Nobody drains for a while: the child writes 30000 lines into a queue of 32.
    assert!(wait_for(Duration::from_secs(30), || {
        chat.status().twitch.dropped_queue_full > 20_000
    }));
    // The loop takes a bounded handful per turn, instantly, however much the
    // child still writes.
    let mut delivered = 0_u64;
    let mut slowest = Duration::ZERO;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let started = Instant::now();
        let batch = chat.take_records(8);
        slowest = slowest.max(started.elapsed());
        assert!(batch.len() <= 8);
        delivered += batch.len() as u64;
        let dropped = chat.status().twitch.dropped_queue_full;
        if delivered + dropped == FLOOD {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "delivered={delivered} dropped={dropped}"
        );
        if batch.is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let status = chat.status();
    eprintln!(
        "flood: lines={FLOOD} queue={QUEUE} delivered={delivered} dropped_queue_full={} slowest_take_records={slowest:?}",
        status.twitch.dropped_queue_full
    );
    // Every line is either delivered or counted: nothing is lost silently and
    // nothing was buffered without bound.
    assert_eq!(delivered + status.twitch.dropped_queue_full, FLOOD);
    assert!(status.twitch.dropped_queue_full > 20_000);
    assert_eq!(
        status.twitch.messages_dropped,
        status.twitch.dropped_queue_full
    );
    assert!(
        slowest < Duration::from_millis(250),
        "slowest take_records {slowest:?}"
    );
    chat.disconnect(ChatPlatform::Twitch).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn records_claiming_more_than_a_panel_source_may_are_dropped_and_counted() {
    let root = scratch("forge");
    let mut chat = controller(&root, None);
    chat.connect(ChatPlatform::Twitch, "forgery").unwrap();
    let records = collect(&mut chat, Duration::from_secs(10), 1);
    assert_eq!(records.len(), 1, "only the valid record passes the gate");
    let event: serde_json::Value = serde_json::from_slice(&records[0].raw).unwrap();
    assert_eq!(event["trust_level"], "untrusted");
    assert_eq!(event["source_class"], "public_chat");
    assert!(wait_for(Duration::from_secs(5), || {
        chat.status().twitch.dropped_refused >= 5
    }));
    let status = chat.status();
    // trusted, operator, system, another source and garbage = 5 refused by the
    // panel-source ceiling; the 200 kB line never leaves the pump (oversize).
    assert_eq!(status.twitch.dropped_refused, 5);
    assert_eq!(status.twitch.messages_dropped, 6);
    chat.disconnect(ChatPlatform::Twitch).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_stopping_child_is_restarted_with_backoff_then_given_up_on() {
    let root = scratch("crash");
    let mut chat = controller(&root, None);
    chat.connect(ChatPlatform::Twitch, "crashing").unwrap();
    let mut seen_backoff = false;
    let mut records = 0;
    assert!(wait_for(Duration::from_secs(20), || {
        records += chat.take_records(8).len();
        let status = chat.status();
        seen_backoff |= status.twitch.state == ChatState::Backoff;
        status.twitch.state == ChatState::Failed
    }));
    let status = chat.status();
    assert!(seen_backoff);
    assert_eq!(status.twitch.restarts, 2, "3 failures allowed: 2 restarts");
    assert!(
        status
            .twitch
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("kept stopping")),
        "{:?}",
        status.twitch.last_error
    );
    assert!(records >= 2, "records written before each exit are read");
    assert!(chat.child_ids().is_empty());
    // A new connect starts a fresh supervised run.
    chat.connect(ChatPlatform::Twitch, "alpha_one").unwrap();
    assert_eq!(chat.status().twitch.restarts, 0);
    chat.disconnect(ChatPlatform::Twitch).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_fatal_exit_is_not_retried_and_a_clean_exit_is_a_disconnect() {
    let root = scratch("exit");
    let mut chat = controller(&root, None);
    chat.connect(ChatPlatform::Twitch, "fatal_one").unwrap();
    assert!(wait_for(Duration::from_secs(10), || {
        chat.take_records(8);
        chat.status().twitch.state == ChatState::Failed
    }));
    let status = chat.status();
    assert_eq!(status.twitch.restarts, 0);
    assert_eq!(
        status.twitch.last_error.as_deref(),
        Some("double_ingress: the key was rejected")
    );
    chat.connect(ChatPlatform::Twitch, "ended_one").unwrap();
    assert!(wait_for(Duration::from_secs(10), || {
        chat.take_records(8);
        chat.status().twitch.state == ChatState::Disconnected
    }));
    let status = chat.status();
    assert_eq!(status.twitch.restarts, 0);
    assert!(
        status
            .twitch
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("ended")),
        "{:?}",
        status.twitch.last_error
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn bad_inputs_are_refused_before_anything_is_spawned() {
    let root = scratch("bad");
    let mut chat = controller(&root, Some(KEY));
    for (platform, target) in [
        (ChatPlatform::Twitch, ""),
        (ChatPlatform::Twitch, "ab"),
        (ChatPlatform::Twitch, "#channel"),
        (ChatPlatform::Twitch, "chan nel"),
        (ChatPlatform::Twitch, "--channel"),
        (ChatPlatform::Twitch, "https://twitch.tv/somechannel"),
        (ChatPlatform::Youtube, ""),
        (ChatPlatform::Youtube, "short"),
        (ChatPlatform::Youtube, "dQw4w9WgXcQ; calc"),
        (
            ChatPlatform::Youtube,
            "https://evil.example/watch?v=dQw4w9WgXcQ",
        ),
        (
            ChatPlatform::Youtube,
            "https://www.youtube.com/@somechannel",
        ),
    ] {
        let error = chat.connect(platform, target).unwrap_err();
        assert!(error.starts_with("refused: "), "{target}: {error}");
        assert!(chat.child_ids().is_empty(), "{target}");
    }
    assert_eq!(chat.status().twitch.state, ChatState::Disconnected);
    assert_eq!(chat.status().twitch.saved_target, None);
    assert!(!root.join("local-chat.json").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn youtube_needs_a_key_and_the_key_never_appears_in_any_output() {
    let root = scratch("key");
    let mut chat = controller(&root, None);
    // No key yet.
    let error = chat
        .connect(ChatPlatform::Youtube, "dQw4w9WgXcQ")
        .unwrap_err();
    assert!(error.contains("key"), "{error}");
    assert!(!chat.status().youtube_key.set);

    let mut outputs = vec![chat.set_key(&format!("  {KEY}  ")).unwrap()];
    assert!(chat.set_key("has space").is_err());
    assert!(chat.set_key("").is_err());
    let stored = std::fs::read_to_string(root.join("local-youtube-key.txt")).unwrap();
    assert_eq!(stored.trim(), KEY);
    assert!(chat.status().youtube_key.set);
    outputs.push(chat.connect(ChatPlatform::Youtube, "dQw4w9WgXcQ").unwrap());
    chat.disconnect(ChatPlatform::Youtube).unwrap();
    outputs.push(serde_json::to_string(&chat.status()).unwrap());
    outputs.push(format!("{chat:?}"));
    outputs.push(format!("{:?}", environment(&root, Some(KEY))));
    outputs.push(chat.remove_key().unwrap());
    outputs.push(serde_json::to_string(&chat.status()).unwrap());
    for output in &outputs {
        assert!(!output.contains(KEY), "the key leaked into: {output}");
    }
    assert!(!root.join("local-youtube-key.txt").exists());
    assert!(!chat.status().youtube_key.set);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_hostile_sidecar_cannot_put_the_key_into_the_status() {
    let root = scratch("leak");
    // `leaks______` echoes the key it was given onto stderr, writes one
    // record and exits with code 1, so its last diagnostic becomes the error.
    for (name, env_key) in [("environment", Some(KEY)), ("file", None)] {
        let mut chat = controller(&root, env_key);
        if env_key.is_none() {
            chat.set_key(KEY).unwrap();
        }
        chat.connect(ChatPlatform::Youtube, "leaks______").unwrap();
        assert!(
            wait_for(Duration::from_secs(10), || {
                chat.take_records(8);
                chat.status().youtube.state == ChatState::Backoff
            }),
            "{name}"
        );
        let status = chat.status();
        let error = status.youtube.last_error.clone().expect("an error");
        assert!(error.contains("request failed"), "{name}: {error}");
        assert!(error.contains("[redacted]"), "{name}: {error}");
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains(KEY), "{name}: {json}");
        chat.disconnect(ChatPlatform::Youtube).unwrap();
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_environment_key_wins_and_removing_the_file_key_keeps_it() {
    let root = scratch("envkey");
    let mut chat = controller(&root, Some(KEY));
    let status = chat.status();
    assert!(status.youtube_key.set);
    assert_eq!(status.youtube_key.source.as_deref(), Some("environment"));
    assert!(
        chat.set_key("file-key-0123456789")
            .unwrap()
            .contains("precedence")
    );
    assert_eq!(
        chat.status().youtube_key.source.as_deref(),
        Some("environment")
    );
    chat.connect(ChatPlatform::Youtube, "dQw4w9WgXcQ").unwrap();
    assert!(chat.remove_key().unwrap().contains("still set"));
    assert_eq!(chat.status().youtube.state, ChatState::Connecting);
    chat.disconnect(ChatPlatform::Youtube).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn removing_the_only_key_disconnects_youtube() {
    let root = scratch("rmkey");
    let mut chat = controller(&root, None);
    chat.set_key(KEY).unwrap();
    chat.connect(ChatPlatform::Youtube, "dQw4w9WgXcQ").unwrap();
    assert_eq!(chat.child_ids().len(), 1);
    assert!(chat.remove_key().unwrap().contains("disconnected"));
    assert_eq!(chat.status().youtube.state, ChatState::Disconnected);
    assert!(chat.child_ids().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_last_choice_is_offered_on_the_next_start_but_never_connected() {
    let root = scratch("saved");
    {
        let mut chat = controller(&root, Some(KEY));
        chat.connect(ChatPlatform::Twitch, "Some_Channel").unwrap();
        chat.connect(ChatPlatform::Youtube, "https://youtu.be/dQw4w9WgXcQ")
            .unwrap();
        chat.disconnect(ChatPlatform::Twitch).unwrap();
    }
    let saved = std::fs::read_to_string(root.join("local-chat.json")).unwrap();
    assert!(saved.len() < 256, "{saved}");
    assert!(!saved.contains(KEY));
    let mut chat = controller(&root, Some(KEY));
    let status = chat.status();
    assert_eq!(status.twitch.saved_target.as_deref(), Some("some_channel"));
    assert_eq!(status.youtube.saved_target.as_deref(), Some("dQw4w9WgXcQ"));
    assert_eq!(status.twitch.state, ChatState::Disconnected);
    assert_eq!(status.youtube.state, ChatState::Disconnected);
    assert_eq!(status.twitch.target, None);
    assert!(chat.child_ids().is_empty());
    assert!(chat.take_records(8).is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_sidecar_program_is_a_typed_refusal() {
    let root = scratch("missing");
    let mut env = environment(&root, None);
    env.twitch_program = root.join("no-such-sidecar");
    let mut chat = ChatIngressController::new(env);
    assert!(!chat.status().twitch.sidecar_found);
    assert!(chat.status().youtube.sidecar_found);
    let error = chat
        .connect(ChatPlatform::Twitch, "somechannel")
        .unwrap_err();
    assert!(error.contains("not installed"), "{error}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reconnecting_to_the_same_target_is_idempotent_and_a_new_target_replaces_the_child() {
    let root = scratch("replace");
    let mut chat = controller(&root, None);
    chat.connect(ChatPlatform::Twitch, "first_one").unwrap();
    let first = chat.child_ids();
    assert!(
        chat.connect(ChatPlatform::Twitch, "FIRST_ONE")
            .unwrap()
            .starts_with("already connected")
    );
    assert_eq!(chat.child_ids(), first);
    chat.connect(ChatPlatform::Twitch, "second_one").unwrap();
    let second = chat.child_ids();
    assert_eq!(second.len(), 1);
    assert_ne!(second, first);
    assert!(wait_for(Duration::from_secs(10), || !alive(first[0])));
    chat.disconnect(ChatPlatform::Twitch).unwrap();
    assert!(
        chat.disconnect(ChatPlatform::Twitch)
            .unwrap()
            .contains("not connected")
    );
    let _ = std::fs::remove_dir_all(&root);
}
