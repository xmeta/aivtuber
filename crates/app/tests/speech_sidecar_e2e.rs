//! Issue #92 S2: the app's sidecar supervisor against the protocol double.
//! No microphone, model or audio is involved.

use aivtuber_app::{SpeechSidecar, SpeechSidecarConfig, SpeechSidecarState};
use aivtuber_runtime::{
    IngressPolicy, SpeechObservationConfig, SpeechObservationOutcome, SpeechObservationTracker,
};
use std::time::{Duration, Instant};

const DOUBLE: &str = env!("CARGO_BIN_EXE_speech-sidecar-double");

fn tracker(max_age_ms: u64) -> SpeechObservationTracker {
    let config = SpeechObservationConfig {
        max_age_ms,
        max_future_skew_ms: 1_000,
        ..SpeechObservationConfig::default()
    };
    SpeechObservationTracker::new(config, IngressPolicy::default()).expect("tracker")
}

fn spawn(mode: &str, resync_ms: u64, started: Instant) -> SpeechSidecar {
    let mut config = SpeechSidecarConfig::new(DOUBLE, vec![mode.to_owned()]);
    config.resync_interval = Duration::from_millis(resync_ms);
    SpeechSidecar::spawn(config, started).expect("spawn double")
}

fn poll_until(
    sidecar: &mut SpeechSidecar,
    tracker: &mut SpeechObservationTracker,
    started: Instant,
    limit: Duration,
    mut done: impl FnMut(&SpeechSidecar) -> bool,
) -> Vec<u64> {
    let mut ages = Vec::new();
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline && !done(sidecar) {
        let now_ms = started.elapsed().as_millis() as u64;
        sidecar.poll(tracker, now_ms, |_, report| {
            for outcome in &report.outcomes {
                if let SpeechObservationOutcome::Accepted { age_ms } = outcome {
                    ages.push(*age_ms);
                }
            }
        });
        std::thread::sleep(Duration::from_millis(2));
    }
    ages
}

/// The handshake: the child stamps each observation with the exact runtime
/// time the app sent, so the tracker's age is only the pipe and polling
/// latency. Re-sync keeps arriving on the configured schedule.
#[test]
fn clock_handshake_puts_child_stamps_on_the_runtime_timeline() {
    let started = Instant::now();
    let mut tracker = tracker(1_000);
    let mut sidecar = spawn("clock", 250, started);
    let ages = poll_until(
        &mut sidecar,
        &mut tracker,
        started,
        Duration::from_secs(5),
        |s| s.stats().batches_ingested >= 3,
    );
    let stats = sidecar.stats();
    assert!(stats.batches_ingested >= 3, "{stats:?}");
    assert!(
        stats.clock_syncs_sent >= 3,
        "re-sync on schedule: {stats:?}"
    );
    assert!(!ages.is_empty());
    // Ages are sync-send to ingest latency; an unmapped timeline would be
    // thousands of ms (stale) or future dated.
    assert!(ages.iter().all(|a| *a < 500), "{ages:?}");
    assert_eq!(tracker.metrics().rejected_stale_age, 0);
    assert_eq!(tracker.metrics().rejected_future_dated, 0);
    assert_eq!(tracker.metrics().provider_unavailable, 0);
}

#[test]
fn child_crash_degrades_once_without_blocking() {
    let started = Instant::now();
    let mut tracker = tracker(1_000);
    let mut sidecar = spawn("crash", 1_000, started);
    let began = Instant::now();
    poll_until(
        &mut sidecar,
        &mut tracker,
        started,
        Duration::from_secs(5),
        |s| s.is_degraded(),
    );
    assert!(sidecar.is_degraded());
    assert!(began.elapsed() < Duration::from_secs(4));
    // The batch sent before the crash was still delivered.
    assert_eq!(sidecar.stats().batches_ingested, 1);
    assert_eq!(tracker.metrics().provider_unavailable, 1);
    // Degraded is terminal and not re-counted by further polling.
    for _ in 0..10 {
        sidecar.poll(&mut tracker, 0, |_, _| {});
    }
    assert_eq!(tracker.metrics().provider_unavailable, 1);
    assert!(matches!(sidecar.state(), SpeechSidecarState::Degraded(r) if r.contains("exited")));
}

#[test]
fn a_child_that_cannot_start_is_an_error_not_a_panic() {
    let config = SpeechSidecarConfig::new("definitely-not-a-real-sidecar-binary", vec![]);
    assert!(SpeechSidecar::spawn(config, Instant::now()).is_err());
}

/// A child that only sends garbage is stopped after a bounded number of bad
/// lines and counted once as unavailable; the loop keeps its pace meanwhile.
#[test]
fn garbage_flood_is_bounded_then_degrades() {
    let started = Instant::now();
    let mut tracker = tracker(1_000);
    let mut sidecar = spawn("garbage", 1_000, started);
    let mut slowest_poll = Duration::ZERO;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !sidecar.is_degraded() {
        let before = Instant::now();
        sidecar.poll(&mut tracker, 0, |_, _| {});
        slowest_poll = slowest_poll.max(before.elapsed());
    }
    assert!(sidecar.is_degraded());
    assert_eq!(tracker.metrics().provider_unavailable, 1);
    assert!(tracker.metrics().batches_rejected_invalid >= 32);
    // One poll handles at most 64 lines and never waits on the child.
    assert!(
        slowest_poll < Duration::from_millis(500),
        "{slowest_poll:?}"
    );
}

/// The reader runs at full speed against a flooding child: queued lines stay
/// bounded, excess is dropped and counted, and each poll does bounded work.
#[test]
fn valid_batch_flood_is_dropped_at_the_queue_and_polls_stay_bounded() {
    let started = Instant::now();
    let mut tracker = tracker(1_000);
    let mut sidecar = spawn("flood", 1_000, started);
    // Do not poll for a moment so the queue (64) fills and drops start.
    std::thread::sleep(Duration::from_millis(500));
    let mut slowest_poll = Duration::ZERO;
    for _ in 0..50 {
        let before = Instant::now();
        sidecar.poll(&mut tracker, 5_000, |_, _| {});
        slowest_poll = slowest_poll.max(before.elapsed());
    }
    let stats = sidecar.stats();
    assert!(stats.lines_dropped_queue_full > 1_000, "{stats:?}");
    assert!(stats.batches_ingested <= 50 * 64, "{stats:?}");
    assert!(
        slowest_poll < Duration::from_millis(500),
        "{slowest_poll:?}"
    );
    // Stale by design (observed at 0, now 5000), so rejected by age, not
    // accepted, and the child is not treated as invalid.
    assert!(!sidecar.is_degraded());
    assert!(tracker.metrics().rejected_stale_age > 0);
}

#[test]
fn oversize_lines_are_dropped_without_unbounded_memory() {
    let started = Instant::now();
    let mut tracker = tracker(1_000);
    let mut sidecar = spawn("oversize", 1_000, started);
    std::thread::sleep(Duration::from_millis(400));
    sidecar.poll(&mut tracker, 0, |_, _| {});
    let stats = sidecar.stats();
    assert!(stats.lines_dropped_oversize > 10, "{stats:?}");
    assert_eq!(stats.batches_ingested, 0);
}

/// A silent child is not a failure (silence is normal) and does not block.
#[test]
fn a_silent_child_neither_blocks_nor_degrades_and_dies_on_shutdown() {
    let started = Instant::now();
    let mut tracker = tracker(1_000);
    let mut sidecar = spawn("hang", 1_000, started);
    for _ in 0..20 {
        sidecar.poll(&mut tracker, 1, |_, _| {});
    }
    assert!(!sidecar.is_degraded());
    let before = Instant::now();
    sidecar.shutdown();
    assert!(before.elapsed() < Duration::from_secs(2));
    assert_eq!(tracker.metrics().provider_unavailable, 0);
}

/// The operator ceiling is enforced on this path too: if `speech` is not an
/// admitted source class, every batch is rejected before parsing and the
/// useless child is stopped.
#[test]
fn operator_ceiling_without_speech_rejects_batches_and_stops_the_child() {
    let started = Instant::now();
    let policy = IngressPolicy::parse(Some("untrusted"), Some("public_chat")).expect("policy");
    let mut tracker =
        SpeechObservationTracker::new(SpeechObservationConfig::default(), policy).expect("tracker");
    let mut sidecar = spawn("garbage", 1_000, started);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !sidecar.is_degraded() {
        sidecar.poll(&mut tracker, 0, |_, _| {});
    }
    assert!(sidecar.is_degraded());
    assert!(tracker.metrics().batches_rejected_ingress_policy >= 32);
    assert_eq!(tracker.metrics().observations_accepted, 0);
}
