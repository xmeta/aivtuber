//! Bounded retained-memory selection for one generation request (issue #105).
//!
//! The delivered-memory rule has two halves that must not drift apart:
//!
//! * **what may be recalled** — `WorkingMemory::context_for` owns actor scope,
//!   source-class sharing, supersession, and ranking;
//! * **how much of it fits** — [`retained_memory_context`] owns the byte and
//!   item budget, and is the only place that turns retained entries into
//!   request content.
//!
//! The composition root calls it for every generated reply, and the long-range
//! evaluation calls the same function, so a measured result describes what
//! production actually assembles rather than a re-implementation of it.

use aivtuber_adaptation::{MemoryEntry, WorkingMemory};
use aivtuber_domain::{
    MAX_THINKING_CONTEXT_BYTES, MAX_THINKING_CONTEXT_ITEMS, MAX_THINKING_TEXT_BYTES,
    ThinkingContent, ThinkingContextSource,
};

use crate::routing::privacy_for_source;

/// The retained claims a generation request may carry, in rank order (#105).
///
/// Only memory the write gate already admitted can appear: this reads the
/// store and never writes it, so recall can neither mint a claim, promote a
/// class, create a link, nor buy a durable write.
///
/// `charged_bytes` is the request's own text, already spent from
/// [`MAX_THINKING_CONTEXT_BYTES`]. The budget is a prefix rule, never a
/// truncation: claims are taken in rank order until the budget is reached. A
/// claim larger than one item's own bound ([`MAX_THINKING_TEXT_BYTES`]) is
/// skipped whole and the next claim uses the room, because shortening a
/// retained claim would present text the runtime never admitted as if it had —
/// the defect #231 fixed for the store, one layer up.
pub fn retained_memory_context(
    memory: &WorkingMemory,
    source_namespace: &str,
    actor_id: Option<&str>,
    topic: Option<&str>,
    now_ms: u64,
    charged_bytes: usize,
) -> Vec<ThinkingContent> {
    let entries = memory.context_for(
        source_namespace,
        actor_id,
        topic,
        MAX_THINKING_CONTEXT_ITEMS,
        now_ms,
    );
    let mut used_bytes = charged_bytes;
    let mut context = Vec::with_capacity(entries.len().min(MAX_THINKING_CONTEXT_ITEMS));
    for entry in entries {
        let text = &entry.normalized_claim;
        if text.len() > MAX_THINKING_TEXT_BYTES {
            continue;
        }
        if used_bytes.saturating_add(text.len()) > MAX_THINKING_CONTEXT_BYTES {
            break;
        }
        used_bytes += text.len();
        context.push(memory_content(entry));
    }
    context
}

/// One retained entry as request content.
///
/// The class is carried, never re-derived: generation must be able to phrase a
/// viewer claim as a claim. The entry stores no event kind, so the class and
/// source class are what the request offers instead.
fn memory_content(entry: &MemoryEntry) -> ThinkingContent {
    ThinkingContent {
        source: ThinkingContextSource::Memory,
        event_id: Some(entry.source.event_id.clone()),
        event_kind: None,
        source_class: Some(entry.source.source_class),
        trust_level: entry.source.trust_level,
        privacy: privacy_for_source(entry.source.source_class),
        epistemic_class: Some(entry.epistemic_class),
        text: entry.normalized_claim.clone(),
    }
}
