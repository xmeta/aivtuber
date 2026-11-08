//! Read-only Twitch chat sidecar (#257).
//!
//! Joins channels with the anonymous `justinfan` login (no OAuth, no
//! credentials, and this binary never calls any send API) and writes one
//! raw-ingress JSON record per chat message to stdout:
//!
//! ```text
//! twitch-chat-ingress --channel somechannel | aivtuber-app
//! ```
//!
//! Run the app with `AIVTUBER_INGRESS_MAX_TRUST=untrusted` and
//! `AIVTUBER_INGRESS_SOURCE_CLASSES=public_chat` so the app, not this
//! sidecar, enforces what the pipe may carry. Diagnostics go to stderr only;
//! stdout carries records and nothing else.

use aivtuber_adapters::{
    EnqueueOutcome, MapOutcome, SIDECAR_RECONNECT, TwitchIngressLimits, TwitchIngressStats,
    TwitchRecordMapper, enqueue_bounded, validate_channel_login,
};
use std::io::Write;
use std::process::ExitCode;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout};
use twitch_irc::login::StaticLoginCredentials;
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};

const DEFAULT_QUEUE: usize = 256;
const DEFAULT_STALL_SECS: u64 = 600;
const DEFAULT_STATS_SECS: u64 = 30;
const MAX_CHANNELS: usize = 8;

struct Args {
    channels: Vec<String>,
    queue: usize,
    stall: Duration,
    stats_every: Duration,
    run_for: Option<Duration>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        channels: Vec::new(),
        queue: DEFAULT_QUEUE,
        stall: Duration::from_secs(DEFAULT_STALL_SECS),
        stats_every: Duration::from_secs(DEFAULT_STATS_SECS),
        run_for: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = |name: &str| iter.next().ok_or_else(|| format!("{name} needs a value"));
        let number = |text: String, name: &str| -> Result<u64, String> {
            text.parse::<u64>()
                .map_err(|_| format!("{name} must be a non-negative integer"))
        };
        match flag.as_str() {
            "--channel" => args
                .channels
                .push(validate_channel_login(&value("--channel")?)?),
            "--queue" => {
                args.queue = number(value("--queue")?, "--queue")?.clamp(1, 65_536) as usize
            }
            "--stall-secs" => {
                args.stall =
                    Duration::from_secs(number(value("--stall-secs")?, "--stall-secs")?.max(1))
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
            "-h" | "--help" => {
                return Err(
                    "usage: twitch-chat-ingress --channel <login> [--channel <login>]... \
                     [--queue N] [--stall-secs N] [--stats-secs N] [--duration-secs N]"
                        .to_owned(),
                );
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    args.channels.sort();
    args.channels.dedup();
    if args.channels.is_empty() {
        return Err("at least one --channel is required".to_owned());
    }
    if args.channels.len() > MAX_CHANNELS {
        return Err(format!("at most {MAX_CHANNELS} channels"));
    }
    Ok(args)
}

/// Stdout writer on its own thread so a slow consumer never blocks the network
/// side; the bounded queue in front of it is the only buffer.
fn spawn_writer(capacity: usize) -> SyncSender<String> {
    let (sender, receiver) = sync_channel::<String>(capacity);
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for line in receiver {
            if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                break; // consumer closed the pipe; dropping the receiver stops the sidecar
            }
        }
    });
    sender
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let writer = spawn_writer(args.queue);
    let mut mapper = TwitchRecordMapper::new(TwitchIngressLimits::default());
    let mut stats = TwitchIngressStats::default();
    let started = Instant::now();
    let deadline = args.run_for.map(|run_for| started + run_for);
    let mut next_stats = started + args.stats_every;
    let mut failures: u32 = 0;
    eprintln!(
        "twitch_ingress: anonymous read-only channels={:?} queue={}",
        args.channels, args.queue
    );

    'session: loop {
        // Anonymous `justinfan` login: no credentials exist to leak or misuse.
        let config = ClientConfig::default();
        let (mut incoming, client) =
            TwitchIRCClient::<SecureTCPTransport, StaticLoginCredentials>::new(config);
        for channel in &args.channels {
            if let Err(error) = client.join(channel.clone()) {
                eprintln!("twitch_ingress: cannot join {channel}: {error}");
                return ExitCode::from(2);
            }
        }

        loop {
            let now = Instant::now();
            if deadline.is_some_and(|deadline| now >= deadline) {
                break 'session;
            }
            if now >= next_stats {
                eprintln!("twitch_ingress_stats: {stats}");
                next_stats = now + args.stats_every;
            }
            let wait = [
                Some(args.stall),
                Some(next_stats - now),
                deadline.map(|d| d - now),
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(args.stall);
            match timeout(wait, incoming.recv()).await {
                Ok(Some(message)) => {
                    failures = 0;
                    match mapper.map(&message) {
                        MapOutcome::Record(line) => {
                            if enqueue_bounded(&writer, line, &mut stats)
                                == EnqueueOutcome::WriterGone
                            {
                                eprintln!("twitch_ingress: stdout closed; exiting");
                                break 'session;
                            }
                        }
                        MapOutcome::Skipped(reason) => stats.record_skip(reason),
                    }
                }
                Ok(None) => break, // client ended; rebuild below
                Err(_elapsed) => {
                    // Not a stall unless the whole stall window passed silently;
                    // the timer also wakes for stats and the run deadline.
                    if Instant::now().duration_since(now) >= args.stall {
                        eprintln!("twitch_ingress: no server traffic for {:?}", args.stall);
                        break;
                    }
                }
            }
        }

        // Rebuild the client with capped exponential backoff.
        failures = failures.saturating_add(1);
        stats.reconnects = stats.reconnects.saturating_add(1);
        let delay = SIDECAR_RECONNECT.delay_ms(failures);
        eprintln!("twitch_ingress: reconnecting in {delay} ms (attempt {failures})");
        drop(client);
        sleep(Duration::from_millis(delay)).await;
    }

    eprintln!("twitch_ingress_stats: final {stats}");
    ExitCode::SUCCESS
}
