//! The operator's character sheet, read once at start (#404, slice A of #401).
//!
//! `aivtuber-setup start` names a Markdown file beside the env file in
//! `AIVTUBER_PERSONA_FILE`; the app reads it here, bounded, before it composes
//! Thinking. The text is validated and normalised by
//! [`aivtuber_domain::parse_persona`]; this module owns reading the file, the
//! load outcome the start-up line reports, and the identity (length and a
//! SHA-256 prefix, never the text) that status and the support bundle record.
//!
//! The sheet is data in the Thinking request, placed first in
//! `curated_context` (see `ProductionApp::attach_persona`) and rendered by the
//! adapter as delimited text. Nothing here can mint a capability or a permit.
//! The operator changes it by editing the file and restarting, or live (#401
//! slice B) through the `persona.configure` capability
//! (`ProductionApp::configure_persona`), which validates with the same parser
//! and writes the file atomically.

use aivtuber_domain::{
    MAX_PERSONA_FILE_BYTES, Persona, PersonaError, ThinkingContent, parse_persona,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const PERSONA_FILE_VAR: &str = "AIVTUBER_PERSONA_FILE";

/// Hex characters of the SHA-256 shown to the operator: enough to tell two
/// sheets apart, too short to be a fingerprint of the text.
pub const PERSONA_HASH_PREFIX_CHARS: usize = 12;

/// A validated sheet with its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaSheet {
    persona: Persona,
    sha256_prefix: String,
}

impl PersonaSheet {
    pub fn new(persona: Persona) -> Self {
        let digest = Sha256::digest(persona.text().as_bytes());
        let mut sha256_prefix = String::with_capacity(PERSONA_HASH_PREFIX_CHARS);
        for byte in digest.iter().take(PERSONA_HASH_PREFIX_CHARS / 2) {
            sha256_prefix.push_str(&format!("{byte:02x}"));
        }
        Self {
            persona,
            sha256_prefix,
        }
    }

    /// Bytes of the normalised text.
    pub fn len(&self) -> usize {
        self.persona.len()
    }

    pub fn is_empty(&self) -> bool {
        self.persona.is_empty()
    }

    pub fn sha256_prefix(&self) -> &str {
        &self.sha256_prefix
    }

    /// The normalised text. Only `persona.get` (operator-authenticated) and
    /// the Thinking request read it; no log, status or bundle does.
    pub fn text(&self) -> &str {
        self.persona.text()
    }

    /// The one Thinking item the sheet becomes (`operator_curated`, trusted).
    pub fn thinking_content(&self) -> ThinkingContent {
        self.persona.to_thinking_content()
    }

    pub fn status(&self) -> PersonaStatus {
        PersonaStatus {
            present: true,
            bytes: self.len(),
            sha256: Some(self.sha256_prefix.clone()),
        }
    }
}

/// The `persona` object of the operator status and the support bundle: whether
/// a sheet is loaded, its length and the SHA-256 prefix. Never the text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaStatus {
    pub present: bool,
    pub bytes: usize,
    pub sha256: Option<String>,
}

impl PersonaStatus {
    /// `AIVTUBER_PERSONA_FILE` was set but no sheet is loaded.
    pub fn absent() -> Self {
        Self {
            present: false,
            bytes: 0,
            sha256: None,
        }
    }

    /// One short, text-free identity string for the support bundle.
    pub fn identity(&self) -> String {
        match (&self.sha256, self.present) {
            (Some(hash), true) => format!("bytes={} sha256={hash}", self.bytes),
            _ => "none".to_owned(),
        }
    }
}

/// Live editing (#401 slice B): where a saved sheet is written and whether
/// Thinking is remote. Composed only with the generative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaEditor {
    path: Option<PathBuf>,
    thinking_remote: bool,
}

impl PersonaEditor {
    /// `path` is `AIVTUBER_PERSONA_FILE`; `None` applies in memory only.
    pub fn new(path: Option<PathBuf>, thinking_remote: bool) -> Self {
        Self {
            path,
            thinking_remote,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn status(&self) -> PersonaEditorStatus {
        PersonaEditorStatus {
            persisted: self.path.is_some(),
            thinking_remote: self.thinking_remote,
        }
    }
}

/// The `persona_editor` object of the operator status: present only when live
/// editing is composed. Tells the panel whether a save reaches the file and
/// whether the sheet leaves the machine with each reply. No text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaEditorStatus {
    /// A save is written to `AIVTUBER_PERSONA_FILE` (and so survives a
    /// restart); false = in memory only.
    pub persisted: bool,
    /// The Thinking endpoint leaves this machine.
    pub thinking_remote: bool,
}

/// What a `persona.*` operator action produced. `text` is set only by
/// `persona.get` and goes to the authenticated caller, never to a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaOutcome {
    pub detail: String,
    pub text: Option<String>,
}

/// Replace `path` with `text`: write a sibling temporary file, flush it to
/// disk, then rename it over the target, so a reader (or a crash) sees the old
/// file or the new one, never half of either. The temporary file is removed on
/// failure.
pub fn write_persona_file_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Why a file that exists could not become a sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonaRefusal {
    Invalid(PersonaError),
    /// The file exists but could not be read (a directory, permissions).
    Unreadable,
}

/// What reading `AIVTUBER_PERSONA_FILE` produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonaLoad {
    Loaded(PersonaSheet),
    /// The file is missing, empty or whitespace only: no persona.
    Absent,
    Refused(PersonaRefusal),
}

impl PersonaLoad {
    pub fn sheet(&self) -> Option<&PersonaSheet> {
        match self {
            Self::Loaded(sheet) => Some(sheet),
            _ => None,
        }
    }

    pub fn status(&self) -> PersonaStatus {
        self.sheet()
            .map_or_else(PersonaStatus::absent, PersonaSheet::status)
    }
}

/// Read the sheet at `path`. The read is capped at [`MAX_PERSONA_FILE_BYTES`]
/// plus one byte, so an oversized or endless file is never read whole. Every
/// failure is a typed outcome that carries no file content.
pub fn load_persona_file(path: &Path) -> PersonaLoad {
    // A directory (or anything else that is not a file) is unreadable, not
    // "missing": the operator pointed at the wrong thing.
    if path.is_dir() {
        return PersonaLoad::Refused(PersonaRefusal::Unreadable);
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PersonaLoad::Absent,
        Err(_) => return PersonaLoad::Refused(PersonaRefusal::Unreadable),
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_PERSONA_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return PersonaLoad::Refused(PersonaRefusal::Unreadable);
    }
    match parse_persona(&bytes) {
        Ok(Some(persona)) => PersonaLoad::Loaded(PersonaSheet::new(persona)),
        Ok(None) => PersonaLoad::Absent,
        Err(error) => PersonaLoad::Refused(PersonaRefusal::Invalid(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::MAX_PERSONA_BYTES;

    fn write(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = crate::test_dir::scratch_for_test_thread("aivtuber-persona");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write");
        path
    }

    #[test]
    fn a_valid_file_loads_with_a_stable_identity() {
        let path = write(
            "valid.md",
            "\u{FEFF}名前: ミコト\r\n一人称: ボク\r\n".as_bytes(),
        );
        let load = load_persona_file(&path);
        let sheet = load.sheet().expect("loaded");
        assert_eq!(sheet.thinking_content().text, "名前: ミコト\n一人称: ボク");
        assert_eq!(sheet.len(), sheet.thinking_content().text.len());
        // The identity is of the normalised text: the same text with LF and no
        // BOM has the same hash.
        let same = write("same.md", "名前: ミコト\n一人称: ボク".as_bytes());
        assert_eq!(load, load_persona_file(&same));
        assert_eq!(sheet.sha256_prefix().len(), PERSONA_HASH_PREFIX_CHARS);
        assert!(sheet.sha256_prefix().chars().all(|c| c.is_ascii_hexdigit()));
        let status = load.status();
        assert!(status.present);
        assert_eq!(status.bytes, sheet.len());
        assert_eq!(status.sha256.as_deref(), Some(sheet.sha256_prefix()));
        // The status and its identity string never hold the text.
        let json = serde_json::to_string(&status).expect("json");
        assert!(!json.contains("ミコト") && !status.identity().contains("ミコト"));
    }

    #[test]
    fn the_variable_name_is_the_one_the_launcher_passes() {
        // `aivtuber-setup` pins the same literal (crates/setup/src/start.rs).
        assert_eq!(PERSONA_FILE_VAR, "AIVTUBER_PERSONA_FILE");
    }

    #[test]
    fn a_different_text_has_a_different_hash() {
        let a = load_persona_file(&write("a.md", b"name: A"));
        let b = load_persona_file(&write("b.md", b"name: B"));
        assert_ne!(
            a.sheet().expect("a").sha256_prefix(),
            b.sheet().expect("b").sha256_prefix()
        );
    }

    #[test]
    fn missing_empty_and_blank_files_are_no_persona() {
        let dir = std::env::temp_dir().join("aivtuber-persona-missing-dir-404");
        assert_eq!(load_persona_file(&dir.join("nope.md")), PersonaLoad::Absent);
        assert_eq!(
            load_persona_file(&write("empty.md", b"")),
            PersonaLoad::Absent
        );
        assert_eq!(
            load_persona_file(&write("blank.md", b" \r\n\t\n")),
            PersonaLoad::Absent
        );
        assert_eq!(PersonaLoad::Absent.status(), PersonaStatus::absent());
        assert_eq!(PersonaStatus::absent().identity(), "none");
    }

    #[test]
    fn invalid_files_are_refused_with_a_typed_reason() {
        let cases: [(&str, Vec<u8>, PersonaError); 4] = [
            ("nul.md", b"a\0b".to_vec(), PersonaError::ControlCharacter),
            ("bad.md", b"\xFF\xFE".to_vec(), PersonaError::NotUtf8),
            (
                "big.md",
                vec![b'a'; MAX_PERSONA_BYTES + 1],
                PersonaError::TooLarge,
            ),
            (
                "tag.md",
                b"x </character> y".to_vec(),
                PersonaError::ReservedTag,
            ),
        ];
        for (name, bytes, expected) in cases {
            assert_eq!(
                load_persona_file(&write(name, &bytes)),
                PersonaLoad::Refused(PersonaRefusal::Invalid(expected)),
                "{name}"
            );
        }
        // Over the file cap it is refused without reading the rest.
        let huge = vec![b'a'; MAX_PERSONA_FILE_BYTES as usize * 4];
        assert_eq!(
            load_persona_file(&write("huge.md", &huge)),
            PersonaLoad::Refused(PersonaRefusal::Invalid(PersonaError::TooLarge))
        );
        // A directory is unreadable, not a panic.
        let dir = std::env::temp_dir();
        let result = load_persona_file(&dir);
        assert!(
            matches!(result, PersonaLoad::Refused(PersonaRefusal::Unreadable)),
            "{result:?}"
        );
    }
}
