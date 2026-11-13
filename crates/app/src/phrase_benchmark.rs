//! Phrase-mode against buffered-mode latency benchmark (issue #83 S3e, feeding
//! #58).
//!
//! The comparison is a deterministic logical-time model, not a wall-clock
//! measurement: the Thinking and TTS stubs are described by declared costs
//! (`PhraseModelParams`), the phrase partition comes from the production
//! `phrase-seg-v1` segmenter (buffered `segment_reply` and the streamed
//! `IncrementalSegmenter`), and the commit timeline applies the production
//! lookahead rule. Nothing reads a clock, so two runs give identical bytes and
//! a CI runner's load cannot move a number. The model is tied to the real
//! pipeline by a test that runs the real phrased `ProductionApp` on the same
//! reply and requires the same phrases and the same start/end times.
//!
//! Lookahead rule (production: `wait_for_slot`, `PhraseCommit`): phrase `k`
//! is synthesized only once phrase `k - 1` has started playing, and it starts
//! when both its audio is ready and phrase `k - 1` has ended.
//!
//! Three modes, kept apart by `mode` so they never share a result series with
//! each other or with the `ComparisonMode` replay results:
//!
//! * `phrase_mode_buffered`: whole reply, then one TTS call for all of it;
//! * `phrase_mode_phrased`: buffered Thinking, phrase-level commit;
//! * `phrase_mode_streamed`: streamed Thinking feeding the incremental
//!   segmenter, phrase-level commit.

use aivtuber_generative::{IncrementalSegmenter, SegmentationConfig, segment_reply};
use aivtuber_telemetry::{
    BenchmarkConfiguration, BenchmarkEnvironment, BenchmarkGit, BenchmarkResult, InvariantValue,
    MetricValue, RESULT_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PHRASE_BENCH_SUITE: &str = "phrase-comparison";
pub const PHRASE_BENCH_CONFIG_VERSION: &str = "phrase-benchmark-v1";
/// Upper bounds that keep a hand-edited fixture from turning the benchmark into
/// an unbounded run.
pub const MAX_PHRASE_BENCH_REPLIES: usize = 256;
pub const MAX_PHRASE_BENCH_REPLY_CHARS: usize = 4096;

/// Declared cost of the stub engines, in logical milliseconds. They are model
/// inputs, not measurements of any real model or voice: the fixture documents
/// that and the result's `configuration` records them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhraseModelParams {
    /// Request start to the first streamed character.
    pub thinking_first_token_ms: u64,
    pub thinking_ms_per_char: u64,
    /// Characters per streamed delta (the stub SSE server sends 3).
    pub thinking_delta_chars: usize,
    pub tts_base_ms: u64,
    pub tts_ms_per_char: u64,
    /// Length of the audio the stub TTS returns, per character.
    pub audio_ms_per_char: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhraseBenchReply {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhraseFixture {
    pub dataset_id: String,
    pub seed: u64,
    pub models: PhraseModelParams,
    pub replies: Vec<PhraseBenchReply>,
}

impl PhraseFixture {
    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        let fixture: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("phrase benchmark fixture is not valid: {error}"))?;
        fixture.validate()?;
        Ok(fixture)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.dataset_id.trim().is_empty() {
            return Err("phrase benchmark dataset_id must not be empty".to_owned());
        }
        if self.replies.is_empty() || self.replies.len() > MAX_PHRASE_BENCH_REPLIES {
            return Err(format!(
                "phrase benchmark needs 1 to {MAX_PHRASE_BENCH_REPLIES} replies"
            ));
        }
        if self.models.thinking_delta_chars == 0 {
            return Err("thinking_delta_chars must be positive".to_owned());
        }
        if self.models.audio_ms_per_char == 0 {
            return Err("audio_ms_per_char must be positive".to_owned());
        }
        for reply in &self.replies {
            let chars = reply.text.chars().count();
            if reply.id.trim().is_empty() || reply.text.trim().is_empty() {
                return Err("phrase benchmark replies need a non-empty id and text".to_owned());
            }
            if chars > MAX_PHRASE_BENCH_REPLY_CHARS {
                return Err(format!(
                    "reply {:?} exceeds {MAX_PHRASE_BENCH_REPLY_CHARS} characters",
                    reply.id
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PhraseBenchMode {
    Buffered,
    Phrased,
    Streamed,
}

impl PhraseBenchMode {
    pub const ALL: [Self; 3] = [Self::Buffered, Self::Phrased, Self::Streamed];

    /// The result `mode`: part of the comparator's compatibility key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buffered => "phrase_mode_buffered",
            Self::Phrased => "phrase_mode_phrased",
            Self::Streamed => "phrase_mode_streamed",
        }
    }

    pub fn file_stem(self) -> &'static str {
        match self {
            Self::Buffered => "phrase-mode-buffered",
            Self::Phrased => "phrase-mode-phrased",
            Self::Streamed => "phrase-mode-streamed",
        }
    }
}

/// One phrase (or, in buffered mode, the whole reply) on the logical timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhraseTiming {
    pub chars: usize,
    pub start_ms: u64,
    pub end_ms: u64,
    /// Silence between the previous phrase's end and this start; `0` for the
    /// first phrase and for gapless continuations.
    pub gap_before_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTimeline {
    pub reply_id: String,
    pub phrases: Vec<PhraseTiming>,
}

impl ReplyTimeline {
    /// Request start to the first audible sample.
    pub fn first_audio_ms(&self) -> u64 {
        self.phrases.first().map_or(0, |phrase| phrase.start_ms)
    }

    /// Request start to the end of the last committed phrase's audio.
    pub fn completion_ms(&self) -> u64 {
        self.phrases.last().map_or(0, |phrase| phrase.end_ms)
    }
}

fn synthesis_ms(params: &PhraseModelParams, chars: usize) -> u64 {
    params
        .tts_base_ms
        .saturating_add(params.tts_ms_per_char.saturating_mul(chars as u64))
}

fn audio_ms(params: &PhraseModelParams, chars: usize) -> u64 {
    params.audio_ms_per_char.saturating_mul(chars as u64)
}

fn thinking_ms(params: &PhraseModelParams, chars: usize) -> u64 {
    params
        .thinking_first_token_ms
        .saturating_add(params.thinking_ms_per_char.saturating_mul(chars as u64))
}

/// Phrases with the logical time at which each became available to the
/// commit side (the time its text was complete).
fn available_phrases(
    mode: PhraseBenchMode,
    text: &str,
    params: &PhraseModelParams,
    segmentation: &SegmentationConfig,
) -> Vec<(String, u64)> {
    let total = text.chars().count();
    match mode {
        PhraseBenchMode::Buffered => vec![(text.trim().to_owned(), thinking_ms(params, total))],
        PhraseBenchMode::Phrased => {
            let ready = thinking_ms(params, total);
            segment_reply(text, segmentation)
                .into_iter()
                .map(|phrase| (phrase, ready))
                .collect()
        }
        PhraseBenchMode::Streamed => {
            let chars: Vec<char> = text.chars().collect();
            let mut segmenter = IncrementalSegmenter::new(*segmentation);
            let mut out = Vec::new();
            let mut seen = 0_usize;
            for delta in chars.chunks(params.thinking_delta_chars) {
                seen += delta.len();
                let at = thinking_ms(params, seen);
                let delta: String = delta.iter().collect();
                out.extend(segmenter.push(&delta).into_iter().map(|p| (p, at)));
            }
            let end = thinking_ms(params, total);
            out.extend(segmenter.finish().into_iter().map(|p| (p, end)));
            out
        }
    }
}

/// The deterministic commit timeline of one reply.
pub fn simulate_reply(
    mode: PhraseBenchMode,
    reply: &PhraseBenchReply,
    params: &PhraseModelParams,
    segmentation: &SegmentationConfig,
) -> ReplyTimeline {
    let mut phrases: Vec<PhraseTiming> = Vec::new();
    for (text, available_ms) in available_phrases(mode, &reply.text, params, segmentation) {
        let chars = text.chars().count();
        // Synthesis of phrase k begins once its text exists and phrase k - 1
        // has started playing (one phrase of lookahead).
        let begin_ms = phrases
            .last()
            .map_or(available_ms, |previous| available_ms.max(previous.start_ms));
        let ready_ms = begin_ms.saturating_add(synthesis_ms(params, chars));
        let previous_end = phrases.last().map(|previous| previous.end_ms);
        let start_ms = previous_end.map_or(ready_ms, |end| ready_ms.max(end));
        phrases.push(PhraseTiming {
            chars,
            start_ms,
            end_ms: start_ms.saturating_add(audio_ms(params, chars)),
            gap_before_ms: previous_end.map_or(0, |end| start_ms - end),
        });
    }
    ReplyTimeline {
        reply_id: reply.id.clone(),
        phrases,
    }
}

/// Nearest-rank percentile over unsorted samples; `0` for no samples.
fn nearest_rank(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Distribution {
    pub p50: u64,
    pub p90: u64,
    pub max: u64,
}

impl Distribution {
    fn of(mut samples: Vec<u64>) -> Self {
        samples.sort_unstable();
        Self {
            p50: nearest_rank(&samples, 50),
            p90: nearest_rank(&samples, 90),
            max: samples.last().copied().unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhraseModeSummary {
    pub mode: PhraseBenchMode,
    pub responses: usize,
    pub first_audio_ms: Distribution,
    /// Whole-response completion: request start to the end of the last
    /// committed phrase's audio.
    pub completion_ms: Distribution,
    pub phrases_per_response_mean: f64,
    pub phrases_total: usize,
    /// Inter-phrase gaps over every adjacent phrase pair of every response
    /// (a response with one phrase has none).
    pub gap_samples: usize,
    pub gap_mean_ms: f64,
    pub gap_max_ms: u64,
    /// Replies whose phrase partition differs from the buffered
    /// `phrase-seg-v1` cut (must be 0: streaming never changes the cuts).
    pub partition_mismatch_count: usize,
    pub timelines: Vec<ReplyTimeline>,
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

pub fn summarize(
    mode: PhraseBenchMode,
    fixture: &PhraseFixture,
    segmentation: &SegmentationConfig,
) -> PhraseModeSummary {
    let timelines: Vec<ReplyTimeline> = fixture
        .replies
        .iter()
        .map(|reply| simulate_reply(mode, reply, &fixture.models, segmentation))
        .collect();
    let first_audio = Distribution::of(timelines.iter().map(|t| t.first_audio_ms()).collect());
    let completion = Distribution::of(timelines.iter().map(|t| t.completion_ms()).collect());
    let phrases_total: usize = timelines.iter().map(|t| t.phrases.len()).sum();
    let gaps: Vec<u64> = timelines
        .iter()
        .flat_map(|t| t.phrases.iter().skip(1).map(|p| p.gap_before_ms))
        .collect();
    let gap_total: u64 = gaps.iter().sum();
    let partition_mismatch_count = if mode == PhraseBenchMode::Buffered {
        0
    } else {
        fixture
            .replies
            .iter()
            .zip(&timelines)
            .filter(|(reply, timeline)| {
                let expected: Vec<usize> = segment_reply(&reply.text, segmentation)
                    .iter()
                    .map(|phrase| phrase.chars().count())
                    .collect();
                let actual: Vec<usize> = timeline.phrases.iter().map(|p| p.chars).collect();
                expected != actual
            })
            .count()
    };
    PhraseModeSummary {
        mode,
        responses: timelines.len(),
        first_audio_ms: first_audio,
        completion_ms: completion,
        phrases_per_response_mean: round2(phrases_total as f64 / timelines.len().max(1) as f64),
        phrases_total,
        gap_samples: gaps.len(),
        gap_mean_ms: if gaps.is_empty() {
            0.0
        } else {
            round2(gap_total as f64 / gaps.len() as f64)
        },
        gap_max_ms: gaps.iter().copied().max().unwrap_or(0),
        partition_mismatch_count,
        timelines,
    }
}

/// Provenance the producer binary supplies; the model itself reads nothing from
/// the environment.
#[derive(Debug, Clone)]
pub struct PhraseBenchProvenance {
    pub git_commit: String,
    pub rust_version: String,
    pub bun_version: Option<String>,
    pub cargo_profile: String,
}

fn metric(value: f64, samples: usize) -> MetricValue {
    MetricValue {
        value,
        sample_count: Some(samples as u64),
        run_range: None,
    }
}

/// The `schemas/benchmark-result.schema.json` document of one mode. The
/// metrics are informational: no budget names them, so the comparator reports
/// them without gating (docs/benchmark-gate.adoc, phrase-mode section).
pub fn to_result(
    summary: &PhraseModeSummary,
    fixture: &PhraseFixture,
    provenance: &PhraseBenchProvenance,
) -> BenchmarkResult {
    let n = summary.responses;
    let mut metrics = BTreeMap::new();
    for (name, value) in [
        ("phrase.first_audio.p50_ms", summary.first_audio_ms.p50),
        ("phrase.first_audio.p90_ms", summary.first_audio_ms.p90),
        ("phrase.first_audio.max_ms", summary.first_audio_ms.max),
        ("phrase.completion.p50_ms", summary.completion_ms.p50),
        ("phrase.completion.p90_ms", summary.completion_ms.p90),
        ("phrase.completion.max_ms", summary.completion_ms.max),
    ] {
        metrics.insert(name.to_owned(), metric(value as f64, n));
    }
    metrics.insert(
        "phrase.phrases_per_response.mean_count".to_owned(),
        metric(summary.phrases_per_response_mean, n),
    );
    metrics.insert(
        "phrase.inter_phrase_gap.mean_ms".to_owned(),
        metric(summary.gap_mean_ms, summary.gap_samples),
    );
    metrics.insert(
        "phrase.inter_phrase_gap.max_ms".to_owned(),
        metric(summary.gap_max_ms as f64, summary.gap_samples),
    );

    let invariant = |value: usize| InvariantValue {
        value: value as u64,
        detail: None,
    };
    let invariants = BTreeMap::from([
        (
            "phrase.partition_mismatch_count".to_owned(),
            invariant(summary.partition_mismatch_count),
        ),
        (
            "phrase.start_before_previous_end_count".to_owned(),
            invariant(
                summary
                    .timelines
                    .iter()
                    .flat_map(|t| t.phrases.windows(2))
                    .filter(|pair| pair[1].start_ms < pair[0].end_ms)
                    .count(),
            ),
        ),
    ]);

    let m = &fixture.models;
    BenchmarkResult {
        schema_version: RESULT_SCHEMA_VERSION.to_owned(),
        benchmark_suite: PHRASE_BENCH_SUITE.to_owned(),
        mode: summary.mode.as_str().to_owned(),
        dataset_id: Some(fixture.dataset_id.clone()),
        recording: None,
        git: BenchmarkGit {
            commit: provenance.git_commit.clone(),
            base_commit: None,
        },
        environment: BenchmarkEnvironment {
            os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            cpu: None,
            rust_version: provenance.rust_version.clone(),
            bun_version: provenance.bun_version.clone(),
            cargo_profile: provenance.cargo_profile.clone(),
        },
        configuration: BenchmarkConfiguration {
            // Every model input is part of the compatibility key, so a changed
            // stub cost starts a new series instead of silently mixing.
            config_version: format!(
                "{PHRASE_BENCH_CONFIG_VERSION};segmentation={};think_first_ms={};think_ms_per_char={};delta_chars={};tts_base_ms={};tts_ms_per_char={};audio_ms_per_char={}",
                aivtuber_generative::PHRASE_SEGMENTATION_VERSION,
                m.thinking_first_token_ms,
                m.thinking_ms_per_char,
                m.thinking_delta_chars,
                m.tts_base_ms,
                m.tts_ms_per_char,
                m.audio_ms_per_char,
            ),
            runtime_profile: Some("local".to_owned()),
            asset_version: "phrase-model-stub-v1".to_owned(),
            index_version: None,
            retriever_version: None,
            jev_model: None,
            thinking_model: Some("phrase-benchmark-thinking-v1".to_owned()),
            tts_model: Some("phrase-benchmark-tts-v1".to_owned()),
            cost_model_version: None,
            seed: fixture.seed,
            stream_duration_ms: None,
        },
        metrics,
        invariants,
    }
}

/// All three modes over one fixture, in `PhraseBenchMode::ALL` order.
pub fn run_phrase_benchmark(
    fixture: &PhraseFixture,
    provenance: &PhraseBenchProvenance,
) -> Vec<(PhraseModeSummary, BenchmarkResult)> {
    let segmentation = SegmentationConfig::default();
    PhraseBenchMode::ALL
        .into_iter()
        .map(|mode| {
            let summary = summarize(mode, fixture, &segmentation);
            let result = to_result(&summary, fixture, provenance);
            (summary, result)
        })
        .collect()
}

/// One line per mode for the console and the PR description.
pub fn summary_line(summary: &PhraseModeSummary) -> String {
    format!(
        "{}: responses={} first_audio_ms p50/p90/max={}/{}/{} completion_ms p50/p90/max={}/{}/{} phrases_per_response={} gap_ms mean/max={}/{}",
        summary.mode.as_str(),
        summary.responses,
        summary.first_audio_ms.p50,
        summary.first_audio_ms.p90,
        summary.first_audio_ms.max,
        summary.completion_ms.p50,
        summary.completion_ms.p90,
        summary.completion_ms.max,
        summary.phrases_per_response_mean,
        summary.gap_mean_ms,
        summary.gap_max_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../examples/benchmarks/phrase-comparison.json");
    const SCHEMA: &str = include_str!("../../../schemas/benchmark-result.schema.json");

    fn fixture() -> PhraseFixture {
        PhraseFixture::from_json(FIXTURE.as_bytes()).expect("checked-in fixture is valid")
    }

    fn provenance() -> PhraseBenchProvenance {
        PhraseBenchProvenance {
            git_commit: "test-revision".to_owned(),
            rust_version: "rustc test".to_owned(),
            bun_version: None,
            cargo_profile: "debug".to_owned(),
        }
    }

    fn zero_latency() -> PhraseModelParams {
        PhraseModelParams {
            thinking_first_token_ms: 0,
            thinking_ms_per_char: 0,
            thinking_delta_chars: 3,
            tts_base_ms: 0,
            tts_ms_per_char: 0,
            audio_ms_per_char: 40,
        }
    }

    fn reply(text: &str) -> PhraseBenchReply {
        PhraseBenchReply {
            id: "r".to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn two_runs_are_byte_identical() {
        let render = || {
            run_phrase_benchmark(&fixture(), &provenance())
                .into_iter()
                .map(|(_, result)| serde_json::to_string_pretty(&result).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(render(), render());
    }

    #[test]
    fn modes_never_share_a_compatibility_key() {
        let results = run_phrase_benchmark(&fixture(), &provenance());
        let modes: Vec<&str> = results.iter().map(|(_, r)| r.mode.as_str()).collect();
        assert_eq!(
            modes,
            [
                "phrase_mode_buffered",
                "phrase_mode_phrased",
                "phrase_mode_streamed"
            ]
        );
        for (_, result) in &results {
            assert_eq!(result.benchmark_suite, PHRASE_BENCH_SUITE);
            // Not a ComparisonMode value, so the replay series and the phrase
            // series cannot collide in the history or in the comparator.
            assert!(
                aivtuber_telemetry::ComparisonMode::ALL
                    .iter()
                    .all(|mode| mode.as_str() != result.mode)
            );
        }
    }

    #[test]
    fn emitted_modes_and_shape_match_the_result_schema() {
        let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        let allowed: Vec<&str> = schema["properties"]["mode"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        for (_, result) in run_phrase_benchmark(&fixture(), &provenance()) {
            assert!(allowed.contains(&result.mode.as_str()), "{}", result.mode);
            let document = serde_json::to_value(&result).unwrap();
            let required = schema["required"].as_array().unwrap();
            for key in required {
                assert!(document.get(key.as_str().unwrap()).is_some(), "{key}");
            }
            // The result must round-trip through the comparator's own parser.
            let text = serde_json::to_string(&result).unwrap();
            let parsed = BenchmarkResult::from_json(&text).expect("comparator accepts the result");
            assert_eq!(parsed, result);
        }
    }

    #[test]
    fn buffered_mode_is_one_unit_that_waits_for_the_whole_reply() {
        let params = PhraseModelParams {
            thinking_first_token_ms: 100,
            thinking_ms_per_char: 10,
            thinking_delta_chars: 3,
            tts_base_ms: 50,
            tts_ms_per_char: 5,
            audio_ms_per_char: 40,
        };
        let text = "今日は配信に来てくれてありがとうございます。このあとはゲームを遊びながらお話しします。";
        let chars = text.chars().count();
        let timeline = simulate_reply(
            PhraseBenchMode::Buffered,
            &reply(text),
            &params,
            &SegmentationConfig::default(),
        );
        assert_eq!(timeline.phrases.len(), 1);
        let expected_first = 100 + 10 * chars as u64 + 50 + 5 * chars as u64;
        assert_eq!(timeline.first_audio_ms(), expected_first);
        assert_eq!(timeline.completion_ms(), expected_first + 40 * chars as u64);
    }

    #[test]
    fn phrased_mode_reaches_first_audio_before_buffered_for_a_multi_phrase_reply() {
        let fixture = fixture();
        let segmentation = SegmentationConfig::default();
        let buffered = summarize(PhraseBenchMode::Buffered, &fixture, &segmentation);
        let phrased = summarize(PhraseBenchMode::Phrased, &fixture, &segmentation);
        let streamed = summarize(PhraseBenchMode::Streamed, &fixture, &segmentation);
        assert!(phrased.first_audio_ms.p50 <= buffered.first_audio_ms.p50);
        assert!(phrased.first_audio_ms.max < buffered.first_audio_ms.max);
        assert!(streamed.first_audio_ms.max <= phrased.first_audio_ms.max);
        assert!(phrased.phrases_per_response_mean > 1.0);
        assert_eq!(buffered.phrases_per_response_mean, 1.0);
        assert_eq!(buffered.gap_samples, 0);
    }

    #[test]
    fn streaming_never_changes_the_phrase_partition() {
        let fixture = fixture();
        let streamed = summarize(
            PhraseBenchMode::Streamed,
            &fixture,
            &SegmentationConfig::default(),
        );
        assert_eq!(streamed.partition_mismatch_count, 0);
    }

    #[test]
    fn a_phrase_never_starts_before_the_previous_one_ends() {
        let fixture = fixture();
        for mode in PhraseBenchMode::ALL {
            let summary = summarize(mode, &fixture, &SegmentationConfig::default());
            for timeline in &summary.timelines {
                for pair in timeline.phrases.windows(2) {
                    assert!(pair[1].start_ms >= pair[0].end_ms, "{timeline:?}");
                    assert_eq!(pair[1].gap_before_ms, pair[1].start_ms - pair[0].end_ms);
                }
            }
        }
    }

    #[test]
    fn slow_synthesis_shows_up_as_an_inter_phrase_gap() {
        // Phrase k + 1 is synthesized only after phrase k starts, so a TTS
        // slower than real time leaves silence between phrases.
        let params = PhraseModelParams {
            tts_ms_per_char: 100,
            ..zero_latency()
        };
        let text = "今日は配信に来てくれてありがとうございます。このあとはゲームを遊びながらお話しします。";
        let timeline = simulate_reply(
            PhraseBenchMode::Phrased,
            &reply(text),
            &params,
            &SegmentationConfig::default(),
        );
        assert_eq!(timeline.phrases.len(), 2);
        assert!(timeline.phrases[1].gap_before_ms > 0);
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let sorted = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(nearest_rank(&sorted, 50), 50);
        assert_eq!(nearest_rank(&sorted, 90), 90);
        assert_eq!(nearest_rank(&[], 50), 0);
        assert_eq!(nearest_rank(&[7], 90), 7);
    }

    #[test]
    fn a_malformed_fixture_is_refused() {
        let mut broken = fixture();
        broken.models.thinking_delta_chars = 0;
        assert!(broken.validate().is_err());
        let mut empty = fixture();
        empty.replies.clear();
        assert!(empty.validate().is_err());
        assert!(PhraseFixture::from_json(b"{\"dataset_id\":\"x\"}").is_err());
    }
}
