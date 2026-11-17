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
    retained_memory_context_reserving(
        memory,
        source_namespace,
        actor_id,
        topic,
        now_ms,
        charged_bytes,
        ContextReserve::NONE,
    )
}

/// Room in the request budget that retained memory must leave free (#404): the
/// operator's character sheet is placed first and never dropped, so memory
/// fills what remains of the item and byte budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextReserve {
    pub items: usize,
    pub bytes: usize,
}

impl ContextReserve {
    pub const NONE: Self = Self { items: 0, bytes: 0 };

    /// One item of `bytes` bytes.
    pub fn one_item(bytes: usize) -> Self {
        Self { items: 1, bytes }
    }
}

/// [`retained_memory_context`] that leaves `reserve` free in the budget.
pub fn retained_memory_context_reserving(
    memory: &WorkingMemory,
    source_namespace: &str,
    actor_id: Option<&str>,
    topic: Option<&str>,
    now_ms: u64,
    charged_bytes: usize,
    reserve: ContextReserve,
) -> Vec<ThinkingContent> {
    let entries = memory.context_for(
        source_namespace,
        actor_id,
        topic,
        MAX_THINKING_CONTEXT_ITEMS.saturating_sub(reserve.items),
        now_ms,
    );
    let content = entries.into_iter().map(memory_content).collect();
    bounded_prefix_reserving(content, charged_bytes, reserve, |item: &ThinkingContent| {
        item.text.as_str()
    })
}

/// The request budget, applied to whatever retained content a request carries.
///
/// This is the only place the item and byte bounds are enforced, so the
/// runtime's own delivery and a sidecar's answer are cut by the same rule: a
/// claim larger than one item's own bound is skipped whole, and the budget is
/// a prefix rule, never a truncation.
pub fn bounded_context(
    content: Vec<ThinkingContent>,
    charged_bytes: usize,
) -> Vec<ThinkingContent> {
    bounded_prefix(content, charged_bytes, |item| item.text.as_str())
}

/// The request budget, applied to items of any shape.
///
/// [`bounded_context`] is the runtime's use of this rule; the long-range
/// evaluation applies it to what a memory sidecar answered, so a candidate's
/// delivery is cut by the same item and byte bounds the runtime obeys instead
/// of by a second implementation of them.
pub fn bounded_prefix<T>(
    content: Vec<T>,
    charged_bytes: usize,
    text: impl Fn(&T) -> &str,
) -> Vec<T> {
    bounded_prefix_reserving(content, charged_bytes, ContextReserve::NONE, text)
}

/// [`bounded_prefix`] that leaves `reserve` items and bytes free (#404). The
/// reserved bytes are charged up front and the reserved items are taken off the
/// item bound, so what the caller later adds in that room (the character sheet)
/// always fits both budgets.
pub fn bounded_prefix_reserving<T>(
    content: Vec<T>,
    charged_bytes: usize,
    reserve: ContextReserve,
    text: impl Fn(&T) -> &str,
) -> Vec<T> {
    let item_bound = MAX_THINKING_CONTEXT_ITEMS.saturating_sub(reserve.items);
    let mut used_bytes = charged_bytes.saturating_add(reserve.bytes);
    let mut bounded = Vec::with_capacity(content.len().min(item_bound));
    for item in content {
        if bounded.len() >= item_bound {
            break;
        }
        let len = text(&item).len();
        if len > MAX_THINKING_TEXT_BYTES {
            continue;
        }
        if used_bytes.saturating_add(len) > MAX_THINKING_CONTEXT_BYTES {
            break;
        }
        used_bytes += len;
        bounded.push(item);
    }
    bounded
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
