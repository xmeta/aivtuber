//! Atomic phrase units for phrased generated replies (issue #83 S1).
//!
//! A buffered Thinking reply is split into phrases by a deterministic
//! punctuation/clause segmenter. Each phrase then travels the same fixed
//! sequence as a whole reply used to:
//!
//! ```text
//! Planned -> Gated -> Synthesized -> Compiled -> Committed
//!               \________________________/            (terminal, irreversible)
//!                      Failed | Cancelled
//! ```
//!
//! Only a phrase whose text passed the output gate, whose audio is complete
//! and whose asset validated is ever handed to the commit boundary, so
//! partial text or audio cannot reach the scheduler by construction: a
//! [`PhraseUnit`] can only be built by the pipeline after `Compiled`.
//! `Committed` is terminal; once a phrase is committed it is history and no
//! later failure or cancellation can unspeak it (see the commit horizon in the
//! issue). Failures and cancellation apply only to phrases not yet committed.
//!
//! Segmentation reuse decision (docs/reuse-first.adoc, row "Phrase/sentence
//! boundary primitives"): `custom`. `unicode-segmentation` (UAX #29) was the
//! issue's first candidate. It supplies strong sentence breaks only; this
//! module still needs the project rules around them (clause breaks at 、 and
//! `,`, a minimum and maximum phrase weight, trailing emoji/closer absorption,
//! URL and decimal protection, a short abbreviation list) and UAX #29 breaks
//! before a trailing emoji or closing mark and after `Mr.`, which those rules
//! have to undo. A new dependency would not remove that code, so the rules are
//! kept here, small and fixture-tested
//! (`examples/phrase-segmentation/cases.json`). Revisit if a third language or
//! full Unicode script coverage is needed.

use crate::CancellationStage;
use crate::GenerationCancellationToken;
use crate::GenerationTrace;
use crate::speech_text::is_emoji_part;
use aivtuber_asset_store::PerformanceAsset;
use aivtuber_domain::FallbackReason;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// Identity of the segmentation rules. Recorded with every phrased reply so a
/// replayed reply names the rules that cut it; bump on any behavioural change.
pub const PHRASE_SEGMENTATION_VERSION: &str = "phrase-seg-v1";

/// Hard ceiling on phrases per reply, whatever the configuration says.
pub const MAX_PHRASES_CEILING: usize = 32;

// ---------------------------------------------------------------------------
// Segmentation
// ---------------------------------------------------------------------------

/// Bounds of the segmenter. Lengths are in *weight* units (an ASCII character
/// weighs 1, every other character 2), which keeps Japanese and English
/// phrases at a comparable spoken length without a language detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentationConfig {
    /// No phrase is cut shorter than this (a shorter tail merges backwards).
    pub min_weight: usize,
    /// A clause break (、 or `,`) cuts a phrase only at or above this weight.
    pub clause_weight: usize,
    /// Preferred ceiling: past it the segmenter cuts at the best earlier
    /// boundary.
    pub max_weight: usize,
    /// Absolute ceiling for a phrase that has no boundary below `max_weight`.
    pub hard_max_weight: usize,
    /// At most this many phrases; the tail of a longer reply is merged into
    /// the last phrase rather than dropped.
    pub max_phrases: usize,
}

impl Default for SegmentationConfig {
    fn default() -> Self {
        Self {
            min_weight: 16,
            clause_weight: 32,
            max_weight: 96,
            hard_max_weight: 192,
            max_phrases: 32,
        }
    }
}

impl SegmentationConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.min_weight == 0 {
            return Err("min_weight must be greater than zero".to_owned());
        }
        if !(self.min_weight <= self.clause_weight
            && self.clause_weight <= self.max_weight
            && self.max_weight <= self.hard_max_weight)
        {
            return Err(
                "segmentation weights must satisfy min <= clause <= max <= hard_max".to_owned(),
            );
        }
        if self.max_phrases == 0 || self.max_phrases > MAX_PHRASES_CEILING {
            return Err(format!(
                "max_phrases must be between 1 and {MAX_PHRASES_CEILING}"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strength {
    /// End of a sentence (。！？ or a terminating `.!?`, or a line break).
    Strong,
    /// A clause break or a pause mark (、 `,` ； `:` …).
    Weak,
    /// Whitespace between words: only a fallback for an overlong phrase.
    Space,
}

fn weight(c: char) -> usize {
    if c.is_ascii() { 1 } else { 2 }
}

fn is_closer(c: char) -> bool {
    matches!(
        c,
        ')' | '）'
            | ']'
            | '］'
            | '}'
            | '｝'
            | '」'
            | '』'
            | '】'
            | '〉'
            | '》'
            | '”'
            | '’'
            | '"'
            | '\''
    )
}

fn is_sentence_decoration(c: char) -> bool {
    is_emoji_part(c) || matches!(c, '♪' | '♫' | '♡' | '♥' | '★' | '☆' | '~' | '〜' | '～')
}

fn is_ascii_boundary_neighbour(next: Option<char>) -> bool {
    // An ASCII `.`, `!` or `?` ends a sentence only when nothing word-like or
    // URL-like follows it: `3.14`, `example.com` and `?x=1` are not boundaries.
    match next {
        None => true,
        Some(c) => {
            c.is_whitespace() || !c.is_ascii() || is_closer(c) || matches!(c, '.' | '!' | '?')
        }
    }
}

const ABBREVIATIONS: [&str; 13] = [
    "mr", "mrs", "ms", "dr", "prof", "st", "vs", "no", "e.g", "i.e", "inc", "jr", "sr",
];

fn ends_with_abbreviation(chars: &[char], dot: usize) -> bool {
    let mut start = dot;
    while start > 0 && (chars[start - 1].is_ascii_alphabetic() || chars[start - 1] == '.') {
        start -= 1;
    }
    let word: String = chars[start..dot]
        .iter()
        .map(char::to_ascii_lowercase)
        .collect();
    ABBREVIATIONS.contains(&word.as_str())
}

fn is_strong_at(chars: &[char], index: usize) -> bool {
    let c = chars[index];
    let next = chars.get(index + 1).copied();
    match c {
        '。' | '．' | '！' | '？' | '‼' | '⁇' | '⁈' | '⁉' | '\n' => true,
        '!' | '?' => is_ascii_boundary_neighbour(next),
        '.' => {
            // `...` is a pause (weak), a lone `.` can end a sentence.
            is_ascii_boundary_neighbour(next)
                && !matches!(next, Some('.'))
                && !(index > 0 && chars[index - 1] == '.')
                && !ends_with_abbreviation(chars, index)
        }
        _ => false,
    }
}

fn is_weak_at(chars: &[char], index: usize) -> bool {
    let c = chars[index];
    let next = chars.get(index + 1).copied();
    match c {
        '、' | '，' | '；' | '：' | '…' | '‥' | '―' => true,
        ',' | ';' | ':' => next.is_none_or(char::is_whitespace),
        '.' => matches!(next, Some('.')) || (index > 0 && chars[index - 1] == '.'),
        _ => false,
    }
}

fn is_space_at(chars: &[char], index: usize) -> bool {
    matches!(chars[index], ' ' | '\u{3000}')
}

/// End (exclusive) of the terminator run that starts at `index`: further
/// terminators, closing marks and trailing emoji/decoration belong to the
/// sentence they close.
fn absorb_terminator_run(chars: &[char], index: usize) -> usize {
    let mut end = index + 1;
    while let Some(&c) = chars.get(end) {
        let terminator = matches!(
            c,
            '。' | '．' | '！' | '？' | '‼' | '⁇' | '⁈' | '⁉' | '…' | '‥' | '!' | '?' | '.'
        );
        if terminator || is_closer(c) || is_sentence_decoration(c) {
            end += 1;
        } else {
            break;
        }
    }
    end
}

/// Candidate cut positions (character index at which the next phrase would
/// start) with their strength, in increasing order.
fn boundaries(chars: &[char]) -> Vec<(usize, Strength)> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if is_strong_at(chars, index) {
            let end = absorb_terminator_run(chars, index);
            out.push((end, Strength::Strong));
            index = end;
        } else if is_weak_at(chars, index) {
            out.push((index + 1, Strength::Weak));
            index += 1;
        } else if is_space_at(chars, index) {
            out.push((index + 1, Strength::Space));
            index += 1;
        } else {
            index += 1;
        }
    }
    out
}

pub(crate) fn has_content(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}

/// Split a complete reply into phrases. Deterministic and pure; concatenating
/// the phrases (ignoring whitespace at the cuts) gives back the input, and
/// nothing is ever added, so the phrases are exactly the text the output gate
/// already saw.
pub fn segment_reply(text: &str, config: &SegmentationConfig) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    // prefix[i] = weight of chars[..i]
    let mut prefix = Vec::with_capacity(total + 1);
    prefix.push(0_usize);
    for &c in &chars {
        prefix.push(prefix[prefix.len() - 1] + weight(c));
    }
    let span = |from: usize, to: usize| prefix[to] - prefix[from];
    let cuts = boundaries(&chars);

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    while start < total {
        // Whitespace before a phrase carries no weight of its own.
        while start < total && chars[start].is_whitespace() {
            start += 1;
        }
        if start >= total {
            break;
        }
        let mut best_fallback: Option<usize> = None;
        let mut cut: Option<usize> = None;
        for &(end, strength) in cuts.iter().filter(|(end, _)| *end > start) {
            let w = span(start, end);
            if w < config.min_weight {
                continue;
            }
            if w <= config.max_weight {
                if strength == Strength::Strong
                    || (strength == Strength::Weak && w >= config.clause_weight)
                {
                    cut = Some(end);
                    break;
                }
                best_fallback = Some(end);
                continue;
            }
            // Past the preferred ceiling: the best earlier boundary wins, else
            // the first boundary within the hard ceiling.
            cut = best_fallback.or((w <= config.hard_max_weight).then_some(end));
            break;
        }
        let end = match cut {
            Some(end) => end,
            None if span(start, total) <= config.max_weight => total,
            None => best_fallback.unwrap_or_else(|| {
                // No boundary at all: cut where the preferred ceiling is reached.
                let mut end = start;
                while end < total && span(start, end + 1) <= config.max_weight {
                    end += 1;
                }
                end.max(start + 1)
            }),
        };
        ranges.push((start, end));
        start = end;
    }

    merge_small_ranges(&chars, &prefix, &mut ranges, config);
    cap_phrase_count(&mut ranges, config.max_phrases);

    ranges
        .into_iter()
        .map(|(from, to)| chars[from..to].iter().collect::<String>())
        .map(|phrase| phrase.trim().to_owned())
        .filter(|phrase| !phrase.is_empty())
        .collect()
}

/// A tail shorter than the minimum, and any phrase with nothing speakable in
/// it (only emoji or marks), joins its neighbour instead of standing alone.
fn merge_small_ranges(
    chars: &[char],
    prefix: &[usize],
    ranges: &mut Vec<(usize, usize)>,
    config: &SegmentationConfig,
) {
    let text_of = |range: (usize, usize)| chars[range.0..range.1].iter().collect::<String>();
    let mut index = 0;
    while index < ranges.len() {
        let range = ranges[index];
        let small_tail = index + 1 == ranges.len()
            && index > 0
            && prefix[range.1] - prefix[range.0] < config.min_weight
            && {
                let previous = ranges[index - 1];
                prefix[range.1] - prefix[previous.0] <= config.hard_max_weight
            };
        let hollow = !has_content(&text_of(range));
        if (small_tail || hollow) && ranges.len() > 1 {
            if index > 0 {
                ranges[index - 1].1 = range.1;
                ranges.remove(index);
            } else {
                ranges[1].0 = range.0;
                ranges.remove(0);
            }
            continue;
        }
        index += 1;
    }
}

fn cap_phrase_count(ranges: &mut Vec<(usize, usize)>, max_phrases: usize) {
    if ranges.len() > max_phrases {
        let end = ranges[ranges.len() - 1].1;
        ranges.truncate(max_phrases);
        ranges[max_phrases - 1].1 = end;
    }
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// Why a phrase did not reach the commit boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhraseFailure {
    /// The output gate refused the phrase (or its speech projection).
    GateRejected,
    /// Nothing speakable was left after the gate and the speech projection.
    NothingSpeakable,
    /// TTS failed or timed out for this phrase.
    Synthesis(FallbackReason),
    /// The synthesized phrase could not be compiled into a valid asset.
    Compile,
    /// The scheduler refused the commit (the reply could not continue).
    SchedulerRejected,
    /// The previous phrase never started playing within the wait bound.
    LookaheadStalled,
}

impl PhraseFailure {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GateRejected => "gate_rejected",
            Self::NothingSpeakable => "nothing_speakable",
            Self::Synthesis(_) => "synthesis_failed",
            Self::Compile => "compile_failed",
            Self::SchedulerRejected => "scheduler_rejected",
            Self::LookaheadStalled => "lookahead_stalled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum PhraseState {
    /// Segmented; nothing has been done with it.
    Planned,
    /// The display text was approved by the output gate.
    Gated,
    /// Complete audio for the approved speech text exists.
    Synthesized,
    /// A valid asset exists: eligible for commit, still replaceable.
    Compiled,
    /// Accepted by the shared scheduler. Terminal and irreversible.
    Committed,
    Failed {
        failure: PhraseFailure,
    },
    Cancelled {
        at: CancellationStage,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhraseStep {
    Gate,
    Synthesize,
    Compile,
    Commit,
    Fail(PhraseFailure),
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhraseTransitionError {
    pub from: PhraseState,
    pub step: PhraseStep,
}

impl fmt::Display for PhraseTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal phrase transition {:?} from {:?}",
            self.step, self.from
        )
    }
}

impl std::error::Error for PhraseTransitionError {}

impl PhraseState {
    /// Terminal states accept no further step.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Committed | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }

    /// Past the irreversible horizon: the phrase is history.
    pub fn is_committed(&self) -> bool {
        matches!(self, Self::Committed)
    }

    fn cancellation_stage(&self) -> Option<CancellationStage> {
        match self {
            Self::Planned | Self::Gated => Some(CancellationStage::AfterThinking),
            Self::Synthesized => Some(CancellationStage::AfterTts),
            Self::Compiled => Some(CancellationStage::BeforePublish),
            _ => None,
        }
    }

    /// Apply one step. Only the forward chain, a failure or a cancellation
    /// before commit is legal; nothing leaves a terminal state, and a
    /// committed phrase can neither fail nor be cancelled.
    pub fn apply(self, step: PhraseStep) -> Result<Self, PhraseTransitionError> {
        let illegal = PhraseTransitionError { from: self, step };
        match (self, step) {
            (Self::Planned, PhraseStep::Gate) => Ok(Self::Gated),
            (Self::Gated, PhraseStep::Synthesize) => Ok(Self::Synthesized),
            (Self::Synthesized, PhraseStep::Compile) => Ok(Self::Compiled),
            (Self::Compiled, PhraseStep::Commit) => Ok(Self::Committed),
            (state, PhraseStep::Fail(failure)) if !state.is_terminal() => {
                Ok(Self::Failed { failure })
            }
            (state, PhraseStep::Cancel) => state
                .cancellation_stage()
                .map(|at| Self::Cancelled { at })
                .ok_or(illegal),
            _ => Err(illegal),
        }
    }
}

// ---------------------------------------------------------------------------
// Unit and delivery
// ---------------------------------------------------------------------------

/// One complete, approved, synthesized phrase, ready for an atomic commit.
///
/// Built only by the pipeline once the phrase reached `Compiled`; the fields
/// are read through accessors so a consumer cannot assemble a partial one.
#[derive(Debug, Clone, PartialEq)]
pub struct PhraseUnit {
    response_id: u64,
    seq: u32,
    final_phrase: bool,
    display_text: String,
    asset: PerformanceAsset,
    interrupt_boundary_ms: u64,
    synthesis_ms: u64,
    state: PhraseState,
}

impl PhraseUnit {
    pub(crate) fn new(
        response_id: u64,
        seq: u32,
        final_phrase: bool,
        display_text: String,
        asset: PerformanceAsset,
        synthesis_ms: u64,
    ) -> Self {
        let interrupt_boundary_ms = asset
            .speech
            .as_ref()
            .and_then(|speech| speech.duration_ms)
            .unwrap_or_default();
        Self {
            response_id,
            seq,
            final_phrase,
            display_text,
            asset,
            interrupt_boundary_ms,
            synthesis_ms,
            state: PhraseState::Compiled,
        }
    }

    /// Generation/cancel identity shared by every phrase of the reply.
    pub fn response_id(&self) -> u64 {
        self.response_id
    }

    pub fn seq(&self) -> u32 {
        self.seq
    }

    /// Whether this is the last phrase of the planned reply.
    pub fn is_final(&self) -> bool {
        self.final_phrase
    }

    /// The complete approved text of this phrase (the text the gate published).
    pub fn display_text(&self) -> &str {
        &self.display_text
    }

    /// The compiled asset: complete audio reference, optional viseme track,
    /// provenance (Thinking and TTS identity) and the interrupt point.
    pub fn asset(&self) -> &PerformanceAsset {
        &self.asset
    }

    pub fn into_asset(self) -> PerformanceAsset {
        self.asset
    }

    /// Offset, from the phrase start, of the safe interrupt point declared at
    /// the end of the phrase.
    pub fn interrupt_boundary_ms(&self) -> u64 {
        self.interrupt_boundary_ms
    }

    /// Wall time TTS took for this phrase (a measurement, never an input).
    pub fn synthesis_ms(&self) -> u64 {
        self.synthesis_ms
    }

    pub fn state(&self) -> PhraseState {
        self.state
    }
}

/// Answer to "may phrase `seq` start synthesizing?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotWait {
    Ready,
    Cancelled,
    Stalled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Accepted,
    /// The receiving side is gone; the reply must stop.
    Closed,
}

/// The pipeline's boundary to whatever commits phrases. Implemented by the
/// composition root; the generative crate never touches the scheduler.
pub trait PhraseDelivery {
    /// Block (bounded) until phrase `seq` may start synthesizing. This is the
    /// lookahead bound: synthesis of a later phrase overlaps the playback of
    /// the one before it, never of the ones before that.
    fn wait_for_slot(&mut self, seq: u32, cancellation: &GenerationCancellationToken) -> SlotWait;

    /// Hand over one complete phrase with the trace so far (the LLM call it
    /// belongs to is audited from it). May block when the commit side is
    /// behind (bounded hand-off).
    fn deliver(&mut self, unit: PhraseUnit, trace: &GenerationTrace) -> Delivery;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhraseConfig {
    pub segmentation: SegmentationConfig,
    /// Synthesis deadline of one phrase. Derived per phrase, not per reply, so
    /// a long reply no longer times out as a whole; the first phrase is also
    /// capped by the interaction deadline.
    pub tts_timeout: Duration,
}

impl Default for PhraseConfig {
    fn default() -> Self {
        Self {
            segmentation: SegmentationConfig::default(),
            tts_timeout: Duration::from_secs(12),
        }
    }
}

impl PhraseConfig {
    pub fn validate(&self) -> Result<(), String> {
        self.segmentation.validate()?;
        if self.tts_timeout.is_zero() {
            return Err("phrase tts_timeout must be greater than zero".to_owned());
        }
        Ok(())
    }
}

/// Per-reply inputs of the phrased pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhraseOptions {
    /// Generation/cancel identity every phrase of the reply carries.
    pub response_id: u64,
    pub config: PhraseConfig,
}

/// How a phrased reply ended, from the generative worker's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "end")]
pub enum PhraseEnd {
    /// Every planned phrase was handed to the commit boundary.
    Completed,
    /// Cancelled before phrase `seq` could be delivered.
    Cancelled { seq: u32, at: CancellationStage },
    /// Phrase `seq` failed; the reply ended cleanly after the previous phrase.
    Failed { seq: u32, failure: PhraseFailure },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhraseSummary {
    pub response_id: u64,
    pub segmentation_version: String,
    pub planned_phrases: u32,
    /// Phrases handed to the commit boundary (the app may still refuse one).
    pub delivered_phrases: u32,
    pub end: PhraseEnd,
    /// Characters sent to TTS across all phrases (for budget settlement).
    pub tts_chars: u64,
}

impl PhraseSummary {
    /// True only when every planned phrase was delivered. A reply that stopped
    /// early is partial and must never be reported as complete.
    pub fn completed(&self) -> bool {
        self.end == PhraseEnd::Completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SegmentationConfig {
        SegmentationConfig::default()
    }

    #[derive(Deserialize)]
    struct Case {
        name: String,
        text: String,
        expected: Vec<String>,
        #[serde(default)]
        config: Option<CaseConfig>,
    }

    #[derive(Deserialize)]
    struct CaseConfig {
        min_weight: usize,
        clause_weight: usize,
        max_weight: usize,
        hard_max_weight: usize,
        max_phrases: usize,
    }

    #[derive(Deserialize)]
    struct Fixture {
        version: String,
        cases: Vec<Case>,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!(
            "../../../examples/phrase-segmentation/cases.json"
        ))
        .expect("segmentation fixture parses")
    }

    #[test]
    fn the_segmentation_fixture_matches_the_pinned_rules() {
        let fixture = fixture();
        assert_eq!(
            fixture.version, PHRASE_SEGMENTATION_VERSION,
            "the fixture names the rules it pins"
        );
        assert!(fixture.cases.len() >= 20, "Japanese, English, mixed, edge");
        for case in &fixture.cases {
            let cfg = case
                .config
                .as_ref()
                .map_or(config(), |c| SegmentationConfig {
                    min_weight: c.min_weight,
                    clause_weight: c.clause_weight,
                    max_weight: c.max_weight,
                    hard_max_weight: c.hard_max_weight,
                    max_phrases: c.max_phrases,
                });
            cfg.validate().expect("fixture config is valid");
            assert_eq!(
                segment_reply(&case.text, &cfg),
                case.expected,
                "case {:?}",
                case.name
            );
        }
    }

    #[test]
    fn segmentation_is_pure_and_loses_nothing() {
        let fixture = fixture();
        for case in &fixture.cases {
            let cfg = SegmentationConfig::default();
            let first = segment_reply(&case.text, &cfg);
            assert_eq!(first, segment_reply(&case.text, &cfg), "deterministic");
            let joined: String = first.concat();
            let squeezed = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            if case.config.is_none() {
                assert_eq!(
                    squeezed(&joined),
                    squeezed(&case.text),
                    "case {:?} dropped or invented text",
                    case.name
                );
            }
        }
    }

    #[test]
    fn every_phrase_respects_the_bounds_without_a_hard_cut_marker() {
        let cfg = config();
        let long_no_punctuation = "あ".repeat(400);
        let phrases = segment_reply(&long_no_punctuation, &cfg);
        assert!(phrases.len() <= cfg.max_phrases);
        for phrase in &phrases[..phrases.len() - 1] {
            let w: usize = phrase.chars().map(weight).sum();
            assert!(w <= cfg.max_weight, "weight {w}");
            assert!(w >= cfg.min_weight);
        }
    }

    #[test]
    fn the_phrase_count_is_bounded_and_the_tail_is_merged_not_dropped() {
        let cfg = SegmentationConfig {
            max_phrases: 3,
            ..config()
        };
        let text = "これは長めの一つ目の文です。これは長めの二つ目の文です。これは長めの三つ目の文です。これは長めの四つ目の文です。これは長めの五つ目の文です。";
        let phrases = segment_reply(text, &cfg);
        assert_eq!(phrases.len(), 3);
        assert!(phrases[2].contains("四つ目") && phrases[2].contains("五つ目"));
    }

    #[test]
    fn empty_and_blank_replies_have_no_phrases() {
        assert!(segment_reply("", &config()).is_empty());
        assert!(segment_reply("  \n\t ", &config()).is_empty());
    }

    #[test]
    fn config_validation_rejects_inconsistent_bounds() {
        assert!(config().validate().is_ok());
        let mut bad = config();
        bad.min_weight = 0;
        assert!(bad.validate().is_err());
        bad = config();
        bad.clause_weight = bad.max_weight + 1;
        assert!(bad.validate().is_err());
        bad = config();
        bad.max_phrases = 0;
        assert!(bad.validate().is_err());
        bad.max_phrases = MAX_PHRASES_CEILING + 1;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn the_forward_chain_is_the_only_way_to_commit() {
        let mut state = PhraseState::Planned;
        for step in [
            PhraseStep::Gate,
            PhraseStep::Synthesize,
            PhraseStep::Compile,
            PhraseStep::Commit,
        ] {
            state = state.apply(step).expect("legal step");
        }
        assert!(state.is_committed() && state.is_terminal());
        // Skipping a stage is illegal.
        assert!(PhraseState::Planned.apply(PhraseStep::Commit).is_err());
        assert!(PhraseState::Gated.apply(PhraseStep::Compile).is_err());
        assert!(PhraseState::Synthesized.apply(PhraseStep::Commit).is_err());
    }

    #[test]
    fn a_committed_phrase_can_neither_fail_nor_be_cancelled() {
        let committed = PhraseState::Committed;
        assert!(
            committed
                .apply(PhraseStep::Fail(PhraseFailure::GateRejected))
                .is_err()
        );
        assert!(committed.apply(PhraseStep::Cancel).is_err());
        assert!(committed.apply(PhraseStep::Gate).is_err());
    }

    #[test]
    fn failure_and_cancellation_are_legal_only_before_commit() {
        for state in [
            PhraseState::Planned,
            PhraseState::Gated,
            PhraseState::Synthesized,
            PhraseState::Compiled,
        ] {
            let failed = state
                .apply(PhraseStep::Fail(PhraseFailure::Compile))
                .expect("failure before commit");
            assert!(failed.is_terminal() && !failed.is_committed());
            let cancelled = state
                .apply(PhraseStep::Cancel)
                .expect("cancel before commit");
            assert!(matches!(cancelled, PhraseState::Cancelled { .. }));
            // Terminal states accept nothing more.
            assert!(failed.apply(PhraseStep::Cancel).is_err());
            assert!(cancelled.apply(PhraseStep::Gate).is_err());
        }
    }

    #[test]
    fn cancellation_records_the_stage_it_interrupted() {
        assert_eq!(
            PhraseState::Synthesized.apply(PhraseStep::Cancel),
            Ok(PhraseState::Cancelled {
                at: CancellationStage::AfterTts
            })
        );
        assert_eq!(
            PhraseState::Compiled.apply(PhraseStep::Cancel),
            Ok(PhraseState::Cancelled {
                at: CancellationStage::BeforePublish
            })
        );
    }
}
