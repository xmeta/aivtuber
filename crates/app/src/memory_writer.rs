//! Permit-gated production writer for stream-level working memory (#306,
//! ADR-0008 Q1).
//!
//! Before this module no production path wrote durable memory (A-MEM-003), so
//! cross-session continuity had no producer and the ADR-0008 Q4 falsifier had
//! nothing to measure. The writer is the app's post-turn consolidation step:
//! after a reply the output gate admitted has been committed to playback, it
//! extracts a few deterministic, bounded facts *from the performer's own
//! published text* and retains them under a permit the security runtime mints.
//!
//! Boundaries (all tested):
//!
//! * Off by default (`AIVTUBER_MEMORY_WRITER`); without a configured writer the
//!   post-turn step does nothing.
//! * The candidate source is only the committed public output. The viewer's
//!   event text is never read as a candidate, and a sentence that addresses or
//!   names a viewer is dropped whole, so nothing viewer-attributed is retained.
//! * The permit comes from
//!   [`SecurityRuntime::authorize_performer_output_memory_write`], which
//!   re-applies the output gate to the exact claim. Chat cannot reach it and a
//!   model string alone cannot mint it.
//! * Extraction is rule based (no extra model call), so the same committed
//!   text yields the same writes on replay. Time is the runtime's logical
//!   `now_ms`; entry ids come from a writer sequence, not a clock.
//! * Retained writer entries are capped (`max_live_entries`); at the cap a new
//!   candidate is refused (fail closed) instead of evicting other memory.
//! * Everything written is an ordinary durable entry with no actor, so the
//!   #305 operator erase (`erase_memory_operator` by memory id) removes it.

use std::collections::VecDeque;

use aivtuber_adaptation::WorkingMemory;
use aivtuber_runtime::PERFORMER_OUTPUT_SOURCE;

/// Event-id prefix of every writer-minted entry; the retrieval counter keys on
/// it.
pub const MEMORY_WRITER_EVENT_PREFIX: &str = "memory-writer.";

/// Hard ceiling on `max_live_entries`, below the store's own entry bound.
pub const MEMORY_WRITER_MAX_LIVE_ENTRIES_CEILING: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryWriterConfig {
    /// Writer-origin entries that may be live at once (ADR-0008 D-Q5: bounded
    /// quota; the global 2,048 bound is for disk persistence only).
    pub max_live_entries: usize,
    /// Candidates considered from one committed reply or phrase.
    pub max_candidates_per_text: usize,
    /// A longer sentence is refused whole, never truncated.
    pub max_claim_bytes: usize,
}

impl Default for MemoryWriterConfig {
    fn default() -> Self {
        Self {
            max_live_entries: 32,
            max_candidates_per_text: 2,
            max_claim_bytes: 240,
        }
    }
}

impl MemoryWriterConfig {
    pub fn validate(self) -> Result<Self, String> {
        if self.max_live_entries == 0
            || self.max_live_entries > MEMORY_WRITER_MAX_LIVE_ENTRIES_CEILING
        {
            return Err(format!(
                "memory writer max_live_entries must be 1..={MEMORY_WRITER_MAX_LIVE_ENTRIES_CEILING}"
            ));
        }
        if self.max_candidates_per_text == 0 || self.max_candidates_per_text > 8 {
            return Err("memory writer max_candidates_per_text must be 1..=8".to_owned());
        }
        if self.max_claim_bytes == 0 || self.max_claim_bytes > 1_024 {
            return Err("memory writer max_claim_bytes must be 1..=1024".to_owned());
        }
        Ok(self)
    }
}

/// Counters for the ADR-0008 Q4 falsifier: written, retrieved-and-used and
/// expired entries, plus every refusal reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryWriterMetrics {
    pub texts_considered: u64,
    pub written: u64,
    /// Writer entries delivered into a generation request.
    pub retrieved_used: u64,
    /// Writer entries that reached their TTL.
    pub expired: u64,
    /// Writer entries gone before their TTL (erased or capacity-evicted).
    pub removed_early: u64,
    pub rejected_viewer_attributed: u64,
    pub rejected_oversize: u64,
    pub rejected_quota: u64,
    pub rejected_duplicate: u64,
    pub rejected_gate: u64,
    pub rejected_store: u64,
    /// Currently tracked live writer entries.
    pub live: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FactKind {
    Commitment,
    RunningJoke,
    SegmentSummary,
}

impl FactKind {
    pub(crate) fn topic(self) -> &'static str {
        match self {
            Self::Commitment => "commitment",
            Self::RunningJoke => "running-joke",
            Self::SegmentSummary => "segment-summary",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub kind: FactKind,
    pub claim: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Extraction {
    pub candidates: Vec<Candidate>,
    pub rejected_viewer_attributed: u64,
    pub rejected_oversize: u64,
}

const COMMITMENT_MARKERS: &[&str] = &[
    "next stream",
    "next time",
    "i promise",
    "i'll play",
    "i will play",
    "we'll play",
    "we will play",
    "次の配信",
    "次回",
    "約束",
];
const JOKE_MARKERS: &[&str] = &[
    "running joke",
    "running gag",
    "inside joke",
    "いつものネタ",
    "お約束のネタ",
];
const SUMMARY_MARKERS: &[&str] = &[
    "to recap",
    "recap:",
    "so far today",
    "in summary",
    "今日のまとめ",
    "ここまでのまとめ",
    "まとめると",
];
/// Words that address or thank a viewer. A sentence containing one is about a
/// person in the room, not the stream, and is dropped whole.
const ADDRESSING_WORDS: &[&str] = &[
    "you", "your", "yours", "you're", "you'll", "you've", "thanks", "thank",
];
const ADDRESSING_SUBSTRINGS: &[&str] = &["さん", "あなた", "ありがとう", "@"];

fn classify(lower: &str) -> Option<FactKind> {
    let has = |markers: &[&str]| markers.iter().any(|marker| lower.contains(marker));
    if has(COMMITMENT_MARKERS) {
        Some(FactKind::Commitment)
    } else if has(JOKE_MARKERS) {
        Some(FactKind::RunningJoke)
    } else if has(SUMMARY_MARKERS) {
        Some(FactKind::SegmentSummary)
    } else {
        None
    }
}

fn names_a_viewer(lower: &str, exclusions: &[String]) -> bool {
    if ADDRESSING_SUBSTRINGS.iter().any(|s| lower.contains(s)) {
        return true;
    }
    if lower
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .any(|word| ADDRESSING_WORDS.contains(&word))
    {
        return true;
    }
    exclusions.iter().any(|name| lower.contains(name.as_str()))
}

/// Deterministic extraction from the performer's committed text. `viewer_names`
/// are identifiers of the event being answered (actor id, display name); they
/// only ever *remove* sentences.
pub(crate) fn extract_candidates(
    text: &str,
    viewer_names: &[String],
    config: &MemoryWriterConfig,
) -> Extraction {
    let exclusions: Vec<String> = viewer_names
        .iter()
        .map(|name| name.trim().to_lowercase())
        .filter(|name| name.chars().count() >= 2)
        .collect();
    let mut out = Extraction::default();
    for sentence in text.split(['.', '!', '?', '。', '！', '？', '\n']) {
        if out.candidates.len() >= config.max_candidates_per_text {
            break;
        }
        let sentence = sentence.trim();
        if sentence.chars().count() < 8 {
            continue;
        }
        let lower = sentence.to_lowercase();
        let Some(kind) = classify(&lower) else {
            continue;
        };
        if names_a_viewer(&lower, &exclusions) {
            out.rejected_viewer_attributed += 1;
            continue;
        }
        if sentence.len() > config.max_claim_bytes {
            out.rejected_oversize += 1;
            continue;
        }
        out.candidates.push(Candidate {
            kind,
            claim: sentence.to_owned(),
        });
    }
    out
}

pub(crate) fn fold(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// One live writer entry. The store keeps a case- and whitespace-normalized
/// claim, so the writer keeps the sentence as the performer said it (at most
/// `max_claim_bytes`, within `max_live_entries`) for the operator export
/// (#321), which the operator reviews before promoting it by hand.
#[derive(Debug)]
struct LedgerEntry {
    memory_id: u64,
    expires_at_ms: u64,
    topic: &'static str,
    claim: String,
}

#[derive(Debug)]
pub(crate) struct MemoryWriter {
    config: MemoryWriterConfig,
    next_seq: u64,
    /// Live writer entries, bounded by `max_live_entries`; the source of the
    /// expired / removed-early counters and of the export.
    ledger: VecDeque<LedgerEntry>,
    metrics: MemoryWriterMetrics,
}

impl MemoryWriter {
    pub(crate) fn new(config: MemoryWriterConfig) -> Result<Self, String> {
        Ok(Self {
            config: config.validate()?,
            next_seq: 1,
            ledger: VecDeque::new(),
            metrics: MemoryWriterMetrics::default(),
        })
    }

    pub(crate) fn config(&self) -> MemoryWriterConfig {
        self.config
    }

    pub(crate) fn metrics(&self) -> MemoryWriterMetrics {
        MemoryWriterMetrics {
            live: self.ledger.len(),
            ..self.metrics
        }
    }

    pub(crate) fn note_retrieved(&mut self, delivered_event_ids: usize) {
        self.metrics.retrieved_used += delivered_event_ids as u64;
    }

    pub(crate) fn note_texts_considered(&mut self) {
        self.metrics.texts_considered += 1;
    }

    pub(crate) fn note_extraction_rejections(&mut self, extraction: &Extraction) {
        self.metrics.rejected_viewer_attributed += extraction.rejected_viewer_attributed;
        self.metrics.rejected_oversize += extraction.rejected_oversize;
    }

    /// Classify ledger entries that left the store: reached TTL (`expired`) or
    /// gone early (erased or capacity-evicted). Observed lazily, at the next
    /// turn, in ledger order.
    pub(crate) fn sweep(&mut self, memory: &WorkingMemory, now_ms: u64) {
        let metrics = &mut self.metrics;
        self.ledger.retain(|live| {
            if live.expires_at_ms <= now_ms {
                metrics.expired += 1;
                false
            } else if memory
                .entries()
                .iter()
                .all(|entry| entry.memory_id != live.memory_id)
            {
                metrics.removed_early += 1;
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn quota_full(&self) -> bool {
        self.ledger.len() >= self.config.max_live_entries
    }

    pub(crate) fn is_duplicate(&self, memory: &WorkingMemory, claim: &str, now_ms: u64) -> bool {
        let folded = fold(claim);
        memory.entries().iter().any(|entry| {
            entry.source.source == PERFORMER_OUTPUT_SOURCE
                && entry.expires_at_ms > now_ms
                && fold(&entry.normalized_claim) == folded
        })
    }

    pub(crate) fn next_event_id(&mut self) -> String {
        let id = format!("{MEMORY_WRITER_EVENT_PREFIX}{}", self.next_seq);
        self.next_seq += 1;
        id
    }

    pub(crate) fn record_written(
        &mut self,
        memory_id: u64,
        expires_at_ms: u64,
        kind: FactKind,
        claim: &str,
    ) {
        self.ledger.push_back(LedgerEntry {
            memory_id,
            expires_at_ms,
            topic: kind.topic(),
            claim: claim.to_owned(),
        });
        self.metrics.written += 1;
    }

    /// Live writer entries in write order as `(topic, claim as said)`. Call
    /// [`Self::sweep`] first so erased and expired entries are gone.
    pub(crate) fn live_claims(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.ledger
            .iter()
            .map(|live| (live.topic, live.claim.as_str()))
    }

    pub(crate) fn reject_quota(&mut self) {
        self.metrics.rejected_quota += 1;
    }
    pub(crate) fn reject_duplicate(&mut self) {
        self.metrics.rejected_duplicate += 1;
    }
    pub(crate) fn reject_gate(&mut self) {
        self.metrics.rejected_gate += 1;
    }
    pub(crate) fn reject_store(&mut self) {
        self.metrics.rejected_store += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MemoryWriterConfig {
        MemoryWriterConfig::default()
    }

    #[test]
    fn extracts_stream_level_facts_by_kind() {
        let out = extract_candidates(
            "Welcome back! I'll play Hollow Knight next stream. The cat bit is our running joke. Nothing else here.",
            &[],
            &cfg(),
        );
        let kinds: Vec<_> = out.candidates.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, [FactKind::Commitment, FactKind::RunningJoke]);
    }

    #[test]
    fn viewer_addressing_and_names_are_dropped_whole() {
        let names = vec!["Alice99".to_owned()];
        let out = extract_candidates(
            "Thanks for asking, I promise to play it next stream. I promise alice99 a game next time. I promise a game next time @bob. Next time I will play chess ok",
            &names,
            &cfg(),
        );
        assert_eq!(out.candidates.len(), 1);
        assert_eq!(out.rejected_viewer_attributed, 3);
        assert!(out.candidates[0].claim.contains("chess"));
    }

    #[test]
    fn oversize_is_refused_not_truncated() {
        let long = format!("I promise {} next stream", "x".repeat(300));
        let out = extract_candidates(&long, &[], &cfg());
        assert!(out.candidates.is_empty());
        assert_eq!(out.rejected_oversize, 1);
    }

    #[test]
    fn extraction_is_deterministic_and_bounded() {
        let text =
            "I promise a game next time. I promise two next time. I promise three next time.";
        let a = extract_candidates(text, &[], &cfg());
        assert_eq!(a, extract_candidates(text, &[], &cfg()));
        assert_eq!(a.candidates.len(), cfg().max_candidates_per_text);
    }

    #[test]
    fn config_bounds_are_validated() {
        assert!(
            MemoryWriterConfig {
                max_live_entries: 0,
                ..cfg()
            }
            .validate()
            .is_err()
        );
        assert!(
            MemoryWriterConfig {
                max_live_entries: 129,
                ..cfg()
            }
            .validate()
            .is_err()
        );
        assert!(cfg().validate().is_ok());
    }
}
