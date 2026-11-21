//! The operator's chat-topic list (#87 S2, 「雑談ネタ」).
//!
//! A short list of one-line topics the operator writes so the idle initiative
//! has something real to talk about instead of inventing a past. Like the
//! character sheet ([`crate::parse_persona`]) it is operator-trusted
//! *configuration* but local input: bounded, validated and normalised here
//! before it can become a [`ThinkingContent`]. It is data in the Thinking
//! request, never authority: nothing here mints a capability or a permit.
//!
//! One topic becomes one `operator_curated` item with `source_class`
//! `operator` (the character sheet has none), which is how the renderer tells
//! the two apart without reading the text. This module only parses and shapes
//! the text; reading and writing the file, rotation and placing the item in a
//! request are the app's job, rendering it is the adapter's.

use crate::persona::forbidden;
use crate::{
    MAX_THINKING_TEXT_BYTES, PrivacyClass, SourceClass, ThinkingContent, ThinkingContextSource,
    TrustLevel, contains_character_tag,
};
use std::fmt;

/// Most topics in the list.
pub const MAX_TOPICS: usize = 32;
/// Most characters in one topic (the line, after trimming).
pub const MAX_TOPIC_CHARS: usize = 80;
/// A file larger than this is refused unread. Slack over the worst case of
/// [`MAX_TOPICS`] lines of [`MAX_TOPIC_CHARS`] four-byte characters.
pub const MAX_TOPICS_FILE_BYTES: u64 = 16 * 1024;

// One topic is one Thinking item and must always fit it.
const _: () = assert!(MAX_TOPIC_CHARS * 4 <= MAX_THINKING_TEXT_BYTES);

/// Why a topic list was refused. The text itself is never in the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicsError {
    NotUtf8,
    /// A control character (a tab included: a topic is one plain line) or an
    /// invisible direction-control character.
    ControlCharacter,
    /// More than [`MAX_TOPICS`] distinct non-empty lines.
    TooManyTopics,
    /// A line longer than [`MAX_TOPIC_CHARS`] characters.
    TopicTooLong,
    /// An oversized file.
    TooLarge,
    /// Contains a `<topic` / `<character` tag (either closing form too), which
    /// the prompt uses to delimit operator text.
    ReservedTag,
}

impl TopicsError {
    /// A fixed phrase for the operator: never the content.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotUtf8 => "not valid UTF-8",
            Self::ControlCharacter => {
                "contains a control character (only newlines separate topics)"
            }
            Self::TooManyTopics => "more than 32 topics",
            Self::TopicTooLong => "a topic is longer than 80 characters",
            Self::TooLarge => "larger than 16 KiB",
            Self::ReservedTag => "contains a <topic or <character tag, which the prompt reserves",
        }
    }
}

impl fmt::Display for TopicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for TopicsError {}

/// A validated topic list: at most [`MAX_TOPICS`] distinct, trimmed,
/// non-empty single lines of at most [`MAX_TOPIC_CHARS`] characters.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct TopicList {
    entries: Vec<String>,
}

// The text is deliberately not in `Debug`: it must not reach a log by accident.
impl fmt::Debug for TopicList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TopicList")
            .field("topics", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl TopicList {
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The list as the file holds it: one topic per line, LF, no trailing
    /// newline. Parsing this text gives the same list back.
    pub fn to_text(&self) -> String {
        self.entries.join("\n")
    }
}

/// Whether `text` holds an opening or closing `topic` tag (or a `character`
/// one) in any case. The prompt delimits operator text with these tags.
pub fn contains_reserved_tag(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    contains_character_tag(text) || lower.contains("<topic") || lower.contains("</topic")
}

/// Validate and normalise a topic list read from a file or an operator
/// request.
///
/// One leading BOM is stripped; CRLF and lone CR become LF; each line is
/// trimmed; blank lines are dropped and a repeated line is kept once (first
/// position). An empty result is an empty list, not an error. Nothing is ever
/// truncated: a violation refuses the whole list.
pub fn parse_topics(bytes: &[u8]) -> Result<TopicList, TopicsError> {
    if bytes.len() as u64 > MAX_TOPICS_FILE_BYTES {
        return Err(TopicsError::TooLarge);
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|_| TopicsError::NotUtf8)?;
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if text
        .chars()
        .any(|c| c != '\n' && (forbidden(c) || c == '\t'))
    {
        return Err(TopicsError::ControlCharacter);
    }
    let mut entries: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.chars().count() > MAX_TOPIC_CHARS {
            return Err(TopicsError::TopicTooLong);
        }
        if contains_reserved_tag(line) {
            return Err(TopicsError::ReservedTag);
        }
        if entries.iter().any(|entry| entry == line) {
            continue;
        }
        if entries.len() == MAX_TOPICS {
            return Err(TopicsError::TooManyTopics);
        }
        entries.push(line.to_owned());
    }
    Ok(TopicList { entries })
}

/// The one Thinking item a chosen topic becomes: source `operator_curated`,
/// `trusted` (the operator wrote it) and source class `operator`, which marks
/// it as a topic rather than the character sheet (that has no source class).
/// No event identity, no epistemic class.
pub fn topic_thinking_content(topic: &str) -> ThinkingContent {
    ThinkingContent {
        source: ThinkingContextSource::OperatorCurated,
        event_id: None,
        event_kind: None,
        source_class: Some(SourceClass::Operator),
        trust_level: TrustLevel::Trusted,
        privacy: PrivacyClass::Public,
        epistemic_class: None,
        text: topic.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> TopicList {
        parse_topics(text.as_bytes()).expect("valid")
    }

    #[test]
    fn lines_are_trimmed_blank_lines_dropped_and_duplicates_kept_once() {
        let list = ok("\u{FEFF} 好きな食べ物 \r\n\r\n最近のゲーム\n\u{3000}好きな食べ物\n天気\r");
        assert_eq!(list.entries(), ["好きな食べ物", "最近のゲーム", "天気"]);
        assert_eq!(ok(&list.to_text()), list);
        assert!(ok("").is_empty() && ok(" \n\n \r\n").is_empty());
    }

    #[test]
    fn the_bounds_are_exact_and_nothing_is_truncated() {
        let at = "あ".repeat(MAX_TOPIC_CHARS);
        assert_eq!(ok(&at).entries()[0].chars().count(), MAX_TOPIC_CHARS);
        let over = "あ".repeat(MAX_TOPIC_CHARS + 1);
        assert_eq!(
            parse_topics(over.as_bytes()),
            Err(TopicsError::TopicTooLong)
        );
        let thirty_two: String = (0..MAX_TOPICS).map(|i| format!("話題{i}\n")).collect();
        assert_eq!(ok(&thirty_two).len(), MAX_TOPICS);
        let thirty_three = format!("{thirty_two}話題32");
        assert_eq!(
            parse_topics(thirty_three.as_bytes()),
            Err(TopicsError::TooManyTopics)
        );
        // A duplicate of an existing line is not a 33rd topic.
        let repeat = format!("{thirty_two}話題0");
        assert_eq!(ok(&repeat).len(), MAX_TOPICS);
        // The worst case (32 lines of four-byte characters) is under the file cap.
        let worst: String = (0..MAX_TOPICS)
            .map(|i| format!("{}{i:02}\n", "\u{1F600}".repeat(MAX_TOPIC_CHARS - 2)))
            .collect();
        assert!((worst.len() as u64) < MAX_TOPICS_FILE_BYTES);
        assert_eq!(ok(&worst).len(), MAX_TOPICS);
        let huge = vec![b'a'; MAX_TOPICS_FILE_BYTES as usize + 1];
        assert_eq!(parse_topics(&huge), Err(TopicsError::TooLarge));
    }

    #[test]
    fn control_characters_tabs_and_bad_utf8_are_refused() {
        for bad in [
            "a\u{0}b",
            "a\tb",
            "a\u{1B}[31mb",
            "a\u{202E}b",
            "a\u{FEFF}b",
        ] {
            assert_eq!(
                parse_topics(bad.as_bytes()),
                Err(TopicsError::ControlCharacter),
                "{bad:?}"
            );
        }
        assert_eq!(parse_topics(b"ok \xFF"), Err(TopicsError::NotUtf8));
    }

    #[test]
    fn the_prompt_delimiter_tags_are_refused_in_any_case() {
        for bad in [
            "x <topic> y",
            "x </TOPIC> y",
            "x <Topic",
            "x <character> y",
            "x </CHARACTER> y",
        ] {
            assert_eq!(
                parse_topics(bad.as_bytes()),
                Err(TopicsError::ReservedTag),
                "{bad:?}"
            );
        }
        assert!(parse_topics(b"the topic is <b>bold</b>").is_ok());
    }

    #[test]
    fn the_item_is_trusted_operator_curated_with_the_operator_class_and_valid() {
        let item = topic_thinking_content("好きな食べ物");
        assert_eq!(item.source, ThinkingContextSource::OperatorCurated);
        assert_eq!(item.trust_level, TrustLevel::Trusted);
        assert_eq!(item.source_class, Some(SourceClass::Operator));
        assert!(item.event_id.is_none() && item.epistemic_class.is_none());
        item.validate("topic").expect("valid item");
        topic_thinking_content(&"\u{1F600}".repeat(MAX_TOPIC_CHARS))
            .validate("topic")
            .expect("valid at the bound");
    }

    #[test]
    fn debug_never_holds_the_text() {
        assert!(!format!("{:?}", ok("ひみつの話題")).contains("ひみつ"));
    }
}
