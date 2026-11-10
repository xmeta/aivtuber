//! Read-only YouTube Live chat sidecar (#293).
//!
//! Polls the official YouTube Data API v3 `liveChatMessages.list` with an
//! operator API key (env `YOUTUBE_API_KEY`, never an argument, never logged)
//! and writes one raw-ingress JSON record per chat message to stdout:
//!
//! ```text
//! YOUTUBE_API_KEY=... youtube-chat-ingress --video-id <id> | aivtuber-app
//! ```
//!
//! Run the app with `AIVTUBER_INGRESS_MAX_TRUST=untrusted` and
//! `AIVTUBER_INGRESS_SOURCE_CLASSES=public_chat,donation` so the app, not this
//! sidecar, enforces what the pipe may carry. Diagnostics go to stderr only;
//! stdout carries records and nothing else.

use aivtuber_adapters::{
    DEFAULT_YOUTUBE_API_BASE, EnqueueOutcome, PollEnd, UreqYoutubeHttp,
    YOUTUBE_DEFAULT_MIN_POLL_MS, YoutubeApiKey, YoutubeChatPoller, YoutubePollerConfig,
    enqueue_youtube_record, resolve_live_chat_id, validate_live_chat_id, validate_video_id,
};
use std::io::Write;
use std::process::ExitCode;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::{Duration, Instant};

const DEFAULT_QUEUE: usize = 256;
const DEFAULT_STATS_SECS: u64 = 60;

struct Args {
    video_id: Option<String>,
    live_chat_id: Option<String>,
    api_base: String,
    min_poll_ms: u64,
    queue: usize,
    stats_every: Duration,
    run_for: Option<Duration>,
    include_backlog: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        video_id: None,
        live_chat_id: None,
        api_base: DEFAULT_YOUTUBE_API_BASE.to_owned(),
        min_poll_ms: YOUTUBE_DEFAULT_MIN_POLL_MS,
        queue: DEFAULT_QUEUE,
        stats_every: Duration::from_secs(DEFAULT_STATS_SECS),
        run_for: None,
        include_backlog: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = |name: &str| iter.next().ok_or_else(|| format!("{name} needs a value"));
        let number = |text: String, name: &str| -> Result<u64, String> {
            text.parse::<u64>()
                .map_err(|_| format!("{name} must be a non-negative integer"))
        };
        match flag.as_str() {
            "--video-id" => args.video_id = Some(validate_video_id(&value("--video-id")?)?),
            "--live-chat-id" => {
                args.live_chat_id = Some(validate_live_chat_id(&value("--live-chat-id")?)?)
            }
            "--api-base" => args.api_base = value("--api-base")?,
            "--min-poll-ms" => args.min_poll_ms = number(value("--min-poll-ms")?, "--min-poll-ms")?,
            "--queue" => {
                args.queue = number(value("--queue")?, "--queue")?.clamp(1, 65_536) as usize
            }
            "--stats-secs" => {
                args.stats_every =
                    Duration::from_secs(number(value("--stats-secs")?, "--stats-secs")?.max(1))
            }
            "--duration-secs" => {
                args.run_for = Some(Duration::from_secs(number(
                    value("--duration-secs")?,
                    "--duration-secs",
                )?))
            }
            "--include-backlog" => args.include_backlog = true,
            "-h" | "--help" => {
                return Err("usage: YOUTUBE_API_KEY=... youtube-chat-ingress \
                     (--video-id <id> | --live-chat-id <id>) [--min-poll-ms N] [--queue N] \
                     [--stats-secs N] [--duration-secs N] [--include-backlog] [--api-base URL]"
                    .to_owned());
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if args.video_id.is_some() == args.live_chat_id.is_some() {
        return Err("exactly one of --video-id or --live-chat-id is required".to_owned());
    }
    Ok(args)
}

/// Stdout writer on its own thread so a slow consumer never blocks polling.
fn spawn_writer(capacity: usize) -> SyncSender<String> {
    let (sender, receiver) = sync_channel::<String>(capacity);
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for line in receiver {
            if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                break;
            }
        }
    });
    sender
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let key = match std::env::var("YOUTUBE_API_KEY")
        .map_err(|_| "YOUTUBE_API_KEY is not set".to_owned())
        .and_then(|raw| YoutubeApiKey::new(&raw))
    {
        Ok(key) => key,
        Err(message) => {
            eprintln!("youtube_ingress: {message}");
            return ExitCode::from(2);
        }
    };
    let http = UreqYoutubeHttp::default();
    let live_chat_id = match (&args.live_chat_id, &args.video_id) {
        (Some(id), _) => id.clone(),
        (None, Some(video)) => {
            match resolve_live_chat_id(&http, &args.api_base, &key, video, Duration::from_secs(15))
            {
                Ok(id) => id,
                Err(message) => {
                    eprintln!("youtube_ingress: {message}");
                    return ExitCode::from(2);
                }
            }
        }
        (None, None) => return ExitCode::from(2),
    };

    let mut config = YoutubePollerConfig::new(&args.api_base, &live_chat_id);
    config.min_poll_ms = args.min_poll_ms;
    config.emit_backlog = args.include_backlog;
    let mut poller = YoutubeChatPoller::new(http, key, config);
    let writer = spawn_writer(args.queue);
    let started = Instant::now();
    let deadline = args.run_for.map(|d| started + d);
    let mut next_stats = started + args.stats_every;
    eprintln!(
        "youtube_ingress: read-only, official Data API, min_poll_ms={} queue={}",
        args.min_poll_ms, args.queue
    );

    let mut exit = ExitCode::SUCCESS;
    'run: loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        let step = poller.poll_once();
        for line in step.records {
            if enqueue_youtube_record(&writer, line, &mut poller.stats)
                == EnqueueOutcome::WriterGone
            {
                eprintln!("youtube_ingress: stdout closed; exiting");
                break 'run;
            }
        }
        match step.end {
            Some(PollEnd::ChatEnded) => {
                eprintln!("youtube_ingress: live chat ended; exiting");
                break;
            }
            Some(PollEnd::Fatal(message)) => {
                eprintln!("youtube_ingress: {message}");
                exit = ExitCode::from(2);
                break;
            }
            None => {}
        }
        // Sleep in slices so the run deadline and stats stay responsive.
        let wake = Instant::now() + step.wait;
        loop {
            let now = Instant::now();
            if now >= next_stats {
                eprintln!("youtube_ingress_stats: {}", poller.stats);
                next_stats = now + args.stats_every;
            }
            let limit = deadline.map_or(wake, |d| wake.min(d));
            if now >= limit {
                break;
            }
            std::thread::sleep((limit - now).min(Duration::from_millis(500)));
        }
    }
    eprintln!("youtube_ingress_stats: final {}", poller.stats);
    exit
}
