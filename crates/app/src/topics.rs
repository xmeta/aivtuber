//! The operator's chat-topic list on disk and in status (#87 S2, 「雑談ネタ」).
//!
//! The list itself is validated and shaped by [`aivtuber_domain::parse_topics`];
//! this module owns reading the file at start (bounded), the editor settings
//! (where a save is written, whether Thinking leaves the machine) and the
//! text-free status. The director in [`crate::initiative`] holds the live list
//! and rotates through it; nothing here can mint a capability or a permit.
//!
//! The operator replaces the list live through the same closed capability as
//! the character sheet (`persona.configure`, actions `topics.set` and
//! `topics.get`): both are operator-written text the Thinking request carries
//! as `operator_curated` data. See [`crate::ProductionApp::configure_topics`].

use aivtuber_domain::{MAX_TOPICS_FILE_BYTES, TopicList, TopicsError, parse_topics};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const TOPICS_FILE_VAR: &str = "AIVTUBER_TOPICS_FILE";

/// Why a file that exists could not become a topic list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicsRefusal {
    Invalid(TopicsError),
    /// The file exists but could not be read (a directory, permissions).
    Unreadable,
}

/// What reading `AIVTUBER_TOPICS_FILE` produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopicsLoad {
    Loaded(TopicList),
    /// The file is missing or holds no topic: an empty list.
    Absent,
    Refused(TopicsRefusal),
}

impl TopicsLoad {
    /// The list to start with: a refused file is an empty list too.
    pub fn list(&self) -> TopicList {
        match self {
            Self::Loaded(list) => list.clone(),
            _ => TopicList::default(),
        }
    }

    /// A fixed, text-free line for the start-up report.
    pub fn line(&self) -> String {
        match self {
            Self::Loaded(list) => format!("topics: {} loaded", list.len()),
            Self::Absent => "topics: none (the list is empty)".to_owned(),
            Self::Refused(TopicsRefusal::Invalid(error)) => {
                format!("topics: not used, the file {}", error.reason())
            }
            Self::Refused(TopicsRefusal::Unreadable) => {
                "topics: not used, the file could not be read".to_owned()
            }
        }
    }
}

/// Read the list at `path`. The read is capped at [`MAX_TOPICS_FILE_BYTES`]
/// plus one byte, so an oversized or endless file is never read whole. Every
/// failure is a typed outcome that carries no file content.
pub fn load_topics_file(path: &Path) -> TopicsLoad {
    if path.is_dir() {
        return TopicsLoad::Refused(TopicsRefusal::Unreadable);
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return TopicsLoad::Absent,
        Err(_) => return TopicsLoad::Refused(TopicsRefusal::Unreadable),
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_TOPICS_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return TopicsLoad::Refused(TopicsRefusal::Unreadable);
    }
    match parse_topics(&bytes) {
        Ok(list) if list.is_empty() => TopicsLoad::Absent,
        Ok(list) => TopicsLoad::Loaded(list),
        Err(error) => TopicsLoad::Refused(TopicsRefusal::Invalid(error)),
    }
}

/// Live editing: where a saved list is written and whether Thinking is remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicsEditor {
    path: Option<PathBuf>,
    thinking_remote: bool,
}

impl TopicsEditor {
    /// `path` is `AIVTUBER_TOPICS_FILE`; `None` applies in memory only.
    pub fn new(path: Option<PathBuf>, thinking_remote: bool) -> Self {
        Self {
            path,
            thinking_remote,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn thinking_remote(&self) -> bool {
        self.thinking_remote
    }
}

/// The `topics` object of the initiative status: counts and the two facts the
/// panel needs to be honest (does a save reach the file; does a topic leave
/// the machine with an initiative). Never the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicsStatus {
    /// Topics in the live list.
    pub count: usize,
    /// A save is written to `AIVTUBER_TOPICS_FILE` (and so survives a
    /// restart); false = in memory only.
    pub persisted: bool,
    /// The Thinking endpoint leaves this machine.
    pub thinking_remote: bool,
    /// Initiatives that talked about one of the topics.
    pub used: u64,
    /// Initiatives that ran with no topic because the list was empty.
    pub without_topic: u64,
}

/// What a `topics.*` operator action produced. `text` is set only by
/// `topics.get` and goes to the authenticated caller, never to a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicsOutcome {
    pub detail: String,
    pub text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aivtuber-topics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write");
        path
    }

    #[test]
    fn a_valid_file_loads_normalised_and_a_status_never_holds_text() {
        let load = load_topics_file(&write(
            "valid.txt",
            "\u{FEFF}好きな食べ物\r\n\r\n最近のゲーム\r\n".as_bytes(),
        ));
        let TopicsLoad::Loaded(list) = &load else {
            panic!("expected loaded, got {load:?}");
        };
        assert_eq!(list.entries(), ["好きな食べ物", "最近のゲーム"]);
        assert_eq!(load.line(), "topics: 2 loaded");
        let status = TopicsStatus {
            count: list.len(),
            persisted: true,
            thinking_remote: false,
            used: 0,
            without_topic: 0,
        };
        assert!(!serde_json::to_string(&status).unwrap().contains("好き"));
    }

    #[test]
    fn missing_empty_and_refused_files_start_an_empty_list() {
        let dir = std::env::temp_dir().join("aivtuber-topics-missing-dir-87");
        assert_eq!(load_topics_file(&dir.join("nope.txt")), TopicsLoad::Absent);
        assert_eq!(
            load_topics_file(&write("empty.txt", b" \n\r\n")),
            TopicsLoad::Absent
        );
        let bad = load_topics_file(&write("bad.txt", b"a\0b"));
        assert_eq!(
            bad,
            TopicsLoad::Refused(TopicsRefusal::Invalid(TopicsError::ControlCharacter))
        );
        assert!(bad.list().is_empty());
        assert!(!bad.line().contains('\0'));
        let huge = vec![b'a'; MAX_TOPICS_FILE_BYTES as usize * 4];
        assert_eq!(
            load_topics_file(&write("huge.txt", &huge)),
            TopicsLoad::Refused(TopicsRefusal::Invalid(TopicsError::TooLarge))
        );
        assert_eq!(
            load_topics_file(&std::env::temp_dir()),
            TopicsLoad::Refused(TopicsRefusal::Unreadable)
        );
    }

    #[test]
    fn the_variable_name_is_the_one_the_launcher_passes() {
        // `aivtuber-setup` pins the same literal (crates/setup/src/start.rs).
        assert_eq!(TOPICS_FILE_VAR, "AIVTUBER_TOPICS_FILE");
    }
}
