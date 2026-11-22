//! The performer's own recent turns, for truthful short-range recall (#80 S1).
//!
//! A viewer asking "what were we just talking about?" used to get an invented
//! answer, because the reply request carried nothing the performer had said.
//! This is the smallest thing that fixes that: a bounded, in-memory ring of how
//! the performer *itself* began its last few spoken turns, passed to a chat
//! reply as one typed data item.
//!
//! * Only the performer's own gated, audible output is recorded (the first
//!   phrase of a turn, cut to [`PHRASE_MAX_CHARS`]); viewer text is never
//!   stored, so nothing untrusted is carried forward.
//! * At most [`RECENT_TURNS_MAX`] entries, each live for [`RECENT_TURNS_TTL_MS`]
//!   of the app's logical clock. Eviction reads only the `now_ms` it is given,
//!   never the wall clock, so replay sees the same ring.
//! * Nothing is persisted and the ring grants no authority: it is data in the
//!   Thinking request, off unless `AIVTUBER_RECENT_TOPICS` composes it.

use std::collections::VecDeque;

use aivtuber_adapters::RECENT_TURNS_LABEL;
use aivtuber_domain::{PrivacyClass, ThinkingContent, ThinkingContextSource, TrustLevel};
use aivtuber_scheduler::PhraseLink;

use super::*;

/// Entries kept.
pub const RECENT_TURNS_MAX: usize = 6;
/// How long an entry stays, in logical milliseconds (10 minutes).
pub const RECENT_TURNS_TTL_MS: u64 = 10 * 60 * 1_000;
/// Characters kept of a turn's first spoken phrase.
pub const PHRASE_MAX_CHARS: usize = 40;
/// Characters kept of the event id used only to join a turn's phrases.
const ROOT_MAX_CHARS: usize = 96;

/// What kind of turn the performer spoke. Closed: it never carries text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKind {
    ChatReply,
    Initiative,
}

impl TurnKind {
    fn name(self) -> &'static str {
        match self {
            Self::ChatReply => "chat_reply",
            Self::Initiative => "initiative",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Turn {
    root: String,
    at_ms: u64,
    kind: TurnKind,
    phrase: String,
}

#[derive(Debug, Default)]
pub struct RecentTurns {
    turns: VecDeque<Turn>,
}

impl RecentTurns {
    fn evict(&mut self, now_ms: u64) {
        self.turns
            .retain(|turn| now_ms.saturating_sub(turn.at_ms) < RECENT_TURNS_TTL_MS);
    }

    /// Records the first spoken phrase of a turn. A phrased reply plays as
    /// several items (`{root}#p{seq}`); only the first one of a root is kept.
    pub fn record(&mut self, event_id: &str, kind: TurnKind, text: &str, now_ms: u64) {
        let phrase = cut_phrase(text);
        if phrase.is_empty() {
            return;
        }
        self.evict(now_ms);
        let root: String = PhraseLink::root_event_id(event_id)
            .chars()
            .take(ROOT_MAX_CHARS)
            .collect();
        if self.turns.iter().any(|turn| turn.root == root) {
            return;
        }
        if self.turns.len() == RECENT_TURNS_MAX {
            self.turns.pop_front();
        }
        self.turns.push_back(Turn {
            root,
            at_ms: now_ms,
            kind,
            phrase,
        });
    }

    pub fn len(&self) -> usize {
        self.turns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// The text of the one data item, oldest first. An empty ring renders
    /// `recent_turns: none`, which is what makes the adapter's instruction say
    /// "I don't remember" rather than leaving the model to invent.
    pub fn render(&mut self, now_ms: u64) -> String {
        self.evict(now_ms);
        if self.turns.is_empty() {
            return format!("{RECENT_TURNS_LABEL} none");
        }
        let mut text = RECENT_TURNS_LABEL.to_owned();
        for turn in &self.turns {
            text.push_str("\n- ");
            text.push_str(turn.kind.name());
            text.push_str(": ");
            text.push_str(&turn.phrase);
        }
        text
    }

    /// The typed context item for a chat reply.
    pub fn thinking_content(&mut self, now_ms: u64) -> ThinkingContent {
        ThinkingContent {
            source: ThinkingContextSource::RecentInteraction,
            event_id: None,
            event_kind: None,
            source_class: None,
            // The performer's own gated output, never a viewer's words, and
            // still never an instruction.
            trust_level: TrustLevel::SemiTrusted,
            privacy: PrivacyClass::Public,
            epistemic_class: None,
            text: self.render(now_ms),
        }
    }
}

/// One line, whitespace collapsed, cut to [`PHRASE_MAX_CHARS`] characters.
fn cut_phrase(text: &str) -> String {
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
        if line.chars().count() >= PHRASE_MAX_CHARS {
            break;
        }
    }
    line.chars()
        .filter(|c| !c.is_control())
        .take(PHRASE_MAX_CHARS)
        .collect()
}

impl<R> ProductionApp<R>
where
    R: RoutePlanner,
{
    /// Opt in to recent-turn recall. Without this call a request is
    /// byte-identical to one built before it existed.
    pub fn with_recent_turns(mut self) -> Self {
        self.recent_turns = Some(RecentTurns::default());
        self
    }

    pub fn recent_turns(&self) -> Option<&RecentTurns> {
        self.recent_turns.as_ref()
    }

    /// Called where audio the output accepted is recorded: the only place the
    /// ring is written. `text` is the performer's own display text.
    pub(crate) fn note_recent_turn(&mut self, event_id: &str, text: &str, now_ms: u64) {
        let kind = if event_id.starts_with(initiative::EVENT_ID_PREFIX) {
            TurnKind::Initiative
        } else {
            TurnKind::ChatReply
        };
        if let Some(turns) = self.recent_turns.as_mut() {
            turns.record(event_id, kind, text, now_ms);
        }
    }

    /// Adds the ring to a chat reply's request. Initiatives never get it: they
    /// are told not to refer to earlier conversation. A request that does not
    /// validate with it is kept exactly as routed.
    pub(crate) fn attach_recent_turns(
        &mut self,
        event: &EventEnvelope,
        route: &mut PlaybackRoute,
        now_ms: u64,
    ) {
        if self.recent_turns.is_none() || self.is_initiative_event(event) {
            return;
        }
        let PlaybackRoute::Generate(generation) = route else {
            return;
        };
        let Some(turns) = self.recent_turns.as_mut() else {
            return;
        };
        generation
            .thinking
            .curated_context
            .push(turns.thinking_content(now_ms));
        if generation.thinking.validate().is_err() {
            generation.thinking.curated_context.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000;

    #[test]
    fn the_ring_keeps_the_newest_six() {
        let mut ring = RecentTurns::default();
        for n in 0..10u64 {
            ring.record(
                &format!("e{n}"),
                TurnKind::ChatReply,
                &format!("話{n}"),
                n * SEC,
            );
        }
        assert_eq!(ring.len(), RECENT_TURNS_MAX);
        let text = ring.render(10 * SEC);
        assert!(!text.contains("話3") && text.contains("話4") && text.contains("話9"));
    }

    #[test]
    fn an_entry_expires_on_logical_time_only() {
        let mut ring = RecentTurns::default();
        ring.record("e1", TurnKind::ChatReply, "古い話", 0);
        ring.record("e2", TurnKind::Initiative, "新しい話", 5 * 60 * SEC);
        assert!(ring.render(RECENT_TURNS_TTL_MS - 1).contains("古い話"));
        let text = ring.render(RECENT_TURNS_TTL_MS);
        assert!(!text.contains("古い話") && text.contains("新しい話"));
        assert_eq!(
            ring.render(5 * 60 * SEC + RECENT_TURNS_TTL_MS),
            "recent_turns: none"
        );
        assert!(ring.is_empty());
    }

    #[test]
    fn a_phrase_is_cut_to_forty_characters_on_one_line() {
        let mut ring = RecentTurns::default();
        ring.record("e1", TurnKind::ChatReply, &"あ".repeat(100), 0);
        ring.record("e2", TurnKind::ChatReply, "一行目\n二行目\u{7}", 1);
        let text = ring.render(2);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(
            lines[1],
            format!("- chat_reply: {}", "あ".repeat(PHRASE_MAX_CHARS))
        );
        assert_eq!(lines[2], "- chat_reply: 一行目 二行目");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn only_the_first_phrase_of_a_turn_is_kept() {
        let mut ring = RecentTurns::default();
        ring.record("e1", TurnKind::ChatReply, "最初", 0);
        ring.record("e1#p1", TurnKind::ChatReply, "二つ目", 1);
        ring.record("e1#p2", TurnKind::ChatReply, "三つ目", 2);
        assert_eq!(ring.len(), 1);
        assert!(!ring.render(3).contains("二つ目"));
    }

    #[test]
    fn an_empty_ring_renders_the_none_marker() {
        let mut ring = RecentTurns::default();
        ring.record("e1", TurnKind::ChatReply, "   ", 0);
        assert_eq!(ring.render(0), "recent_turns: none");
        let item = ring.thinking_content(0);
        assert_eq!(item.source, ThinkingContextSource::RecentInteraction);
        assert_ne!(item.trust_level, TrustLevel::Trusted);
    }
}
