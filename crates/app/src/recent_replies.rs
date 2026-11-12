//! A bounded view of what the performer recently said, for the local operation
//! panel (issue #335). Text only, newest last, fixed capacity and per-entry
//! size: an old entry is overwritten, never an allocation that grows.
//!
//! Only audio that the output accepted is recorded, with the display text of
//! the asset that produced it. For generated replies that text has already
//! passed the publication-containment gate (docs/security-threat-model.adoc
//! section 11); this view does not gate again and gives no authority.

use serde::Serialize;
use std::collections::VecDeque;

/// Replies kept. Small on purpose: the panel shows a conversation tail.
pub const RECENT_REPLY_CAPACITY: usize = 16;
/// Characters kept per reply; longer text is cut and marked.
pub const RECENT_REPLY_MAX_CHARS: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplyRecord {
    /// Strictly increasing per run, so a consumer can tell what is new.
    pub seq: u64,
    /// The event the reply answers (`panel-chat-N` for a panel line).
    pub event_id: String,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct RecentReplies {
    items: VecDeque<ReplyRecord>,
    next_seq: u64,
}

impl RecentReplies {
    pub fn record(&mut self, event_id: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let mut shown: String = text.chars().take(RECENT_REPLY_MAX_CHARS).collect();
        if text.chars().count() > RECENT_REPLY_MAX_CHARS {
            shown.push('…');
        }
        if self.items.len() == RECENT_REPLY_CAPACITY {
            self.items.pop_front();
        }
        self.next_seq += 1;
        self.items.push_back(ReplyRecord {
            seq: self.next_seq,
            event_id: event_id.chars().take(96).collect(),
            text: shown,
        });
    }

    pub fn items(&self) -> impl ExactSizeIterator<Item = &ReplyRecord> {
        self.items.iter()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_keeps_only_the_newest_capacity_entries() {
        let mut replies = RecentReplies::default();
        for i in 0..(RECENT_REPLY_CAPACITY + 5) {
            replies.record(&format!("e{i}"), &format!("text {i}"));
        }
        assert_eq!(replies.len(), RECENT_REPLY_CAPACITY);
        let first = replies.items().next().unwrap();
        assert_eq!(first.event_id, "e5");
        assert_eq!(
            replies.items().last().unwrap().seq,
            (RECENT_REPLY_CAPACITY + 5) as u64
        );
    }

    #[test]
    fn each_entry_is_bounded_and_empty_text_is_ignored() {
        let mut replies = RecentReplies::default();
        replies.record("e", "   ");
        assert!(replies.is_empty());
        replies.record("e", &"あ".repeat(RECENT_REPLY_MAX_CHARS * 3));
        let text = &replies.items().next().unwrap().text;
        assert_eq!(text.chars().count(), RECENT_REPLY_MAX_CHARS + 1);
        assert!(text.ends_with('…'));
    }
}
