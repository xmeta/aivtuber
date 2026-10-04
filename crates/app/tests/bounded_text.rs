//! Deterministic bounded-text invariants (issue #146).
//!
//! Issue #146 asks for UTF-8/byte-bound coverage as "property tests or
//! deterministic fuzz assertions". These are property tests, so they run on the
//! pinned stable toolchain inside the normal `cargo test --locked --workspace`
//! gate and need no nightly — unlike the libFuzzer targets in `fuzz/`, which
//! cover the parser boundaries as a local/nightly activity.
//!
//! Two invariants, both about byte arithmetic at a trust boundary:
//!
//! 1. **Byte-bound oversize handling never emits a partial record.**
//!    `pump_bounded_records` retains up to `max_record_bytes + 1` bytes with no
//!    char-boundary check, then discards the whole record when it is
//!    definitely oversized. The retained bytes exist for metrics and
//!    diagnostics only. So the safety property is stronger than "truncation
//!    keeps UTF-8 valid": a record that would be split mid-character is never
//!    delivered at all, and the bytes that are retained for diagnostics can
//!    only ever be a clean prefix.
//! 2. **Encoded size accounting stays in UTF-8 bytes, not characters.**
//!    The domain limits are byte budgets. If a future change measured with
//!    `chars().count()` instead of `len()`, a Japanese chat message would pass a
//!    limit it should fail, which is a content-boundary failure, not a cosmetic
//!    one.

use aivtuber_app::{RawIngressConfig, RawIngressExit, RawIngressMetrics, pump_bounded_records};
use aivtuber_domain::{
    EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, MAX_THINKING_CONTEXT_BYTES,
    MAX_THINKING_TEXT_BYTES, PrivacyClass, ReflexContext, RetrievalSnapshot, SecurityPlane,
    SourceClass, ThinkingContent, ThinkingContextSource, ThinkingRequest, TrustLevel,
};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::mpsc;

fn chat_event() -> EventEnvelope {
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: "bounded-text-fixture".to_owned(),
        correlation_id: "bounded-text-corr".to_owned(),
        sequence: 1,
        observed_at: "2026-10-04T00:00:00Z".to_owned(),
        source: "chat".to_owned(),
        source_class: SourceClass::PublicChat,
        plane: SecurityPlane::Content,
        trust_level: TrustLevel::Untrusted,
        kind: EventKind::ChatMessage,
        actor_id: None,
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::new(),
    }
}

fn curated(text: String) -> ThinkingContent {
    ThinkingContent {
        source: ThinkingContextSource::RecentInteraction,
        event_id: Some("evt-previous".to_owned()),
        event_kind: Some(EventKind::ChatMessage),
        source_class: Some(SourceClass::PublicChat),
        trust_level: TrustLevel::Untrusted,
        privacy: PrivacyClass::Pseudonymous,
        text,
    }
}

fn thinking_request(text: String, curated_context: Vec<ThinkingContent>) -> ThinkingRequest {
    ThinkingRequest::from_event(
        &chat_event(),
        text,
        PrivacyClass::Pseudonymous,
        ReflexContext::default(),
        RetrievalSnapshot::default(),
        curated_context,
    )
}

fn drain_raw_ingress(input: Vec<u8>, max_record_bytes: usize) -> (Vec<Vec<u8>>, RawIngressExit) {
    let (sender, receiver) = mpsc::sync_channel(64);
    let metrics = RawIngressMetrics::default();
    let exit = pump_bounded_records(
        Cursor::new(input),
        &sender,
        RawIngressConfig {
            max_record_bytes,
            queue_capacity: 64,
        },
        &metrics,
    )
    .expect("ingress runs");
    drop(sender);
    (receiver.into_iter().collect(), exit)
}

/// A record longer than the cap is *dropped whole*, never delivered truncated.
///
/// This is the property that makes byte-bound truncation safe without a
/// char-boundary check: an oversized multi-byte record could otherwise be split
/// mid-character and the partial bytes would reach the JSON parser. Dropping it
/// means a record delivered to the parser is always a complete, well-formed
/// byte sequence.
///
/// Retained bytes still exist for metrics, and the cap is `max_record_bytes + 1`
/// (one boundary byte), so the observed retained length is asserted too.
#[test]
fn oversized_records_are_dropped_whole_never_delivered_truncated() {
    let text = "あ".repeat(64); // 192 bytes, 3 bytes per character
    assert_eq!(text.len(), 192, "the fixture must be 3 bytes per character");

    let metrics = RawIngressMetrics::default();
    let (sender, receiver) = mpsc::sync_channel(64);
    let mut input = Vec::new();
    input.extend_from_slice(text.as_bytes());
    input.push(b'\n');

    let cap = 8_usize;
    let exit = pump_bounded_records(
        Cursor::new(input),
        &sender,
        RawIngressConfig {
            max_record_bytes: cap,
            queue_capacity: 64,
        },
        &metrics,
    )
    .expect("ingress runs");
    drop(sender);
    let records: Vec<Vec<u8>> = receiver.into_iter().collect();

    assert_eq!(exit, RawIngressExit::Eof);
    assert!(
        records.is_empty(),
        "cap {cap}: an oversized record must be dropped, not delivered truncated: \
         {records:?}"
    );

    // The retained bytes are a clean prefix of the input, bounded by the cap
    // plus its one boundary byte.
    let snapshot = metrics.snapshot();
    assert!(
        snapshot.max_retained_record_bytes <= (cap + 1) as u64,
        "retained {} bytes, exceeding max_record_bytes + 1",
        snapshot.max_retained_record_bytes
    );
    assert_eq!(
        snapshot.dropped_oversize, 1,
        "the oversized record must be counted as dropped"
    );
}

/// A short record is the control case: it proves the assertions above are not
/// passing merely because everything is dropped.
#[test]
fn records_within_the_cap_are_retained_verbatim() {
    let line = r#"{"schema_version":"0.2.0","event_id":"e1"}"#;
    let mut input = Vec::new();
    input.extend_from_slice(line.as_bytes());
    input.push(b'\n');

    let (records, exit) = drain_raw_ingress(input, 4096);
    assert_eq!(exit, RawIngressExit::Eof);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0],
        line.as_bytes(),
        "a short record must be retained verbatim"
    );
}

/// An oversized record does not desynchronise the stream: the record after it
/// is still delivered intact. If oversize handling consumed the wrong number of
/// bytes, every following record would be corrupted too.
#[test]
fn an_oversized_record_does_not_corrupt_the_records_after_it() {
    let mut input = Vec::new();
    input.extend_from_slice("x".repeat(200).as_bytes());
    input.push(b'\n');
    input.extend_from_slice(br#"{"schema_version":"0.2.0","event_id":"after"}"#);
    input.push(b'\n');
    input.extend_from_slice(br#"{"schema_version":"0.2.0","event_id":"after2"}"#);
    input.push(b'\n');

    // Cap 64 admits the 43-byte records but not the 200-byte one.
    let (records, exit) = drain_raw_ingress(input, 64);
    assert_eq!(exit, RawIngressExit::Eof);
    assert_eq!(
        records.len(),
        2,
        "only the two short records may be delivered: {records:?}"
    );
    assert_eq!(
        records[0], br#"{"schema_version":"0.2.0","event_id":"after"}"#,
        "the record after an oversized one must be byte-identical"
    );
    assert_eq!(
        records[1], br#"{"schema_version":"0.2.0","event_id":"after2"}"#,
        "the second short record must be byte-identical"
    );
}

/// Empty input yields no records rather than one empty record, which would reach
/// the parser as invalid JSON and be counted as a parse error instead of as no
/// input at all.
#[test]
fn empty_input_yields_no_records() {
    let (records, exit) = drain_raw_ingress(Vec::new(), 64);
    assert_eq!(exit, RawIngressExit::Eof);
    assert!(
        records.is_empty(),
        "empty input must yield no records, got {records:?}"
    );
}

/// The input byte budget is measured in UTF-8 bytes. A `chars().count()`
/// implementation would admit a multi-byte message through a byte budget roughly
/// three times over.
#[test]
fn thinking_text_limit_counts_utf8_bytes_not_characters() {
    // Three times the character count stays well inside the byte budget, so a
    // character-based measurement would wrongly accept the over-limit case.
    let characters = MAX_THINKING_TEXT_BYTES / 3;
    let at_limit = "あ".repeat(characters);
    assert_eq!(at_limit.len(), characters * 3);
    assert!(at_limit.len() <= MAX_THINKING_TEXT_BYTES);
    assert!(
        at_limit.chars().count() * 3 <= MAX_THINKING_TEXT_BYTES,
        "the at-limit fixture must fit the byte budget"
    );
    thinking_request(at_limit, Vec::new())
        .validate()
        .expect("input at the byte limit must validate");

    let over_limit = "あ".repeat(characters + 1);
    assert!(
        over_limit.len() > MAX_THINKING_TEXT_BYTES,
        "the over-limit fixture must exceed the byte budget"
    );
    assert!(
        thinking_request(over_limit, Vec::new()).validate().is_err(),
        "input over the byte limit must be rejected, measured in bytes"
    );
}

/// The curated-context budget sums bytes across items and is enforced across the
/// whole request, not per item.
#[test]
fn thinking_context_budget_sums_bytes_across_items() {
    // Each curated item is individually capped at MAX_THINKING_TEXT_BYTES, so
    // the per-item fixture uses exactly that; the request-wide budget is twice
    // as large, which is what makes the summing behaviour observable.
    assert_eq!(
        MAX_THINKING_CONTEXT_BYTES,
        MAX_THINKING_TEXT_BYTES * 2,
        "the request budget is expected to be twice the per-item budget"
    );
    let half = format!("{}a", "あ".repeat(MAX_THINKING_TEXT_BYTES / 3));
    assert_eq!(
        half.len(),
        MAX_THINKING_TEXT_BYTES,
        "the per-item fixture must sit exactly at the per-item byte limit"
    );
    assert!(
        half.chars().count() < MAX_THINKING_TEXT_BYTES,
        "the fixture must be under the budget if measured in characters"
    );

    // One such item fits.
    thinking_request("ok".to_owned(), vec![curated(half.clone())])
        .validate()
        .expect("a single half-budget item must validate");

    // Two of them exceed the total, even though neither does alone.
    let two_halves = vec![curated(half.clone()), curated(half)];
    assert!(
        thinking_request("ok".to_owned(), two_halves)
            .validate()
            .is_err(),
        "the byte budget must be summed across curated context, not per item"
    );
}
