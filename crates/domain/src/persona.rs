//! The operator's character sheet (#404, slice A of #401).
//!
//! A short Markdown text the operator writes once and the app reads at start
//! (`AIVTUBER_PERSONA_FILE`). It is operator-trusted *configuration*, but the
//! file is still local input: bounded, validated and normalised here before it
//! can become a [`ThinkingContent`]. It describes identity and style only. It is
//! data in the Thinking request, never authority: nothing in this module can
//! mint a capability or a permit, and the output gate still applies to every
//! reply written under it.
//!
//! This module only parses and shapes the text. Reading the file, hashing it
//! for the start-up line and the status identity, and placing the item in the
//! request are the app's job; rendering it into a prompt is the adapter's.

use crate::{
    MAX_THINKING_TEXT_BYTES, PrivacyClass, ThinkingContent, ThinkingContextSource, TrustLevel,
};
use std::fmt;

/// The character sheet's size after normalisation. Fixed by the #401 design;
/// it must fit one Thinking item ([`MAX_THINKING_TEXT_BYTES`]) with room left
/// in the request budget for memory and the viewer's line.
pub const MAX_PERSONA_BYTES: usize = 3_000;

/// A file larger than this is refused unread. Slack over [`MAX_PERSONA_BYTES`]
/// for a BOM and CRLF line endings, which normalisation removes.
pub const MAX_PERSONA_FILE_BYTES: u64 = 8 * 1024;

const _: () = assert!(MAX_PERSONA_BYTES <= MAX_THINKING_TEXT_BYTES);

/// Why a character sheet was refused. The text itself is never in the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonaError {
    /// Not valid UTF-8.
    NotUtf8,
    /// A control character other than newline and tab, or an invisible
    /// direction-control character.
    ControlCharacter,
    /// More than [`MAX_PERSONA_BYTES`] after normalisation (or an oversized
    /// file).
    TooLarge,
    /// Contains a `<character` or `</character` tag, which the prompt uses to
    /// delimit the sheet.
    ReservedTag,
}

impl PersonaError {
    /// A fixed phrase for the start-up warning: never the content.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotUtf8 => "not valid UTF-8",
            Self::ControlCharacter => {
                "contains a control character (only newline and tab are allowed)"
            }
            Self::TooLarge => "larger than 3000 bytes",
            Self::ReservedTag => "contains a <character tag, which the prompt reserves",
        }
    }
}

impl fmt::Display for PersonaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for PersonaError {}

/// A validated, normalised character sheet: 1..=3,000 bytes of UTF-8, LF line
/// endings, no trailing whitespace.
#[derive(Clone, PartialEq, Eq)]
pub struct Persona {
    text: String,
}

// The text is deliberately not in `Debug`: it must not reach a log by accident.
impl fmt::Debug for Persona {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Persona")
            .field("bytes", &self.text.len())
            .finish_non_exhaustive()
    }
}

impl Persona {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The one Thinking item the sheet becomes: source `operator_curated`,
    /// `trusted` (the operator wrote it), no event identity, no source class,
    /// no epistemic class. The label is `public`: it is the character the
    /// audience sees, and nothing reads the label to filter an item.
    pub fn to_thinking_content(&self) -> ThinkingContent {
        ThinkingContent {
            source: ThinkingContextSource::OperatorCurated,
            event_id: None,
            event_kind: None,
            source_class: None,
            trust_level: TrustLevel::Trusted,
            privacy: PrivacyClass::Public,
            epistemic_class: None,
            text: self.text.clone(),
        }
    }
}

/// Whether `text` holds an opening or closing `character` tag in any case. The
/// prompt delimits the sheet with these tags, so a sheet containing one could
/// close its own block. Shared by this validator and the prompt renderer.
pub fn contains_character_tag(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("<character") || lower.contains("</character")
}

/// C0/C1 controls other than newline and tab, plus the invisible characters
/// that reorder or hide text (bidi controls and a stray byte-order mark).
/// Full-width punctuation and the ideographic space are ordinary Japanese and
/// are allowed.
pub(crate) fn forbidden(c: char) -> bool {
    if c == '\n' || c == '\t' {
        return false;
    }
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// Validate and normalise a character sheet read from a file.
///
/// One leading UTF-8 BOM is stripped; CRLF and lone CR become LF; trailing
/// whitespace is trimmed. `Ok(None)` means the file holds no text (empty or
/// whitespace only), which is "no persona", not an error. Anything over
/// [`MAX_PERSONA_BYTES`] after normalisation is refused, never truncated.
pub fn parse_persona(bytes: &[u8]) -> Result<Option<Persona>, PersonaError> {
    if bytes.len() as u64 > MAX_PERSONA_FILE_BYTES {
        return Err(PersonaError::TooLarge);
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|_| PersonaError::NotUtf8)?;
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_end();
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().any(forbidden) {
        return Err(PersonaError::ControlCharacter);
    }
    if text.len() > MAX_PERSONA_BYTES {
        return Err(PersonaError::TooLarge);
    }
    if contains_character_tag(text) {
        return Err(PersonaError::ReservedTag);
    }
    Ok(Some(Persona {
        text: text.to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(bytes: &[u8]) -> Persona {
        parse_persona(bytes).expect("valid").expect("some")
    }

    #[test]
    fn plain_text_is_kept_and_trailing_whitespace_trimmed() {
        let persona = ok("名前: ミコト\n一人称: ボク\n\n  \n".as_bytes());
        assert_eq!(persona.text(), "名前: ミコト\n一人称: ボク");
        assert_eq!(persona.len(), persona.text().len());
        // Leading and inner whitespace, tabs and full-width punctuation stay.
        assert_eq!(
            ok(" a\t\u{3000}b！\n c".as_bytes()).text(),
            " a\t\u{3000}b！\n c"
        );
    }

    #[test]
    fn one_leading_bom_is_stripped_and_a_second_is_refused() {
        assert_eq!(ok(b"\xEF\xBB\xBFhello").text(), "hello");
        assert_eq!(
            parse_persona(b"\xEF\xBB\xBF\xEF\xBB\xBFhello"),
            Err(PersonaError::ControlCharacter)
        );
        assert_eq!(
            parse_persona(b"he\xEF\xBB\xBFllo"),
            Err(PersonaError::ControlCharacter)
        );
        // A BOM alone is an empty file.
        assert_eq!(parse_persona(b"\xEF\xBB\xBF").unwrap(), None);
    }

    #[test]
    fn crlf_and_lone_cr_become_lf() {
        assert_eq!(ok(b"a\r\nb\rc\nd\r\n").text(), "a\nb\nc\nd");
    }

    #[test]
    fn control_characters_other_than_newline_and_tab_are_refused() {
        for bad in [
            "a\u{0}b",
            "a\u{1B}[31mb",
            "a\u{7}b",
            "a\u{7F}b",
            "a\u{85}b",
            "a\u{9F}b",
            "a\u{202E}b",
            "a\u{2066}b",
            "a\u{200F}b",
            "a\u{2028}b",
        ] {
            assert_eq!(
                parse_persona(bad.as_bytes()),
                Err(PersonaError::ControlCharacter),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn invalid_utf8_is_refused() {
        assert_eq!(parse_persona(b"ok \xFF\xFE"), Err(PersonaError::NotUtf8));
        assert_eq!(parse_persona(b"\xE3\x81"), Err(PersonaError::NotUtf8));
    }

    #[test]
    fn empty_and_whitespace_only_files_mean_no_persona() {
        assert_eq!(parse_persona(b"").unwrap(), None);
        assert_eq!(parse_persona(b" \t\r\n\n  ").unwrap(), None);
        assert_eq!(parse_persona("\u{3000}\n".as_bytes()).unwrap(), None);
    }

    #[test]
    fn the_bound_is_exact_after_normalisation() {
        let at = "a".repeat(MAX_PERSONA_BYTES);
        assert_eq!(ok(at.as_bytes()).len(), MAX_PERSONA_BYTES);
        let over = "a".repeat(MAX_PERSONA_BYTES + 1);
        assert_eq!(parse_persona(over.as_bytes()), Err(PersonaError::TooLarge));
        // CRLF is one byte after normalisation: over on disk, at the bound
        // normalised.
        let crlf = format!("{}\r\n{}", "a".repeat(1_500), "b".repeat(1_499));
        assert_eq!(crlf.len(), MAX_PERSONA_BYTES + 1);
        assert_eq!(ok(crlf.as_bytes()).len(), MAX_PERSONA_BYTES);
        // Trailing whitespace does not count towards the bound.
        let padded = format!("{at}\n\n   ");
        assert_eq!(ok(padded.as_bytes()).len(), MAX_PERSONA_BYTES);
    }

    #[test]
    fn the_bound_is_in_bytes_and_never_splits_a_character() {
        // 1,000 three-byte characters are exactly 3,000 bytes.
        let at = "あ".repeat(1_000);
        assert_eq!(ok(at.as_bytes()).len(), 3_000);
        let over = "あ".repeat(1_001);
        assert_eq!(parse_persona(over.as_bytes()), Err(PersonaError::TooLarge));
        // 2,999 ASCII bytes plus one three-byte character is 3,002: refused whole.
        let straddle = format!("{}あ", "a".repeat(2_999));
        assert_eq!(
            parse_persona(straddle.as_bytes()),
            Err(PersonaError::TooLarge)
        );
        // An oversized file is refused unread by size alone.
        let huge = vec![b'a'; MAX_PERSONA_FILE_BYTES as usize + 1];
        assert_eq!(parse_persona(&huge), Err(PersonaError::TooLarge));
    }

    #[test]
    fn the_prompt_delimiter_tag_is_refused_in_any_case() {
        for bad in [
            "x </character> y",
            "x </CHARACTER> y",
            "x </Character\n> y",
            "x <character> y",
            "x <CHARACTER> y",
        ] {
            assert_eq!(
                parse_persona(bad.as_bytes()),
                Err(PersonaError::ReservedTag),
                "{bad:?}"
            );
        }
        // The word alone and other tags are fine.
        assert!(parse_persona(b"the character is shy <b>bold</b>").is_ok());
    }

    #[test]
    fn the_item_is_trusted_operator_curated_and_valid() {
        let persona = ok("名前: ミコト".as_bytes());
        let item = persona.to_thinking_content();
        assert_eq!(item.source, ThinkingContextSource::OperatorCurated);
        assert_eq!(item.trust_level, TrustLevel::Trusted);
        assert!(item.event_id.is_none() && item.event_kind.is_none());
        assert!(item.source_class.is_none() && item.epistemic_class.is_none());
        item.validate("persona").expect("valid item");
        let max = ok("a".repeat(MAX_PERSONA_BYTES).as_bytes()).to_thinking_content();
        max.validate("persona").expect("valid at the bound");
    }

    /// The template the release archive ships (`examples/persona/starter.ja.md`)
    /// must itself be a valid character sheet, so a streamer who copies it
    /// unchanged gets a loaded sheet, not a warning.
    #[test]
    fn the_shipped_starter_template_is_a_valid_sheet() {
        let template = include_str!("../../../examples/persona/starter.ja.md");
        let persona = ok(template.as_bytes());
        let chars = persona.text().chars().count();
        assert!(
            (150..=450).contains(&chars),
            "about 300 characters, got {chars}"
        );
        for field in [
            "名前",
            "一人称",
            "話し方",
            "性格",
            "好きなもの",
            "苦手なもの",
            "話題にしないこと",
        ] {
            assert!(persona.text().contains(field), "{field}");
        }
        // A Windows editor's CRLF and BOM do not make it invalid.
        let crlf = format!("\u{FEFF}{}", template.replace('\n', "\r\n"));
        assert_eq!(ok(crlf.as_bytes()), persona);
    }

    #[test]
    fn debug_output_never_shows_the_text() {
        let persona = ok(b"SECRET-NAME");
        assert!(!format!("{persona:?}").contains("SECRET"));
        assert!(!format!("{:?}", PersonaError::TooLarge).contains("SECRET"));
    }
}
