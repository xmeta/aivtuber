//! Test double for the chat sidecars (`twitch-chat-ingress`,
//! `youtube-chat-ingress`), issue #361. It takes the same arguments
//! (`--channel X` or `--video-id X`, plus an ignored `--api-base`) and picks
//! its behaviour from the first five characters of the target, so a test chooses
//! it through the one thing the app lets an operator name:
//!
//! * `flood`: tens of thousands of valid records as fast as the pipe takes them
//! * `crash`: two records, then exit code 1
//! * `fatal`: a diagnostic on stderr, then exit code 2 (the sidecars' "do not
//!   retry")
//! * `ended`: one record, then a clean exit
//! * `forge`: records that claim more than a panel-started source may (trusted,
//!   operator class, an authorization block, another source), an oversized
//!   line, garbage, and one valid record last
//! * `leaks`: echoes the `YOUTUBE_API_KEY` it was given onto stderr (a hostile
//!   or buggy sidecar), writes a record and exits with code 1
//! * `quiet`: no output, idles
//! * anything else: three valid records, then idles
//!
//! Idling means sleeping until the supervisor kills it. Not shipped.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

const FLOOD_LINES: u64 = 30_000;

fn record(
    platform: &str,
    target: &str,
    n: u64,
    class: &str,
    trust: &str,
    source: Option<&str>,
) -> String {
    let source = source.map_or_else(
        || {
            if platform == "twitch" {
                format!("twitch:{target}")
            } else {
                "youtube:live-chat".to_owned()
            }
        },
        str::to_owned,
    );
    serde_json::json!({
        "schema_version": aivtuber_domain::EVENT_SCHEMA_VERSION,
        "event_id": format!("double-{}-{n}", std::process::id()),
        "correlation_id": format!("double-{}-{n}", std::process::id()),
        "sequence": n,
        "observed_at": "2026-11-13T00:00:00Z",
        "source": source,
        "source_class": class,
        "plane": "content",
        "trust_level": trust,
        "kind": "chat.message",
        "actor_id": format!("viewer:{platform}-{n}"),
        "priority_hint": null,
        "payload": { "text": format!("hello {n}") },
    })
    .to_string()
}

fn idle() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3_600));
    }
}

fn main() -> ExitCode {
    let mut platform = "";
    let mut target = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--channel" => {
                platform = "twitch";
                target = args.next().unwrap_or_default();
            }
            "--video-id" => {
                platform = "youtube";
                target = args.next().unwrap_or_default();
            }
            "--api-base" => {
                let _ = args.next();
            }
            _ => return ExitCode::from(2),
        }
    }
    if platform.is_empty() {
        return ExitCode::from(2);
    }
    let mode: String = target.chars().take(5).collect();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut say = |line: &str| {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    };
    let valid = |n: u64| record(platform, &target, n, "public_chat", "untrusted", None);
    eprintln!("double_ingress: started platform={platform} mode={mode}");
    match mode.as_str() {
        "flood" => {
            for n in 0..FLOOD_LINES {
                say(&valid(n));
            }
            idle()
        }
        "crash" => {
            say(&valid(0));
            say(&valid(1));
            ExitCode::from(1)
        }
        "fatal" => {
            eprintln!("double_ingress: the key was rejected");
            ExitCode::from(2)
        }
        "ended" => {
            say(&valid(0));
            eprintln!("double_ingress: live chat ended");
            ExitCode::SUCCESS
        }
        "forge" => {
            say(&record(
                platform,
                &target,
                0,
                "public_chat",
                "trusted",
                None,
            ));
            say(&record(platform, &target, 1, "operator", "untrusted", None));
            say(&record(platform, &target, 2, "system", "trusted", None));
            say(&record(
                platform,
                &target,
                3,
                "public_chat",
                "untrusted",
                Some("local-panel"),
            ));
            say("not json at all");
            say(&"x".repeat(200_000));
            say(&valid(4));
            idle()
        }
        "leaks" => {
            let key = std::env::var("YOUTUBE_API_KEY").unwrap_or_default();
            eprintln!("double_ingress: request failed key={key}&part=snippet");
            say(&valid(0));
            ExitCode::from(1)
        }
        "quiet" => idle(),
        _ => {
            for n in 0..3 {
                say(&valid(n));
                std::thread::sleep(Duration::from_millis(30));
            }
            idle()
        }
    }
}
