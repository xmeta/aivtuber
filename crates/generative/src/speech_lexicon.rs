//! Bounded, versioned reading lexicon and the display-to-speech projection
//! built on it (ADR-0004 D2 to D4, issue #222 S1).
//!
//! A lexicon is an operator-owned list of `surface -> reading` entries. The
//! projection first applies the deletion rules of [`crate::speech_text`]
//! (URLs, emoji, kaomoji; ADR-0004 D-5) and then splices readings into the
//! result, so a reading can never reintroduce a URL or an emoji.
//!
//! Invariants, each tested:
//!
//! * **Readings only.** A reading is 1 to 64 hiragana, katakana or prolonged
//!   sound mark characters; an entry cannot say anything but how a word is
//!   pronounced. Model output, chat and the TTS provider cannot add entries:
//!   files come from the operator (an environment path) or the binary.
//! * **Deterministic.** Longest match first; a tie cannot occur because
//!   surfaces are unique. ASCII letters match case-insensitively unless the
//!   entry sets `case_sensitive`. Fullwidth ASCII (`ＹｏｕＴｕｂｅ`) matches
//!   its halfwidth surface (matching only: unmatched text is copied as it was).
//! * **Word boundaries for Latin.** An entry whose first (last) character is an
//!   ASCII letter or digit only matches when the preceding (following) character
//!   is not an ASCII letter, digit or `_`: `AI` does not match inside `MAIL` or
//!   `AIs`. Entries that begin or end with other characters (Japanese words,
//!   symbols) match anywhere.
//! * **No analyzer.** There is no morphological analysis in this stage
//!   (ADR-0004 D-1). Numbers, counters and ordinary kanji are left to the TTS
//!   engine unless an entry says otherwise.
//! * **Bounded.** At most [`MAX_LEXICON_ENTRIES`] entries and
//!   [`MAX_LEXICON_BYTES`] bytes per file, surfaces of 1 to
//!   [`MAX_SURFACE_CHARS`] and readings of 1 to [`MAX_READING_CHARS`]
//!   characters, at most [`MAX_LEXICON_LAYERS`] layers merged. A file that
//!   violates a bound is rejected whole, with the offending entry named.
//!   Text longer than [`MAX_LEXICON_INPUT_CHARS`] characters is projected by the
//!   deletion rules only.
//! * **Identity.** [`ProjectionIdentity`] carries the rules version and a
//!   SHA-256 of the *effective* (merged, canonicalised) entries, so reordering a
//!   file, whitespace, or a CRLF checkout does not change it and any change of
//!   a reading does.
//!
//! The JSON wire format is `schemas/speech-lexicon.schema.json`.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::speech_text::{SpeechProjectionReport, project_speech_text};

/// Version of the deletion rules plus the lexicon engine. Bump it when the
/// projection of the same display text and lexicon can change.
pub const SPEECH_RULES_VERSION: &str = "1";
pub const SPEECH_LEXICON_SCHEMA_VERSION: &str = "0.1.0";
pub const MAX_LEXICON_ENTRIES: usize = 2048;
pub const MAX_LEXICON_BYTES: usize = 131_072;
pub const MAX_SURFACE_CHARS: usize = 32;
pub const MAX_READING_CHARS: usize = 64;
pub const MAX_LEXICON_LAYERS: usize = 4;
/// Longest text the lexicon scan runs on; keeps worst-case work bounded.
pub const MAX_LEXICON_INPUT_CHARS: usize = 4096;
const MAX_LAYER_LABEL_CHARS: usize = 160;

const DEFAULT_LEXICON_JSON: &str = include_str!("../data/default-speech-lexicon.json");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechLexiconError {
    Io(String),
    TooLarge { bytes: usize },
    Parse(String),
    Invalid { path: String, reason: String },
    TooManyLayers,
}

impl fmt::Display for SpeechLexiconError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "speech lexicon cannot be read: {message}"),
            Self::TooLarge { bytes } => write!(
                f,
                "speech lexicon is larger than {MAX_LEXICON_BYTES} bytes ({bytes} bytes)"
            ),
            Self::Parse(message) => write!(f, "speech lexicon is not valid: {message}"),
            Self::Invalid { path, reason } => {
                write!(f, "speech lexicon {path}: {reason}")
            }
            Self::TooManyLayers => {
                write!(
                    f,
                    "at most {MAX_LEXICON_LAYERS} speech lexicons can be merged"
                )
            }
        }
    }
}

impl Error for SpeechLexiconError {}

fn invalid(path: impl Into<String>, reason: impl Into<String>) -> SpeechLexiconError {
    SpeechLexiconError::Invalid {
        path: path.into(),
        reason: reason.into(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LexiconFile {
    schema_version: String,
    lexicon_id: String,
    revision: String,
    entries: Vec<EntryFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryFile {
    surface: String,
    reading: String,
    source: String,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    note: Option<String>,
}

/// One validated entry. `surface` is stored width-folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexiconEntry {
    pub surface: String,
    pub reading: String,
    pub case_sensitive: bool,
    /// Where the reading came from (`self-authored`, `operator`, ...).
    pub source: String,
}

/// One validated lexicon file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexiconLayer {
    pub id: String,
    pub revision: String,
    pub entries: Vec<LexiconEntry>,
}

fn is_kana_reading_char(c: char) -> bool {
    matches!(c as u32, 0x3041..=0x3096 | 0x30A1..=0x30FA | 0x30FC)
}

/// Fullwidth ASCII to halfwidth for matching; one character in, one out, so
/// positions in the folded and the original text agree.
fn fold_width(c: char) -> char {
    match c as u32 {
        0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        0x3000 => ' ',
        _ => c,
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (1..=63).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

fn is_valid_revision(revision: &str) -> bool {
    (1..=32).contains(&revision.len())
        && revision
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl LexiconLayer {
    /// Parse and validate one lexicon file's text.
    pub fn parse(json: &str) -> Result<Self, SpeechLexiconError> {
        if json.len() > MAX_LEXICON_BYTES {
            return Err(SpeechLexiconError::TooLarge { bytes: json.len() });
        }
        let file: LexiconFile =
            serde_json::from_str(json).map_err(|e| SpeechLexiconError::Parse(e.to_string()))?;
        if file.schema_version != SPEECH_LEXICON_SCHEMA_VERSION {
            return Err(invalid(
                "schema_version",
                format!(
                    "expected {SPEECH_LEXICON_SCHEMA_VERSION}, got {}",
                    file.schema_version
                ),
            ));
        }
        if !is_valid_id(&file.lexicon_id) {
            return Err(invalid(
                "lexicon_id",
                "must match ^[a-z0-9][a-z0-9._-]{0,62}$",
            ));
        }
        if !is_valid_revision(&file.revision) {
            return Err(invalid("revision", "must match ^[A-Za-z0-9._-]{1,32}$"));
        }
        if file.entries.len() > MAX_LEXICON_ENTRIES {
            return Err(invalid(
                "entries",
                format!(
                    "{} entries, at most {MAX_LEXICON_ENTRIES} are allowed",
                    file.entries.len()
                ),
            ));
        }
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        let mut entries = Vec::with_capacity(file.entries.len());
        for (index, raw) in file.entries.into_iter().enumerate() {
            let at = |field: &str| format!("entries[{index}].{field}");
            let surface_chars = raw.surface.chars().count();
            if !(1..=MAX_SURFACE_CHARS).contains(&surface_chars) {
                return Err(invalid(
                    at("surface"),
                    format!("must be 1 to {MAX_SURFACE_CHARS} characters"),
                ));
            }
            if raw.surface.chars().any(char::is_control)
                || raw.surface.trim() != raw.surface
                || raw.surface.trim().is_empty()
            {
                return Err(invalid(
                    at("surface"),
                    "must not contain control characters or leading/trailing whitespace",
                ));
            }
            let reading_chars = raw.reading.chars().count();
            if !(1..=MAX_READING_CHARS).contains(&reading_chars) {
                return Err(invalid(
                    at("reading"),
                    format!("must be 1 to {MAX_READING_CHARS} characters"),
                ));
            }
            if !raw.reading.chars().all(is_kana_reading_char) {
                return Err(invalid(
                    at("reading"),
                    "must be hiragana, katakana or the prolonged sound mark only",
                ));
            }
            if !(1..=64).contains(&raw.source.chars().count()) {
                return Err(invalid(at("source"), "must be 1 to 64 characters"));
            }
            if raw.note.as_ref().is_some_and(|n| n.chars().count() > 128) {
                return Err(invalid(at("note"), "must be at most 128 characters"));
            }
            let surface: String = raw.surface.chars().map(fold_width).collect();
            let key = surface.to_ascii_lowercase();
            if let Some(first) = seen.insert(key, index) {
                return Err(invalid(
                    at("surface"),
                    format!(
                        "duplicates entries[{first}] (surfaces are unique ignoring ASCII case)"
                    ),
                ));
            }
            entries.push(LexiconEntry {
                surface,
                reading: raw.reading,
                case_sensitive: raw.case_sensitive,
                source: raw.source,
            });
        }
        Ok(Self {
            id: file.lexicon_id,
            revision: file.revision,
            entries,
        })
    }

    /// Read a lexicon file, refusing more than [`MAX_LEXICON_BYTES`] bytes
    /// before parsing.
    pub fn load(path: &Path) -> Result<Self, SpeechLexiconError> {
        let file = File::open(path)
            .map_err(|e| SpeechLexiconError::Io(format!("{}: {e}", path.display())))?;
        let mut text = String::new();
        file.take(MAX_LEXICON_BYTES as u64 + 1)
            .read_to_string(&mut text)
            .map_err(|e| SpeechLexiconError::Io(format!("{}: {e}", path.display())))?;
        Self::parse(&text)
    }

    /// The committed starter lexicon of common streamer and tech vocabulary.
    pub fn builtin() -> Self {
        Self::parse(DEFAULT_LEXICON_JSON).expect("the built-in speech lexicon is valid")
    }
}

/// What a projection was made with: the identity that participates in replay
/// and asset compatibility (ADR-0004 D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionIdentity {
    pub rules_version: String,
    /// `id@revision` of each layer, lowest precedence first, joined by `;`;
    /// `none` when no lexicon is active.
    pub lexicon_layers: String,
    /// SHA-256 (lowercase hex) of the effective entries; `None` with no layers.
    pub lexicon_sha256: Option<String>,
}

impl ProjectionIdentity {
    /// Canonical key: `rules:<v>|lexicon:none` or
    /// `rules:<v>|lexicon:<layers>+<sha256>`. This is the string stored in
    /// `compatibility.speech_projection` of a performance asset.
    pub fn key(&self) -> String {
        match &self.lexicon_sha256 {
            Some(digest) => format!(
                "rules:{}|lexicon:{}+{digest}",
                self.rules_version, self.lexicon_layers
            ),
            None => format!("rules:{}|lexicon:none", self.rules_version),
        }
    }
}

#[derive(Debug, Clone)]
struct CompiledEntry {
    chars: Vec<char>,
    reading: String,
    case_sensitive: bool,
}

/// A merged, ready-to-apply lexicon.
#[derive(Debug, Clone)]
pub struct SpeechLexicon {
    entries: Vec<CompiledEntry>,
    /// ASCII-lowercased first character -> entry indexes, longest first.
    index: HashMap<char, Vec<usize>>,
    identity: ProjectionIdentity,
}

/// The result of projecting display text to speech text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechTextProjection {
    /// The gated text the publication policy reasons about; never changed.
    pub display_text: String,
    /// What is sent to TTS.
    pub speech_text: String,
    /// Rules version and lexicon identity this projection was made with.
    pub identity: ProjectionIdentity,
    /// Counts only; never contains text.
    pub report: SpeechProjectionReport,
}

impl SpeechLexicon {
    /// No entries and no identity: deletion rules only.
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
            index: HashMap::new(),
            identity: ProjectionIdentity {
                rules_version: SPEECH_RULES_VERSION.to_owned(),
                lexicon_layers: "none".to_owned(),
                lexicon_sha256: None,
            },
        }
    }

    /// The committed starter lexicon only.
    pub fn builtin() -> Self {
        Self::from_layers(&[LexiconLayer::builtin()]).expect("the built-in speech lexicon merges")
    }

    /// Merge layers. Precedence: a later layer's entry replaces an earlier
    /// layer's entry with the same surface (ignoring ASCII case), so pass the
    /// default first and the per-persona or project lexicon after it.
    pub fn from_layers(layers: &[LexiconLayer]) -> Result<Self, SpeechLexiconError> {
        if layers.len() > MAX_LEXICON_LAYERS {
            return Err(SpeechLexiconError::TooManyLayers);
        }
        if layers.is_empty() {
            return Ok(Self::empty());
        }
        let mut merged: BTreeMap<String, &LexiconEntry> = BTreeMap::new();
        for layer in layers {
            for entry in &layer.entries {
                merged.insert(entry.surface.to_ascii_lowercase(), entry);
            }
        }
        if merged.len() > MAX_LEXICON_ENTRIES {
            return Err(invalid(
                "entries",
                format!(
                    "{} merged entries, at most {MAX_LEXICON_ENTRIES} are allowed",
                    merged.len()
                ),
            ));
        }
        let label = layers
            .iter()
            .map(|l| format!("{}@{}", l.id, l.revision))
            .collect::<Vec<_>>()
            .join(";");
        if label.chars().count() > MAX_LAYER_LABEL_CHARS {
            return Err(invalid(
                "lexicon_id",
                format!("the merged layer label is longer than {MAX_LAYER_LABEL_CHARS} characters"),
            ));
        }

        let mut hasher = Sha256::new();
        hasher.update(b"aivtuber-speech-lexicon/1\0");
        let mut entries = Vec::with_capacity(merged.len());
        for entry in merged.values() {
            for part in [entry.surface.as_str(), entry.reading.as_str()] {
                hasher.update((part.len() as u32).to_be_bytes());
                hasher.update(part.as_bytes());
            }
            hasher.update([u8::from(entry.case_sensitive)]);
            entries.push(CompiledEntry {
                chars: entry.surface.chars().collect(),
                reading: entry.reading.clone(),
                case_sensitive: entry.case_sensitive,
            });
        }
        let digest = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

        let mut index: HashMap<char, Vec<usize>> = HashMap::new();
        for (position, entry) in entries.iter().enumerate() {
            index
                .entry(entry.chars[0].to_ascii_lowercase())
                .or_default()
                .push(position);
        }
        for candidates in index.values_mut() {
            // Longest first; equal lengths are ordered by position, which is
            // the sorted-key order, so the choice is deterministic.
            candidates.sort_by(|a, b| {
                entries[*b]
                    .chars
                    .len()
                    .cmp(&entries[*a].chars.len())
                    .then(a.cmp(b))
            });
        }
        Ok(Self {
            entries,
            index,
            identity: ProjectionIdentity {
                rules_version: SPEECH_RULES_VERSION.to_owned(),
                lexicon_layers: label,
                lexicon_sha256: Some(digest),
            },
        })
    }

    /// The starter lexicon, optionally with an operator file merged over it.
    /// `include_builtin = false` with no file gives the empty lexicon.
    pub fn load(
        include_builtin: bool,
        operator_file: Option<&Path>,
    ) -> Result<Self, SpeechLexiconError> {
        let mut layers = Vec::new();
        if include_builtin {
            layers.push(LexiconLayer::builtin());
        }
        if let Some(path) = operator_file {
            layers.push(LexiconLayer::load(path)?);
        }
        Self::from_layers(&layers)
    }

    pub fn identity(&self) -> &ProjectionIdentity {
        &self.identity
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Project gated display text to the text sent to TTS: deletion rules,
    /// then readings. Pure and deterministic; the display text is returned
    /// unchanged beside it.
    pub fn project(&self, display: &str) -> SpeechTextProjection {
        let base = project_speech_text(display);
        let mut report = base.report;
        let (speech_text, hits, passthrough) = self.apply(&base.text);
        report.lexicon_hits = hits;
        report.passthrough_latin = passthrough;
        SpeechTextProjection {
            display_text: display.to_owned(),
            speech_text,
            identity: self.identity.clone(),
            report,
        }
    }

    fn apply(&self, text: &str) -> (String, usize, usize) {
        let original: Vec<char> = text.chars().collect();
        if original.len() > MAX_LEXICON_INPUT_CHARS {
            return (text.to_owned(), 0, 0);
        }
        let folded: Vec<char> = original.iter().copied().map(fold_width).collect();
        let mut out = String::with_capacity(text.len());
        let (mut hits, mut passthrough) = (0, 0);
        let mut at = 0;
        while at < folded.len() {
            if let Some((length, entry)) = self.longest_at(&folded, at) {
                out.push_str(&entry.reading);
                hits += 1;
                at += length;
                continue;
            }
            if folded[at].is_ascii_alphabetic() && (at == 0 || !is_word_char(folded[at - 1])) {
                // An unknown Latin word: counted so the lexicon can grow from
                // evidence (ADR-0004 D-5), and passed through unchanged.
                let mut end = at;
                while end < folded.len() && is_word_char(folded[end]) {
                    end += 1;
                }
                if end - at >= 2 {
                    passthrough += 1;
                }
                out.extend(&original[at..end]);
                at = end;
                continue;
            }
            out.push(original[at]);
            at += 1;
        }
        (out, hits, passthrough)
    }

    fn longest_at(&self, folded: &[char], at: usize) -> Option<(usize, &CompiledEntry)> {
        let candidates = self.index.get(&folded[at].to_ascii_lowercase())?;
        'candidates: for &position in candidates {
            let entry = &self.entries[position];
            let length = entry.chars.len();
            if at + length > folded.len() {
                continue;
            }
            for (expected, actual) in entry.chars.iter().zip(&folded[at..at + length]) {
                let same = if entry.case_sensitive {
                    expected == actual
                } else {
                    expected.eq_ignore_ascii_case(actual)
                };
                if !same {
                    continue 'candidates;
                }
            }
            if entry.chars[0].is_ascii_alphanumeric() && at > 0 && is_word_char(folded[at - 1]) {
                continue;
            }
            if entry.chars[length - 1].is_ascii_alphanumeric()
                && folded.get(at + length).is_some_and(|c| is_word_char(*c))
            {
                continue;
            }
            return Some((length, entry));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn examples(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/speech-lexicons")
            .join(name)
    }

    fn json_files(dir: &str) -> Vec<PathBuf> {
        let mut files: Vec<_> = std::fs::read_dir(examples(dir))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        files.sort();
        files
    }

    fn layer(id: &str, revision: &str, entries: &[(&str, &str, bool)]) -> LexiconLayer {
        LexiconLayer {
            id: id.to_owned(),
            revision: revision.to_owned(),
            entries: entries
                .iter()
                .map(|(surface, reading, case_sensitive)| LexiconEntry {
                    surface: (*surface).to_owned(),
                    reading: (*reading).to_owned(),
                    case_sensitive: *case_sensitive,
                    source: "test".to_owned(),
                })
                .collect(),
        }
    }

    fn speak(lexicon: &SpeechLexicon, text: &str) -> String {
        lexicon.project(text).speech_text
    }

    fn file_with(entries: &str) -> String {
        format!(
            r#"{{"schema_version":"0.1.0","lexicon_id":"t","revision":"1","entries":[{entries}]}}"#
        )
    }

    #[test]
    fn the_shipped_lexicon_is_valid_self_sourced_and_within_bounds() {
        let layer = LexiconLayer::builtin();
        assert!(layer.entries.len() >= 30 && layer.entries.len() <= MAX_LEXICON_ENTRIES);
        assert!(layer.entries.iter().all(|e| e.source == "self-authored"));
        assert_eq!(SpeechLexicon::builtin().len(), layer.entries.len());
    }

    #[test]
    fn the_handling_set_projects_to_the_hand_written_speech_text() {
        #[derive(Deserialize)]
        struct Item {
            category: String,
            text: String,
            expected_speech: String,
        }
        let items: Vec<Item> = serde_json::from_str(include_str!(
            "../../../examples/tts-projection/handling-set.json"
        ))
        .expect("fixture parses");
        let lexicon = SpeechLexicon::builtin();
        let mut brand = 0;
        for item in &items {
            let projection = lexicon.project(&item.text);
            assert_eq!(
                projection.speech_text, item.expected_speech,
                "{} item {:?}",
                item.category, item.text
            );
            assert_eq!(projection.display_text, item.text);
            brand += usize::from(item.category == "brand");
        }
        assert_eq!(brand, 16);
        // Numbers and kanji items (the engine reads them well: EXP-0003) are
        // untouched by the lexicon.
        for item in items
            .iter()
            .filter(|i| matches!(i.category.as_str(), "number" | "kanji"))
        {
            assert_eq!(item.text, item.expected_speech);
        }
    }

    #[test]
    fn brand_words_are_read_and_the_display_text_is_untouched() {
        let lexicon = SpeechLexicon::builtin();
        let p = lexicon.project("今日はYouTubeとTwitchでVTuberのOBS配信だよ。AIがDiscordで話す。");
        assert_eq!(
            p.speech_text,
            "今日はユーチューブとツイッチでブイチューバーのオービーエス配信だよ。エーアイがディスコードで話す。"
        );
        assert_eq!(
            p.display_text,
            "今日はYouTubeとTwitchでVTuberのOBS配信だよ。AIがDiscordで話す。"
        );
        assert_eq!(p.report.lexicon_hits, 6);
        assert!(p.report.changed());
        assert_eq!(
            speak(&lexicon, "Twitter/Xで告知した"),
            "ツイッターエックスで告知した"
        );
        assert_eq!(speak(&lexicon, "Twitterで告知した"), "ツイッターで告知した");
    }

    #[test]
    fn latin_entries_respect_word_boundaries() {
        let lexicon = SpeechLexicon::builtin();
        for kept in ["MAIL", "RAINBOW", "AIs", "FAIR", "AI2", "_AI", "SAID AI_X"] {
            assert!(
                !speak(&lexicon, kept).contains("エーアイ"),
                "{kept} -> {}",
                speak(&lexicon, kept)
            );
        }
        assert_eq!(speak(&lexicon, "AI"), "エーアイ");
        assert_eq!(speak(&lexicon, "(AI)"), "(エーアイ)");
        assert_eq!(speak(&lexicon, "AIの話"), "エーアイの話");
        assert_eq!(speak(&lexicon, "AI、AI。"), "エーアイ、エーアイ。");
        // Whole-word YouTuber is not cut at YouTube.
        assert_eq!(speak(&lexicon, "YouTuber"), "ユーチューバー");
        assert_eq!(speak(&lexicon, "YouTubers"), "ユーチューバーたち");
    }

    #[test]
    fn matching_is_case_insensitive_unless_the_entry_says_otherwise() {
        let lexicon = SpeechLexicon::builtin();
        assert_eq!(speak(&lexicon, "youtube"), "ユーチューブ");
        assert_eq!(speak(&lexicon, "YOUTUBE"), "ユーチューブ");
        assert_eq!(speak(&lexicon, "vtuber"), "ブイチューバー");
        // AI is case sensitive: the name Ai and the romaji "ai" are not read.
        assert_eq!(speak(&lexicon, "Ai"), "Ai");
        assert_eq!(speak(&lexicon, "ai"), "ai");
    }

    #[test]
    fn fullwidth_latin_matches_and_unmatched_text_keeps_its_form() {
        let lexicon = SpeechLexicon::builtin();
        assert_eq!(speak(&lexicon, "ＹｏｕＴｕｂｅ"), "ユーチューブ");
        assert_eq!(speak(&lexicon, "Ｆｏｏ１２"), "Ｆｏｏ１２");
    }

    #[test]
    fn longest_match_wins_and_matching_is_stable() {
        let layers = [layer(
            "t",
            "1",
            &[
                ("Chat", "チャット", false),
                ("ChatGPT", "チャットジーピーティー", false),
                ("GPT", "ジーピーティー", false),
            ],
        )];
        let lexicon = SpeechLexicon::from_layers(&layers).unwrap();
        assert_eq!(
            speak(&lexicon, "ChatGPT と Chat と GPT"),
            "チャットジーピーティー と チャット と ジーピーティー"
        );
        // Entry order in the file does not matter.
        let reversed = [layer(
            "t",
            "1",
            &[
                ("GPT", "ジーピーティー", false),
                ("ChatGPT", "チャットジーピーティー", false),
                ("Chat", "チャット", false),
            ],
        )];
        let other = SpeechLexicon::from_layers(&reversed).unwrap();
        assert_eq!(lexicon.identity(), other.identity());
        // Japanese surfaces need no boundary and also use longest match.
        let ja = SpeechLexicon::from_layers(&[layer(
            "t",
            "1",
            &[
                ("約束", "やくそく", false),
                ("約束事", "やくそくごと", false),
            ],
        )])
        .unwrap();
        assert_eq!(speak(&ja, "約束事と約束"), "やくそくごととやくそく");
    }

    #[test]
    fn numbers_and_ordinary_text_are_left_alone() {
        let lexicon = SpeechLexicon::builtin();
        for kept in [
            "今日は3月5日です。",
            "チャンネル登録者が1000人を超えました。",
            "気温は18.5度だって。",
            "100円のスパチャありがとう。",
            "二十歳の誕生日をお祝いするね。",
        ] {
            let p = lexicon.project(kept);
            assert_eq!(p.speech_text, kept);
            assert!(!p.report.changed());
        }
    }

    #[test]
    fn unknown_latin_words_pass_through_and_are_counted() {
        let lexicon = SpeechLexicon::builtin();
        let p = lexicon.project("Pythonでhello worldとYouTubeとAとx1と3Dを試した");
        assert_eq!(p.report.passthrough_latin, 4, "Python hello world x1");
        assert_eq!(p.report.lexicon_hits, 1);
        assert!(p.speech_text.contains("Python"));
        assert!(p.speech_text.contains("hello world"));
        // A known word is not counted as passthrough.
        let known = lexicon.project("YouTubeとDiscord");
        assert_eq!(known.report.passthrough_latin, 0);
        // A word inside an entry that did not match on a boundary still counts once.
        assert_eq!(lexicon.project("MAIL").report.passthrough_latin, 1);
    }

    #[test]
    fn the_lexicon_runs_after_deletion_so_it_cannot_reintroduce_urls_or_emoji() {
        let lexicon = SpeechLexicon::builtin();
        let p = lexicon.project("YouTube😊 https://youtube.com/watch?v=1 を見てね✨");
        assert_eq!(p.speech_text, "ユーチューブ を見てね");
        assert_eq!(p.report.dropped_urls, 1);
        assert_eq!(p.report.dropped_emoji, 2);
        assert_eq!(p.report.lexicon_hits, 1);
        // Readings are kana only: nothing outside the input and the readings.
        let layers = [layer("t", "1", &[("Foo", "フー", false)])];
        let lexicon = SpeechLexicon::from_layers(&layers).unwrap();
        let out = speak(&lexicon, "a Foo b");
        assert!(out.chars().all(|c| "a b".contains(c) || "フー".contains(c)));
    }

    #[test]
    fn operator_entries_win_over_the_default_and_the_merge_is_bounded() {
        let operator = LexiconLayer::parse(&file_with(
            r#"{"surface":"AI","reading":"アイ","source":"operator","case_sensitive":true},
               {"surface":"youtube","reading":"ようつべ","source":"operator"},
               {"surface":"Claude","reading":"クロード","source":"operator"}"#,
        ))
        .unwrap();
        let merged =
            SpeechLexicon::from_layers(&[LexiconLayer::builtin(), operator.clone()]).unwrap();
        assert_eq!(
            speak(&merged, "AIとYouTubeとClaude"),
            "アイとようつべとクロード"
        );
        // Untouched default entries survive; the replaced one is not duplicated.
        assert_eq!(speak(&merged, "Twitch"), "ツイッチ");
        assert_eq!(merged.len(), LexiconLayer::builtin().entries.len() + 1);
        // Reversing the order reverses the winner (later layer wins).
        let reversed = SpeechLexicon::from_layers(&[operator, LexiconLayer::builtin()]).unwrap();
        assert_eq!(speak(&reversed, "AI"), "エーアイ");
        // More than four layers are refused.
        let five: Vec<_> = (0..5).map(|_| LexiconLayer::builtin()).collect();
        assert_eq!(
            SpeechLexicon::from_layers(&five).unwrap_err(),
            SpeechLexiconError::TooManyLayers
        );
    }

    #[test]
    fn bounds_are_enforced_and_name_the_offending_entry() {
        let reject = |entries: &str| {
            LexiconLayer::parse(&file_with(entries))
                .unwrap_err()
                .to_string()
        };
        let ok = |s: &str, r: &str| format!(r#"{{"surface":"{s}","reading":"{r}","source":"t"}}"#);

        assert!(LexiconLayer::parse(&file_with(&ok("a", "ア"))).is_ok());
        assert!(LexiconLayer::parse(&file_with(&ok(&"a".repeat(32), &"ア".repeat(64)))).is_ok());
        assert!(reject(&ok(&"a".repeat(33), "ア")).contains("entries[0].surface"));
        assert!(reject(&ok("a", &"ア".repeat(65))).contains("entries[0].reading"));
        assert!(reject(&ok("", "ア")).contains("surface"));
        assert!(reject(&ok(" a", "ア")).contains("surface"));
        assert!(reject(&ok("a", "")).contains("reading"));
        assert!(reject(&ok("a", "abc")).contains("hiragana, katakana"));
        assert!(reject(&ok("a", "文字")).contains("hiragana, katakana"));
        assert!(reject(&ok("a", "ア・ア")).contains("hiragana, katakana"));
        assert!(LexiconLayer::parse(&file_with(&ok("a", "アーあ"))).is_ok());
        // Duplicates ignore ASCII case, and the second one is named.
        let dup = reject(&format!("{},{}", ok("Foo", "フー"), ok("foo", "フー")));
        assert!(
            dup.contains("entries[1].surface") && dup.contains("entries[0]"),
            "{dup}"
        );
        // Unknown fields, wrong version, bad id and malformed JSON.
        assert!(
            reject(r#"{"surface":"a","reading":"ア","source":"t","ruby":"x"}"#)
                .contains("not valid")
        );
        assert!(LexiconLayer::parse("{").is_err());
        let wrong = file_with("").replace("0.1.0", "0.2.0");
        assert!(
            LexiconLayer::parse(&wrong)
                .unwrap_err()
                .to_string()
                .contains("schema_version")
        );
        let bad_id = file_with("").replace("\"t\"", "\"T T\"");
        assert!(
            LexiconLayer::parse(&bad_id)
                .unwrap_err()
                .to_string()
                .contains("lexicon_id")
        );

        // Entry count and byte caps.
        let many = (0..=MAX_LEXICON_ENTRIES)
            .map(|i| ok(&format!("w{i}"), "ア"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(matches!(
            LexiconLayer::parse(&file_with(&many)),
            Err(SpeechLexiconError::Invalid { .. } | SpeechLexiconError::TooLarge { .. })
        ));
        let exactly = (0..MAX_LEXICON_ENTRIES)
            .map(|i| ok(&format!("w{i}"), "ア"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            LexiconLayer::parse(&file_with(&exactly))
                .unwrap()
                .entries
                .len(),
            MAX_LEXICON_ENTRIES
        );
        let huge = format!("{}{}", file_with(""), " ".repeat(MAX_LEXICON_BYTES));
        assert!(matches!(
            LexiconLayer::parse(&huge),
            Err(SpeechLexiconError::TooLarge { .. })
        ));
    }

    #[test]
    fn repository_fixtures_agree_with_the_schema_classification() {
        for path in json_files("valid") {
            let text = std::fs::read_to_string(&path).unwrap();
            LexiconLayer::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
        // `invalid` fails the schema; `inconsistent` passes the schema and
        // fails a cross-field rule only the loader enforces. Both are rejected.
        for dir in ["invalid", "inconsistent"] {
            for path in json_files(dir) {
                let text = std::fs::read_to_string(&path).unwrap();
                assert!(LexiconLayer::parse(&text).is_err(), "{}", path.display());
            }
        }
        // A missing file is a typed error, not a panic.
        assert!(matches!(
            LexiconLayer::load(&examples("valid/missing.json")),
            Err(SpeechLexiconError::Io(_))
        ));
        let loaded = SpeechLexicon::load(true, Some(&examples("valid/persona.json"))).unwrap();
        assert_eq!(speak(&loaded, "AIはClaude"), "アイはクロード");
        let none = SpeechLexicon::load(false, None).unwrap();
        assert!(none.is_empty());
        assert_eq!(none.identity().key(), "rules:1|lexicon:none");
        assert_eq!(speak(&none, "YouTube"), "YouTube");
    }

    #[test]
    fn identity_is_stable_and_changes_with_any_effective_change() {
        let base = SpeechLexicon::builtin();
        let again = SpeechLexicon::builtin();
        assert_eq!(base.identity(), again.identity());
        let key = base.identity().key();
        assert!(
            aivtuber_asset_store::is_valid_speech_projection_key(&key),
            "{key}"
        );
        assert!(key.starts_with("rules:1|lexicon:default-streamer@"));

        let a = SpeechLexicon::from_layers(&[layer("t", "1", &[("Foo", "フー", false)])]).unwrap();
        let reading =
            SpeechLexicon::from_layers(&[layer("t", "1", &[("Foo", "ふー", false)])]).unwrap();
        let case =
            SpeechLexicon::from_layers(&[layer("t", "1", &[("Foo", "フー", true)])]).unwrap();
        let extra = SpeechLexicon::from_layers(&[layer(
            "t",
            "1",
            &[("Foo", "フー", false), ("Bar", "バー", false)],
        )])
        .unwrap();
        let revised =
            SpeechLexicon::from_layers(&[layer("t", "2", &[("Foo", "フー", false)])]).unwrap();
        let digests: Vec<_> = [&a, &reading, &case, &extra]
            .iter()
            .map(|l| l.identity().lexicon_sha256.clone())
            .collect();
        for (i, left) in digests.iter().enumerate() {
            for right in &digests[i + 1..] {
                assert_ne!(left, right);
            }
        }
        // The revision label is part of the key but not of the content hash.
        assert_eq!(
            a.identity().lexicon_sha256,
            revised.identity().lexicon_sha256
        );
        assert_ne!(a.identity().key(), revised.identity().key());
        // The source and note fields do not change what is spoken or the hash.
        let mut sourced = layer("t", "1", &[("Foo", "フー", false)]);
        sourced.entries[0].source = "other".to_owned();
        assert_eq!(
            SpeechLexicon::from_layers(&[sourced])
                .unwrap()
                .identity()
                .lexicon_sha256,
            a.identity().lexicon_sha256
        );
        // Every projection carries the identity it was made with.
        assert_eq!(a.project("Foo").identity, *a.identity());
        // Content hash is independent of line endings in the file.
        let lf = LexiconLayer::parse(&file_with(
            r#"{"surface":"Foo","reading":"フー","source":"t"}"#,
        ))
        .unwrap();
        let crlf = LexiconLayer::parse(&file_with(
            "\r\n{\"surface\":\"Foo\",\"reading\":\"フー\",\"source\":\"t\"}\r\n",
        ))
        .unwrap();
        assert_eq!(
            SpeechLexicon::from_layers(&[lf]).unwrap().identity(),
            SpeechLexicon::from_layers(&[crlf]).unwrap().identity()
        );
    }

    #[test]
    fn projection_is_deterministic_and_oversized_text_skips_only_the_lexicon() {
        let lexicon = SpeechLexicon::builtin();
        let text = "YouTubeでAIとVTuber😊 https://example.com/a ね";
        let first = lexicon.project(text);
        for _ in 0..20 {
            assert_eq!(first, lexicon.project(text));
        }
        let long = format!("YouTube😊{}", "あ".repeat(MAX_LEXICON_INPUT_CHARS));
        let p = lexicon.project(&long);
        assert!(p.speech_text.starts_with("YouTube"), "lexicon skipped");
        assert_eq!(p.report.dropped_emoji, 1, "deletion rules still ran");
        assert_eq!(p.report.lexicon_hits, 0);
    }
}
