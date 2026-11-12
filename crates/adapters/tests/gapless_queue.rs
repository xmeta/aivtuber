//! #83 S3d falsifier: the persistent-player queue starts a continuation phrase
//! on the very next sample after the previous phrase, and a stop drops what is
//! queued. Device-free: the queue runs on rodio's offline mixer, which this
//! test pulls sample by sample, so the numbers below are exact, not timed.

#![cfg(feature = "gapless-queue")]

use aivtuber_adapters::{
    AudioPlayRequest, AudioPlayer, AudioStopOutcome, GaplessQueue, MAX_QUEUED_PHRASES,
};
use aivtuber_domain::EngineErrorKind;
use rodio::mixer::{MixerSource, mixer};
use std::fs;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const A: i16 = 8_192; // 0.25
const B: i16 = -16_384; // -0.5
const C: i16 = 24_576; // 0.75

fn a() -> f32 {
    f32::from(A) / 32_768.0
}
fn b() -> f32 {
    f32::from(B) / 32_768.0
}
fn c() -> f32 {
    f32::from(C) / 32_768.0
}

/// Mono 16-bit 48 kHz WAV whose every sample is `value`.
fn write_wav(root: &Path, name: &str, value: i16, samples: u32) {
    let data_bytes = samples * 2;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for _ in 0..samples {
        wav.extend_from_slice(&value.to_le_bytes());
    }
    fs::write(root.join(name), wav).expect("wav");
}

struct Rig {
    queue: GaplessQueue,
    out: MixerSource,
}

fn rig(test: &str) -> (Rig, PathBuf) {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("gapless-{test}"));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("root");
    let (mixer, out) = mixer(
        NonZero::new(1).expect("mono"),
        NonZero::new(RATE).expect("rate"),
    );
    let queue = GaplessQueue::new(&root, mixer).expect("queue");
    (Rig { queue, out }, root)
}

impl Rig {
    fn play(&self, name: &str, speed_factor: f64, continuation: bool) {
        self.try_play(name, speed_factor, continuation)
            .expect("play");
    }

    fn try_play(
        &self,
        name: &str,
        speed_factor: f64,
        continuation: bool,
    ) -> Result<(), aivtuber_domain::EngineError> {
        self.queue.play(&AudioPlayRequest {
            audio_ref: format!("audio://{name}"),
            speed_factor,
            continuation,
        })
    }

    fn pull(&mut self, n: usize) -> Vec<f32> {
        (&mut self.out).take(n).collect()
    }
}

/// Index of the first sample that is not silence.
fn first_audible(samples: &[f32]) -> usize {
    samples
        .iter()
        .position(|sample| *sample != 0.0)
        .expect("audible samples")
}

/// Length of the run of `value` that starts at `at`.
fn run_of(samples: &[f32], at: usize, value: f32) -> usize {
    samples[at..]
        .iter()
        .take_while(|sample| **sample == value)
        .count()
}

#[test]
fn a_continuation_starts_on_the_sample_after_the_previous_phrase() {
    let (mut rig, root) = rig("exact");
    let (n, m) = (4_800_u32, 3_600_u32);
    write_wav(&root, "a.wav", A, n);
    write_wav(&root, "b.wav", B, m);
    rig.play("a.wav", 1.0, false);
    rig.play("b.wav", 1.0, true);

    let samples = rig.pull(20_000);
    let a_start = first_audible(&samples);
    let a_len = run_of(&samples, a_start, a());
    let b_start = a_start + a_len;
    assert_eq!(a_len, n as usize, "phrase A is intact");
    assert_eq!(
        samples[b_start],
        b(),
        "B's first sample is at index A_start + N, with no zero sample between"
    );
    assert_eq!(
        run_of(&samples, b_start, b()),
        m as usize,
        "phrase B intact"
    );
    assert!(
        samples[b_start + m as usize..].iter().all(|s| *s == 0.0),
        "silence after the last phrase"
    );
    eprintln!(
        "gapless_exact: a_start={a_start} a_len={a_len} b_first_index={b_start} gap_samples={}",
        b_start - (a_start + n as usize)
    );
}

#[test]
fn three_phrases_chain_without_a_zero_sample_between_them() {
    let (mut rig, root) = rig("chain");
    write_wav(&root, "a.wav", A, 1_000);
    write_wav(&root, "b.wav", B, 777);
    write_wav(&root, "c.wav", C, 2_001);
    rig.play("a.wav", 1.0, false);
    rig.play("b.wav", 1.0, true);
    rig.play("c.wav", 1.0, true);
    let samples = rig.pull(6_000);
    let start = first_audible(&samples);
    let body = &samples[start..start + 1_000 + 777 + 2_001];
    assert!(body.iter().all(|s| *s != 0.0), "no zero inside the chain");
    assert_eq!(body.iter().filter(|s| **s == a()).count(), 1_000, "A count");
    assert_eq!(body.iter().filter(|s| **s == b()).count(), 777, "B count");
    assert_eq!(body.iter().filter(|s| **s == c()).count(), 2_001, "C count");
    assert_eq!(body[1_000], b());
    assert_eq!(body[1_777], c());
}

/// Contrast: the same two phrases when the second one only reaches the
/// backend after the first has ended (what a hand-over at the logical start
/// does) leave exactly the arrival delay as silence, so the assertion above
/// is able to see a gap.
#[test]
fn a_late_hand_over_leaves_exactly_the_arrival_delay_as_silence() {
    let (mut rig, root) = rig("late");
    let (n, delay) = (4_800_usize, 480_usize);
    write_wav(&root, "a.wav", A, n as u32);
    write_wav(&root, "b.wav", B, 1_200);
    rig.play("a.wav", 1.0, false);
    let first = rig.pull(first_audible_budget() + n + delay);
    let pulled = first.len();
    // The second phrase arrives `delay` samples after A ended.
    rig.play("b.wav", 1.0, true);
    let second = rig.pull(4_000);
    let mut samples = first;
    samples.extend(second);

    let a_start = first_audible(&samples);
    let a_end = a_start + run_of(&samples, a_start, a());
    let b_start = samples
        .iter()
        .position(|s| *s == b())
        .expect("B is audible");
    assert_eq!(a_end - a_start, n);
    let gap = b_start - a_end;
    let arrival_gap = pulled - a_end;
    assert!(
        gap >= arrival_gap,
        "a late arrival leaves at least the arrival delay, got {gap} < {arrival_gap}"
    );
    // rodio's idle queue plays silence in spans of 1024 samples, so a late
    // phrase can wait up to one span more.
    assert!(
        gap <= arrival_gap + 1_024,
        "gap {gap}, arrival {arrival_gap}"
    );
    assert!(
        samples[a_end..b_start].iter().all(|s| *s == 0.0),
        "the gap is silence"
    );
    eprintln!(
        "gapless_contrast: late hand-over gap_samples={gap} arrival_gap_samples={arrival_gap}"
    );
}

/// Upper bound on the leading silence before the first phrase starts.
fn first_audible_budget() -> usize {
    256
}

#[test]
fn the_queue_is_bounded_and_refuses_with_a_counted_typed_outcome() {
    let (rig, root) = rig("bound");
    write_wav(&root, "a.wav", A, 4_800);
    rig.play("a.wav", 1.0, false);
    for _ in 1..MAX_QUEUED_PHRASES {
        rig.play("a.wav", 1.0, true);
    }
    assert_eq!(rig.queue.queued(), MAX_QUEUED_PHRASES);
    let error = rig.try_play("a.wav", 1.0, true).expect_err("full");
    assert_eq!(error.kind, EngineErrorKind::Overloaded);
    assert_eq!(rig.queue.refused_queue_full(), 1);
    assert_eq!(
        rig.queue.queued(),
        MAX_QUEUED_PHRASES,
        "nothing was dropped or added"
    );
    assert!(rig.queue.supports_gapless_queue());
}

#[test]
fn speed_applies_to_the_phrase_it_belongs_to_only() {
    let (mut rig, root) = rig("speed");
    write_wav(&root, "a.wav", A, 4_800);
    write_wav(&root, "b.wav", B, 4_800);
    rig.play("a.wav", 1.0, false);
    // speed_factor 0.5 plays twice as fast.
    rig.play("b.wav", 0.5, true);
    let samples = rig.pull(20_000);
    let a_start = first_audible(&samples);
    assert_eq!(
        run_of(&samples, a_start, a()),
        4_800,
        "phrase A keeps its own speed"
    );
    // A speed other than 1.0 goes through rodio's resampler, so only the
    // length (about half) and the absence of a gap are pinned, not every sample.
    assert!(
        (samples[a_start + 4_800] - b()).abs() < 0.05,
        "B still starts right behind A"
    );
    let b_len = samples.iter().filter(|s| (**s - b()).abs() < 1e-3).count();
    assert!(
        (2_300..=2_800).contains(&b_len),
        "phrase B plays at about twice the speed, got {b_len} samples"
    );
}

#[test]
fn a_stop_drops_queued_phrases_at_once_and_the_next_play_builds_a_new_player() {
    let (mut rig, root) = rig("stop");
    write_wav(&root, "a.wav", A, 96_000);
    write_wav(&root, "b.wav", B, 4_800);
    write_wav(&root, "c.wav", C, 4_800);
    rig.play("a.wav", 1.0, false);
    rig.play("b.wav", 1.0, true);
    rig.play("c.wav", 1.0, true);
    let before = rig.pull(1_000);
    assert!(before.contains(&a()));

    let started = Instant::now();
    let outcome = rig.queue.stop(120).expect("stop");
    let call = started.elapsed();
    assert_eq!(outcome, AudioStopOutcome::Faded { fade_ms: 120 });
    assert!(
        call < Duration::from_millis(50),
        "stop returned in {call:?}"
    );
    assert_eq!(rig.queue.queued(), 0, "the player is gone from the queue");

    // Whatever is pulled while A fades out, B and C never start.
    let during = rig.pull(30_000);
    assert!(
        during.iter().all(|s| *s >= 0.0 && *s != b() && *s != c()),
        "no queued-ahead phrase starts after the stop"
    );
    std::thread::sleep(Duration::from_millis(250));
    let after = rig.pull(4_800);
    // A stop reaches a source at its next 5 ms periodic access (240 samples).
    assert!(
        after.iter().skip(480).all(|s| *s == 0.0),
        "silent once faded"
    );

    // The next play is a fresh player and audible.
    rig.play("c.wav", 1.0, false);
    assert_eq!(rig.queue.queued(), 1);
    let again = rig.pull(2_000);
    assert!(again.contains(&c()));
    eprintln!("gapless_stop: call_micros={}", call.as_micros());
}

#[test]
fn a_hard_stop_and_an_idle_stop_report_honestly() {
    let (mut rig, root) = rig("hard");
    write_wav(&root, "a.wav", A, 96_000);
    write_wav(&root, "b.wav", B, 4_800);
    assert_eq!(
        rig.queue.stop(120).expect("idle"),
        AudioStopOutcome::NothingPlaying
    );
    rig.play("a.wav", 1.0, false);
    rig.play("b.wav", 1.0, true);
    let _ = rig.pull(500);
    assert_eq!(
        rig.queue.stop(0).expect("hard"),
        AudioStopOutcome::HardStopped
    );
    let after = rig.pull(20_000);
    assert!(
        after.iter().skip(500).all(|s| *s != b()),
        "queued-ahead B never starts"
    );
}

#[test]
fn a_fresh_performance_replaces_what_is_still_playing() {
    let (mut rig, root) = rig("replace");
    write_wav(&root, "a.wav", A, 96_000);
    write_wav(&root, "c.wav", C, 4_800);
    rig.play("a.wav", 1.0, false);
    let _ = rig.pull(500);
    rig.play("c.wav", 1.0, false);
    assert_eq!(rig.queue.queued(), 1, "the old player was replaced");
    let samples = rig.pull(30_000);
    assert!(samples.contains(&c()));
    let last_a = samples.iter().rposition(|s| *s == a());
    let first_c = samples.iter().position(|s| *s == c()).expect("c");
    assert!(
        last_a.is_none_or(|index| index < first_c),
        "A does not play on under C"
    );
}
