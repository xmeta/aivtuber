//! Fuzz the Jev / System One response parsing and normalization boundary
//! (issue #146).
//!
//! This is a provider trust boundary: the response body is untrusted bytes from
//! a remote engine, and its normalized form drives a `ReflexDecision` — which in
//! turn selects what the avatar does. Two stages are exercised:
//!
//! 1. `parse_jev_response_body` on arbitrary bytes. Must reject, never panic.
//! 2. `normalize_answers` on the parsed answers, against a bounded candidate
//!    list. Must either normalize or return a message, never panic and never
//!    index out of bounds.
//!
//! Both functions are pure: no clock, no transport. `latency_ms` is passed in
//! rather than measured, so a failure reproduces exactly from the corpus input.
//!
//! No network I/O happens here: the transport is never constructed, and the
//! parse/normalize boundary is exercised directly.

#![no_main]

use aivtuber_reflex::{normalize_answers, parse_jev_response_body};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(response) = parse_jev_response_body(data) else {
        return;
    };

    // Candidate ids are derived from the fuzz input so the normalizer is
    // exercised against ids it has never seen, including ones that collide with
    // answer keys and ones that are empty.
    let mut candidates = Vec::new();
    for (index, chunk) in data.chunks(16).enumerate().take(8) {
        let id = String::from_utf8_lossy(chunk);
        candidates.push(id.chars().take(64).collect::<String>());
        if index == 3 {
            // Also offer a plain indexed id, so the target covers ids that look
            // like the ones the runtime actually generates.
            candidates.push(format!("candidate-{index}"));
        }
    }
    let candidates: Vec<String> = candidates.into_iter().filter(|id| !id.is_empty()).collect();

    let Ok(normalized) = normalize_answers(
        &response.answers,
        &candidates,
        "requested-alias",
        &response.model,
        1.0,
    ) else {
        return;
    };

    // A selected candidate must be one the runtime actually offered. Selecting
    // an id that was never requested would let a provider steer playback
    // towards an asset the retrieval stage excluded.
    if let Some(selected) = normalized.selected_candidate_id.as_deref() {
        assert!(
            candidates.iter().any(|candidate| candidate == selected),
            "normalized selected an unrequested candidate {selected:?} from {candidates:?}"
        );
    }

    // Normalization must be deterministic: same input, same output. A hidden
    // HashMap iteration order here would make decisions unreproducible.
    let again = normalize_answers(
        &response.answers,
        &candidates,
        "requested-alias",
        &response.model,
        1.0,
    )
    .expect("normalization is deterministic");
    assert_eq!(
        normalized.decision, again.decision,
        "normalization is not deterministic"
    );
    assert_eq!(
        normalized.answers, again.answers,
        "normalized answers are not deterministic"
    );
});
