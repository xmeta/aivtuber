//! Opt-in local session conversation log (issue #463).
//!
//! `docs/runtime-retention.adoc` keeps "no hidden transcript": nothing the
//! performer hears or says is retained behind the operator's back. This module
//! is the explicit exception the operator can choose: a JSON Lines file per app
//! session under `logs/sessions/`, written only while the operator has the
//! switch on, so a live run can be measured afterwards (`aivtuberctl log
//! summary`), for example for the repetition the #456 runs could not count.
//!
//! # What a line holds
//!
//! One record per line (see [`Record`]): `session` (header), `config` (hashes
//! of the character sheet and memory seed, never their text), `input` (a chat
//! line or transcript, or an idle initiative), `phrase` (one committed phrase
//! as spoken, with the repeat verdict of #89), `route` (how the turn was
//! answered, and any fallback), `fallback` (a turn answered by a typed fallback
//! instead of a reply, with its reason and whether the viewer saw it) and `note` (a fixed vocabulary of events such as
//! a refused initiative repeat). Records of one turn share a `turn` id
//! (`t-0001`), assigned here: raw event ids are never written.
//!
//! # Privacy and bounds
//!
//! * Off by default. While off nothing is formatted, queued or written, and no
//!   directory or file is created.
//! * Viewer ids are replaced by per-session pseudonyms (`viewer-01`, ...)
//!   from a bounded in-memory map that is never persisted; a raw id that
//!   appears inside text is replaced the same way. Secrets known to the
//!   redactor and URLs are removed, and every text field is cut at
//!   [`FIELD_MAX_CHARS`]. No audio, key, token, character sheet or memory seed
//!   text is ever passed in.
//! * A file stops at [`FILE_MAX_BYTES`] (a final `capped` record says so),
//!   the newest [`RETAIN_SESSIONS`] files are kept, and the panel can delete
//!   them all.
//! * Ordering uses `seq` and the loop's monotonic `logical_ms`; `wall` is a
//!   UTC display time and is never read back for any decision.
//! * Writing is on a dedicated thread behind a bounded queue. The app thread
//!   only formats a line and `try_send`s it; a full queue, a full file or an
//!   I/O error is counted in [`SessionLogStatus`] and never blocks or fails a
//!   reply or an operator command.
//!
//! Viewer text in the file is third-party data and the operator's
//! responsibility. The log is never in a diagnose bundle and is never sent
//! anywhere.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aivtuber_scheduler::PhraseLink;
use aivtuber_telemetry::SecretRedactor;
use serde::{Deserialize, Serialize};

/// Directory of the session files (set by `aivtuber-setup start`).
pub const SESSION_LOG_DIR_VAR: &str = "AIVTUBER_SESSION_LOG_DIR";
/// The remembered on/off choice (set by `aivtuber-setup start`).
pub const SESSION_LOG_SETTINGS_VAR: &str = "AIVTUBER_SESSION_LOG_SETTINGS";
pub const SESSION_LOG_SCHEMA: &str = "aivtuber-session-log/1";
pub const SESSION_LOG_SETTINGS_SCHEMA: &str = "local-session-log-settings/1";
pub const MAX_SESSION_LOG_SETTINGS_BYTES: u64 = 256;
/// Characters kept per text field; longer text is cut and marked.
pub const FIELD_MAX_CHARS: usize = 400;
/// A file stops growing at this size (5 MiB).
pub const FILE_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Session files kept, the newest ones.
pub const RETAIN_SESSIONS: usize = 20;
/// Lines waiting for the writer thread.
pub const QUEUE_CAPACITY: usize = 512;
/// Viewer pseudonyms remembered per session.
pub const MAX_VIEWERS: usize = 256;
/// Turn ids remembered (so a reply finds its turn).
pub const MAX_TURNS: usize = 256;
/// Longest line the writer accepts; fields are bounded far below this.
pub const MAX_LINE_BYTES: usize = 8 * 1024;
/// Room kept at the end of a file for the `capped` record.
const CAP_RESERVE: u64 = 256;
const FILE_PREFIX: &str = "session-";
const FILE_SUFFIX: &str = ".jsonl";

/// One line of a session file. Writers set only the fields their record
/// type uses; readers ignore unknown ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub v: u32,
    /// `session`, `config`, `input`, `phrase`, `route`, `fallback`, `note` or `capped`.
    pub t: String,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    /// Monotonic loop time in milliseconds; the only ordering clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_ms: Option<u64>,
    /// UTC wall time for display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall: Option<String>,
    /// `chat`, `voice`, `initiative`, `cached` or `operator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idx: Option<u32>,
    /// `generated` or `cached` for a phrase; the route class for `route`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    /// `none`, `exact` or `near` (the #89 verdict).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `crowd` records (#79): members the crowd reaction stands for, and the
    /// distinct viewers among them. Counts only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actors: Option<u32>,
    /// `initiative_repeat_suppressed` notes (#547): the origin of the phrase
    /// the refusal matched, its logical age, and whether the request showed
    /// the model that phrase. No text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_age_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_shown: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_seed_sha256: Option<String>,
}

/// Operator-visible state. Counters and the location; never any logged text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLogStatus {
    pub enabled: bool,
    /// The directory the files are written to. Cleared in the diagnose bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub max_file_bytes: u64,
    pub retain_sessions: usize,
    pub files_kept: u64,
    pub lines_written: u64,
    pub bytes_written: u64,
    pub dropped_queue_full: u64,
    pub dropped_file_full: u64,
    pub write_errors: u64,
    /// The current file reached its size bound and stopped.
    pub capped: bool,
    /// The last on/off choice could not be saved for the next launch.
    pub settings_save_failed: bool,
}

#[derive(Default)]
struct Shared {
    lines_written: AtomicU64,
    bytes_written: AtomicU64,
    dropped_queue_full: AtomicU64,
    dropped_file_full: AtomicU64,
    write_errors: AtomicU64,
    files_kept: AtomicU64,
    capped: AtomicBool,
}

enum Msg {
    Line(String),
    /// Header written first whenever a file is created.
    Header(String),
    DeleteAll,
    /// Close the current file (the switch went off).
    Close,
    Flush(SyncSender<()>),
    Stop,
}

/// The app-side handle: formats records and hands lines to the writer thread.
pub struct SessionLog {
    dir: PathBuf,
    settings_path: Option<PathBuf>,
    enabled: bool,
    settings_save_failed: bool,
    tx: SyncSender<Msg>,
    shared: Arc<Shared>,
    session_id: String,
    seq: u64,
    viewers: Vec<(String, String)>,
    turns: VecDeque<TurnEntry>,
    next_turn: u64,
    last_hashes: Option<(Option<String>, Option<String>)>,
}

struct TurnEntry {
    root: String,
    id: String,
    origin: &'static str,
    phrases: u32,
}

impl std::fmt::Debug for SessionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLog")
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        let _ = self.tx.try_send(Msg::Stop);
    }
}

impl SessionLog {
    /// A log writing under `dir`, remembering its switch in `settings_path`,
    /// starting `enabled` as given. Spawns the writer thread; touches no
    /// file until the first record is written.
    pub fn new(dir: PathBuf, settings_path: Option<PathBuf>, enabled: bool) -> Self {
        let (tx, rx) = sync_channel(QUEUE_CAPACITY);
        let shared = Arc::new(Shared::default());
        let session_id = random_hex(4);
        let writer = Writer {
            dir: dir.clone(),
            file_name: format!("{FILE_PREFIX}{}-{session_id}{FILE_SUFFIX}", wall_compact()),
            header: String::new(),
            file: None,
            written: 0,
            shared: Arc::clone(&shared),
        };
        // A failed spawn leaves a log that counts every line as dropped.
        let _ = std::thread::Builder::new()
            .name("session-log".to_owned())
            .spawn(move || writer.run(rx));
        let log = Self {
            dir,
            settings_path,
            enabled,
            settings_save_failed: false,
            tx,
            shared,
            session_id,
            seq: 0,
            viewers: Vec::new(),
            turns: VecDeque::new(),
            next_turn: 0,
            last_hashes: None,
        };
        log.send_header();
        log
    }

    /// The remembered choice (off when the file is missing, damaged, too big
    /// or from another schema), then a log started in that state.
    pub fn from_settings(dir: PathBuf, settings_path: PathBuf) -> Self {
        let enabled = read_enabled(&settings_path);
        Self::new(dir, Some(settings_path), enabled)
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Switch recording on or off and remember the choice. The switch takes
    /// effect even when the choice cannot be saved (`Err` says so).
    pub fn set_enabled(&mut self, on: bool) -> Result<(), String> {
        self.enabled = on;
        if on {
            // A fresh header and a fresh hash record for this stretch.
            self.last_hashes = None;
            self.send_header();
        } else {
            let _ = self.tx.try_send(Msg::Close);
        }
        let saved = match &self.settings_path {
            Some(path) => write_enabled(path, on),
            None => Ok(()),
        };
        self.settings_save_failed = saved.is_err();
        saved.map_err(|_| "the choice could not be saved for the next launch".to_owned())
    }

    /// Ask the writer to delete every session file, the current one included.
    /// Returns false when the request could not be queued.
    pub fn delete_all(&self) -> bool {
        self.tx.try_send(Msg::DeleteAll).is_ok()
    }

    /// Wait (bounded) until everything queued so far has been written.
    pub fn flush(&self) {
        let (ack_tx, ack_rx) = sync_channel(1);
        if self.tx.send(Msg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(5));
        }
    }

    pub fn status(&self) -> SessionLogStatus {
        let s = &self.shared;
        SessionLogStatus {
            enabled: self.enabled,
            location: Some(self.dir.display().to_string()),
            max_file_bytes: FILE_MAX_BYTES,
            retain_sessions: RETAIN_SESSIONS,
            files_kept: s.files_kept.load(Ordering::Relaxed),
            lines_written: s.lines_written.load(Ordering::Relaxed),
            bytes_written: s.bytes_written.load(Ordering::Relaxed),
            dropped_queue_full: s.dropped_queue_full.load(Ordering::Relaxed),
            dropped_file_full: s.dropped_file_full.load(Ordering::Relaxed),
            write_errors: s.write_errors.load(Ordering::Relaxed),
            capped: s.capped.load(Ordering::Relaxed),
            settings_save_failed: self.settings_save_failed,
        }
    }

    fn send_header(&self) {
        let record = Record {
            v: 1,
            t: "session".to_owned(),
            seq: 0,
            wall: Some(wall_iso()),
            session: Some(self.session_id.clone()),
            schema: Some(SESSION_LOG_SCHEMA.to_owned()),
            ..Record::default()
        };
        if let Ok(line) = serde_json::to_string(&record) {
            let _ = self.tx.try_send(Msg::Header(line));
        }
    }

    fn emit(&mut self, mut record: Record) {
        self.seq = self.seq.saturating_add(1);
        record.v = 1;
        record.seq = self.seq;
        record.wall = Some(wall_iso());
        let Ok(line) = serde_json::to_string(&record) else {
            return;
        };
        match self.tx.try_send(Msg::Line(line)) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.shared
                    .dropped_queue_full
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn pseudonym(&mut self, actor: &str) -> String {
        if let Some((_, name)) = self.viewers.iter().find(|(raw, _)| raw == actor) {
            return name.clone();
        }
        if self.viewers.len() >= MAX_VIEWERS {
            // Past the bound, ids share one label rather than grow the map.
            return "viewer-other".to_owned();
        }
        let name = format!("viewer-{:02}", self.viewers.len() + 1);
        self.viewers.push((actor.to_owned(), name.clone()));
        name
    }

    /// Remove what must not be written, then bound the text.
    fn clean(&self, redactor: &SecretRedactor, text: &str) -> String {
        let mut out = redactor.redact(text);
        for (raw, name) in &self.viewers {
            if raw.chars().count() >= 3 {
                out = out.replace(raw.as_str(), name);
            }
        }
        cut_chars(&scrub_urls(&out), FIELD_MAX_CHARS)
    }

    fn turn_index(&mut self, event_id: &str, origin: &'static str) -> usize {
        let root = PhraseLink::root_event_id(event_id);
        if let Some(index) = self.turns.iter().position(|turn| turn.root == root) {
            return index;
        }
        self.next_turn = self.next_turn.saturating_add(1);
        if self.turns.len() >= MAX_TURNS {
            self.turns.pop_front();
        }
        self.turns.push_back(TurnEntry {
            root: root.to_owned(),
            id: format!("t-{:04}", self.next_turn),
            origin,
            phrases: 0,
        });
        self.turns.len() - 1
    }

    /// The hashes of the character sheet and memory seed, written when they
    /// differ from the last record.
    pub fn note_hashes(&mut self, persona: Option<&str>, seed: Option<&str>) {
        if !self.enabled {
            return;
        }
        let now = (persona.map(str::to_owned), seed.map(str::to_owned));
        if self.last_hashes.as_ref() == Some(&now) {
            return;
        }
        self.emit(Record {
            t: "config".to_owned(),
            persona_sha256: now.0.clone(),
            memory_seed_sha256: now.1.clone(),
            ..Record::default()
        });
        self.last_hashes = Some(now);
    }

    /// A chat line, a transcript or an initiative that opens a turn.
    #[allow(clippy::too_many_arguments)]
    pub fn input(
        &mut self,
        redactor: &SecretRedactor,
        event_id: &str,
        origin: &'static str,
        actor: Option<&str>,
        text: Option<&str>,
        kind: Option<&str>,
        logical_ms: u64,
    ) {
        if !self.enabled {
            return;
        }
        let viewer = actor.map(|actor| self.pseudonym(actor));
        let text = text.map(|text| self.clean(redactor, text));
        let index = self.turn_index(event_id, origin);
        let turn = self.turns[index].id.clone();
        self.emit(Record {
            t: "input".to_owned(),
            turn: Some(turn),
            logical_ms: Some(logical_ms),
            origin: Some(origin.to_owned()),
            viewer,
            text,
            kind: kind.map(|kind| cut_chars(kind, 32)),
            ..Record::default()
        });
    }

    /// One committed phrase as spoken.
    pub fn phrase(
        &mut self,
        redactor: &SecretRedactor,
        event_id: &str,
        generated: bool,
        text: &str,
        repeat: &'static str,
        logical_ms: u64,
    ) {
        if !self.enabled {
            return;
        }
        let default_origin = if event_id.starts_with(crate::initiative::EVENT_ID_PREFIX) {
            "initiative"
        } else if event_id.starts_with("panel-chat-") {
            "operator"
        } else {
            "chat"
        };
        let index = self.turn_index(event_id, default_origin);
        let text = self.clean(redactor, text);
        let entry = &mut self.turns[index];
        let idx = entry.phrases;
        entry.phrases = entry.phrases.saturating_add(1);
        let turn = entry.id.clone();
        let origin = if generated { entry.origin } else { "cached" };
        self.emit(Record {
            t: "phrase".to_owned(),
            turn: Some(turn),
            logical_ms: Some(logical_ms),
            origin: Some(origin.to_owned()),
            idx: Some(idx),
            route: Some(if generated { "generated" } else { "cached" }.to_owned()),
            text: Some(text),
            repeat: Some(repeat.to_owned()),
            ..Record::default()
        });
    }

    /// How a turn was answered, and why it fell back when it did.
    pub fn route(
        &mut self,
        event_id: &str,
        route: &'static str,
        cache: Option<&'static str>,
        fallback: Option<&str>,
        logical_ms: u64,
    ) {
        if !self.enabled {
            return;
        }
        let index = self.turn_index(event_id, "chat");
        let turn = self.turns[index].id.clone();
        self.emit(Record {
            t: "route".to_owned(),
            turn: Some(turn),
            logical_ms: Some(logical_ms),
            route: Some(route.to_owned()),
            cache: cache.map(str::to_owned),
            fallback: fallback.map(|reason| cut_chars(reason, 48)),
            ..Record::default()
        });
    }

    /// A turn answered by a fallback instead of a reply (#469): `reason` is the
    /// closed fallback vocabulary, `visible` says whether the viewer was shown
    /// a cached reaction. Never viewer text.
    /// A turn ended on purpose by a newer line is `superseded` (kind `silent`).
    pub fn fallback(&mut self, event_id: &str, reason: &str, visible: bool, logical_ms: u64) {
        if !self.enabled {
            return;
        }
        let index = self.turn_index(event_id, "chat");
        let turn = self.turns[index].id.clone();
        self.emit(Record {
            t: "fallback".to_owned(),
            turn: Some(turn),
            logical_ms: Some(logical_ms),
            kind: Some(if visible { "visible" } else { "silent" }.to_owned()),
            fallback: Some(cut_chars(reason, 48)),
            ..Record::default()
        });
    }

    /// A crowd reaction was routed to the turn of its carrier (#79): `class`
    /// is the closed crowd vocabulary; no viewer text and no actor ids. The
    /// phrases later spoken on this turn are the crowd reaction.
    pub fn crowd(
        &mut self,
        event_id: &str,
        class: &str,
        members: u32,
        actors: u32,
        logical_ms: u64,
    ) {
        if !self.enabled {
            return;
        }
        let index = self.turn_index(event_id, "chat");
        let turn = self.turns[index].id.clone();
        self.emit(Record {
            t: "crowd".to_owned(),
            turn: Some(turn),
            logical_ms: Some(logical_ms),
            kind: Some(cut_chars(class, 32)),
            members: Some(members),
            actors: Some(actors),
            ..Record::default()
        });
    }

    /// A fixed-vocabulary event on a turn (`kind` and `detail` are never
    /// viewer text).
    pub fn note(&mut self, event_id: &str, kind: &'static str, detail: &str, logical_ms: u64) {
        if !self.enabled {
            return;
        }
        let index = self.turn_index(event_id, "chat");
        let turn = self.turns[index].id.clone();
        self.emit(Record {
            t: "note".to_owned(),
            turn: Some(turn),
            logical_ms: Some(logical_ms),
            kind: Some(kind.to_owned()),
            fallback: Some(cut_chars(detail, 48)),
            ..Record::default()
        });
    }
}

impl SessionLog {
    /// An initiative phrase refused as a repeat (#547): `repeat` is `exact` or
    /// `near`; the rest says which earlier phrase it matched. Fixed
    /// vocabulary, a logical age and a flag: never the phrase.
    pub fn note_repeat_refusal(
        &mut self,
        event_id: &str,
        repeat: &str,
        matched_origin: &str,
        matched_age_ms: u64,
        matched_shown: bool,
        logical_ms: u64,
    ) {
        if !self.enabled {
            return;
        }
        let index = self.turn_index(event_id, "chat");
        let turn = self.turns[index].id.clone();
        self.emit(Record {
            t: "note".to_owned(),
            turn: Some(turn),
            logical_ms: Some(logical_ms),
            kind: Some("initiative_repeat_suppressed".to_owned()),
            fallback: Some(cut_chars(repeat, 48)),
            matched_origin: Some(cut_chars(matched_origin, 24)),
            matched_age_ms: Some(matched_age_ms),
            matched_shown: Some(matched_shown),
            ..Record::default()
        });
    }
}

struct Writer {
    dir: PathBuf,
    file_name: String,
    header: String,
    file: Option<File>,
    written: u64,
    shared: Arc<Shared>,
}

impl Writer {
    fn run(mut self, rx: Receiver<Msg>) {
        self.count_files();
        while let Ok(message) = rx.recv() {
            match message {
                Msg::Line(line) => self.append(&line),
                Msg::Header(header) => self.header = header,
                Msg::DeleteAll => self.delete_all(),
                Msg::Close => self.file = None,
                Msg::Flush(ack) => {
                    let _ = ack.try_send(());
                }
                Msg::Stop => break,
            }
        }
    }

    fn error(&self) {
        self.shared.write_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn session_files(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            // `file_type` does not follow links, so a link is never touched.
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter(|entry| {
                entry.file_name().to_str().is_some_and(|name| {
                    name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX)
                })
            })
            .map(|entry| entry.path())
            .collect();
        files.sort();
        files
    }

    fn count_files(&self) {
        self.shared
            .files_kept
            .store(self.session_files().len() as u64, Ordering::Relaxed);
    }

    fn open(&mut self) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(&self.file_name);
        if !path.exists() {
            // Make room first so the new file never makes it 21.
            let files = self.session_files();
            let excess = (files.len() + 1).saturating_sub(RETAIN_SESSIONS);
            for old in files.into_iter().take(excess) {
                if fs::remove_file(old).is_err() {
                    self.error();
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.written = file.metadata()?.len();
        self.file = Some(file);
        if self.written == 0 && !self.header.is_empty() {
            let header = self.header.clone();
            self.write_line(&header)?;
        }
        self.count_files();
        Ok(())
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Err(std::io::Error::other("no open file"));
        };
        let mut buffer = String::with_capacity(line.len() + 1);
        buffer.push_str(line);
        buffer.push('\n');
        file.write_all(buffer.as_bytes())?;
        self.written += buffer.len() as u64;
        self.shared.lines_written.fetch_add(1, Ordering::Relaxed);
        self.shared
            .bytes_written
            .fetch_add(buffer.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn append(&mut self, line: &str) {
        if self.shared.capped.load(Ordering::Relaxed) {
            self.shared
                .dropped_file_full
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        if line.len() > MAX_LINE_BYTES {
            self.error();
            return;
        }
        if self.file.is_none() && self.open().is_err() {
            self.error();
            return;
        }
        if self.written + line.len() as u64 + 1 > FILE_MAX_BYTES - CAP_RESERVE {
            let marker = serde_json::to_string(&Record {
                v: 1,
                t: "capped".to_owned(),
                seq: u64::MAX,
                wall: Some(wall_iso()),
                ..Record::default()
            })
            .unwrap_or_default();
            let _ = self.write_line(&marker);
            self.shared.capped.store(true, Ordering::Relaxed);
            self.shared
                .dropped_file_full
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.write_line(line).is_err() {
            self.error();
            // Reopen (append) on the next line rather than keep a broken handle.
            self.file = None;
        }
    }

    fn delete_all(&mut self) {
        // Close first: Windows cannot delete an open file.
        self.file = None;
        for path in self.session_files() {
            if fs::remove_file(path).is_err() {
                self.error();
            }
        }
        self.written = 0;
        self.shared.capped.store(false, Ordering::Relaxed);
        self.count_files();
    }
}

// ---- remembered choice ----

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    schema: String,
    enabled: bool,
}

/// The remembered choice; anything unexpected reads as off.
pub fn read_enabled(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_SESSION_LOG_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_SESSION_LOG_SETTINGS_BYTES
    {
        return false;
    }
    serde_json::from_slice::<SettingsFile>(&bytes)
        .is_ok_and(|file| file.schema == SESSION_LOG_SETTINGS_SCHEMA && file.enabled)
}

/// Replace the state file (temp file then rename, so a crash leaves the old
/// or the new file, never half of one).
pub fn write_enabled(path: &Path, enabled: bool) -> std::io::Result<()> {
    let text = serde_json::to_string(&SettingsFile {
        schema: SESSION_LOG_SETTINGS_SCHEMA.to_owned(),
        enabled,
    })
    .map_err(std::io::Error::other)?;
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&temp, format!("{text}\n"))?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

// ---- text handling ----

fn cut_chars(text: &str, max: usize) -> String {
    let mut shown: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        shown.push('…');
    }
    shown
}

/// Every `http(s)://` run (up to whitespace or the first non-ASCII
/// character) becomes `[url]`, so no query token or credential survives.
fn scrub_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| rest.to_ascii_lowercase().find(scheme))
        .min()
    {
        out.push_str(&rest[..start]);
        out.push_str("[url]");
        let tail = &rest[start..];
        let end = tail
            .char_indices()
            .find(|(_, c)| c.is_whitespace() || !c.is_ascii())
            .map_or(tail.len(), |(index, _)| index);
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0_u8; bytes];
    if getrandom::fill(&mut buffer).is_err() {
        buffer = vec![0xa5; bytes];
    }
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `2026-11-22T10:00:00Z` for display. The std library has no local offset.
fn wall_iso() -> String {
    let (y, mo, d, h, mi, s) = civil(SystemTime::now());
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn wall_compact() -> String {
    let (y, mo, d, h, mi, s) = civil(SystemTime::now());
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

fn civil(time: SystemTime) -> (i64, u32, u32, u32, u32, u32) {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs()) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3_600) as u32,
        (rem % 3_600 / 60) as u32,
        (rem % 60) as u32,
    )
}

// ---- offline summary ----

/// Lines longer than this are skipped and counted as malformed.
pub const SUMMARY_MAX_LINE_BYTES: usize = 16 * 1024;
/// A summary reads at most this many lines.
pub const SUMMARY_MAX_LINES: usize = 200_000;
/// Total bytes examined, including discarded malformed records. Above the
/// 5 MiB writer cap, but bounded even for an imported or damaged file.
pub const SUMMARY_MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OriginRepeat {
    pub origin: String,
    pub phrases: u64,
    pub exact: u64,
    pub near: u64,
}

/// One routed crowd reaction (#79) and what was spoken for its carrier.
/// Counts and indexes only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrowdRouteSummary {
    pub class: String,
    pub members: u32,
    pub actors: u32,
    pub turn: String,
    /// Phrases spoken on the carrier's turn, and how many were cached.
    pub phrases: u32,
    pub cached_phrases: u32,
    pub first_audio_ms: Option<u64>,
    /// Separate logical-time intervals reconstructed from existing log records.
    pub timing: CrowdTiming,
}

/// Only intervals with observed, unambiguous, ordered endpoints are reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CrowdTiming {
    pub input_to_route_ms: Option<u64>,
    pub route_to_dispatch_ms: Option<u64>,
    pub dispatch_to_phrase_ms: Option<u64>,
    pub route_to_phrase_ms: Option<u64>,
}

#[derive(Debug, Default)]
enum TimingStamp {
    #[default]
    Unseen,
    Observed(Option<u64>),
    Ambiguous,
}

impl TimingStamp {
    fn observe(&mut self, ms: Option<u64>) {
        *self = match self {
            Self::Unseen => Self::Observed(ms),
            _ => Self::Ambiguous,
        };
    }

    fn value(&self) -> Option<u64> {
        match self {
            Self::Observed(ms) => *ms,
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
struct TimingPoints {
    input: TimingStamp,
    route: TimingStamp,
    dispatch: TimingStamp,
    phrase: TimingStamp,
}

impl TimingPoints {
    fn intervals(&self) -> CrowdTiming {
        let stamps = [&self.input, &self.route, &self.dispatch, &self.phrase];
        if stamps
            .iter()
            .any(|stamp| matches!(stamp, TimingStamp::Ambiguous))
        {
            return CrowdTiming::default();
        }
        let [input, route, dispatch, phrase] = stamps.map(TimingStamp::value);
        // A log is ordered by writes, not necessarily by logical time. Do not
        // turn an inconsistent timestamp into a zero-duration observation.
        let known: Vec<_> = [input, route, dispatch, phrase]
            .into_iter()
            .flatten()
            .collect();
        if known.windows(2).any(|pair| pair[0] > pair[1]) {
            return CrowdTiming::default();
        }
        let delta = |start: Option<u64>, end: Option<u64>| end?.checked_sub(start?);
        CrowdTiming {
            input_to_route_ms: delta(input, route),
            route_to_dispatch_ms: delta(route, dispatch),
            dispatch_to_phrase_ms: delta(dispatch, phrase),
            route_to_phrase_ms: delta(route, phrase),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSummary {
    pub lines: u64,
    pub malformed: u64,
    pub truncated_read: bool,
    pub capped: bool,
    /// `(origin, turns)` from `input` records.
    pub turns: Vec<(String, u64)>,
    pub phrases: u64,
    pub repeats: Vec<OriginRepeat>,
    /// Logical milliseconds from a chat, voice or operator input to the first
    /// phrase of the same turn.
    pub first_audio_ms: Vec<u64>,
    /// The same measurement split by how the reply was dispatched (#79 S3c):
    /// a selected comment answered at once, one that waited in the deferred
    /// inbox, and a crowd reaction. Initiative and cached turns without a
    /// recorded path count as direct.
    pub first_audio_direct_ms: Vec<u64>,
    pub first_audio_deferred_ms: Vec<u64>,
    pub first_audio_crowd_ms: Vec<u64>,
    pub crowd_routes: Vec<CrowdRouteSummary>,
    /// Logical milliseconds between consecutive initiative turns.
    pub initiative_spacing_ms: Vec<u64>,
    pub fallbacks: u64,
    /// Initiative repeat refusals (#547), counted by the matched phrase's
    /// origin and by whether the model was shown it. A note from before the
    /// fields existed counts as `unknown`.
    pub repeat_refusals: u64,
    pub repeat_refusals_by_origin: Vec<(String, u64)>,
    pub repeat_refusals_by_shown: Vec<(String, u64)>,
    /// Sorted logical ages of the matched phrases.
    pub repeat_refusal_age_ms: Vec<u64>,
}

fn bump(list: &mut Vec<(String, u64)>, key: &str) {
    match list.iter_mut().find(|(name, _)| name == key) {
        Some((_, count)) => *count += 1,
        None => list.push((key.to_owned(), 1)),
    }
}

/// Nearest-rank percentile of `sorted` (ascending), `None` when empty.
pub fn percentile(sorted: &[u64], percent: u64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (percent * sorted.len() as u64).div_ceil(100).max(1) as usize;
    sorted.get(rank - 1).copied()
}

/// Read one session file. Bounded in line length, line count and total bytes;
/// a damaged line is counted. Read failures mark the evidence incomplete.
pub fn summarize(reader: impl BufRead) -> SessionSummary {
    let mut summary = SessionSummary::default();
    let mut input_ms: HashMap<String, u64> = HashMap::new();
    let mut answered: HashSet<String> = HashSet::new();
    let mut fallback_turns: HashSet<String> = HashSet::new();
    let mut last_initiative: Option<u64> = None;
    let mut deferred_turns: HashSet<String> = HashSet::new();
    let mut first_audio_by_turn: Vec<(String, u64)> = Vec::new();
    let mut turn_phrases: HashMap<String, (u32, u32)> = HashMap::new();
    // Offline only, bounded by SUMMARY_MAX_LINES like the existing turn maps.
    let mut timing_points: HashMap<String, TimingPoints> = HashMap::new();
    let mut reader = reader.take(SUMMARY_MAX_BYTES);
    loop {
        if summary.lines >= SUMMARY_MAX_LINES as u64 || reader.limit() == 0 {
            summary.truncated_read = true;
            break;
        }
        let mut buffer = Vec::new();
        let read = (&mut reader)
            .take(SUMMARY_MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut buffer);
        match read {
            Ok(0) => break,
            Err(_) => {
                summary.truncated_read = true;
                break;
            }
            Ok(_) => {}
        }
        summary.lines += 1;
        if buffer.len() > SUMMARY_MAX_LINE_BYTES {
            summary.malformed += 1;
            // Drain without allocating the tail. Take also bounds this work.
            if !buffer.ends_with(b"\n") && reader.skip_until(b'\n').is_err() {
                summary.truncated_read = true;
                break;
            }
            continue;
        }
        // A byte cap can cut through an otherwise valid JSON prefix. Do
        // not count a partial final record as an observed event.
        if reader.limit() == 0 && !buffer.ends_with(b"\n") {
            summary.truncated_read = true;
            break;
        }
        let Ok(record) = serde_json::from_slice::<Record>(&buffer) else {
            summary.malformed += 1;
            continue;
        };
        // The timestamp of the first recorded phrase is only usable when its
        // index is zero. A missing first phrase is not a faster later phrase.
        if let Some(turn) = record.turn.as_ref() {
            let ms = record.logical_ms;
            match record.t.as_str() {
                "input" => timing_points
                    .entry(turn.clone())
                    .or_default()
                    .input
                    .observe(ms),
                "crowd" => timing_points
                    .entry(turn.clone())
                    .or_default()
                    .route
                    .observe(ms),
                "note"
                    if record.kind.as_deref() == Some("path")
                        && matches!(record.fallback.as_deref(), Some("direct" | "deferred")) =>
                {
                    timing_points
                        .entry(turn.clone())
                        .or_default()
                        .dispatch
                        .observe(ms);
                }
                "phrase" => {
                    let stamp = &mut timing_points.entry(turn.clone()).or_default().phrase;
                    if matches!(stamp, TimingStamp::Unseen) || record.idx == Some(0) {
                        stamp.observe(if record.idx == Some(0) { ms } else { None });
                    }
                }
                _ => {}
            }
        }
        match record.t.as_str() {
            "input" => {
                let origin = record.origin.clone().unwrap_or_else(|| "chat".to_owned());
                bump(&mut summary.turns, &origin);
                if let (Some(turn), Some(ms)) = (record.turn.clone(), record.logical_ms) {
                    if origin == "initiative" {
                        if let Some(previous) = last_initiative {
                            summary
                                .initiative_spacing_ms
                                .push(ms.saturating_sub(previous));
                        }
                        last_initiative = Some(ms);
                    } else {
                        input_ms.insert(turn, ms);
                    }
                }
            }
            "phrase" => {
                summary.phrases += 1;
                let origin = record.origin.clone().unwrap_or_else(|| "chat".to_owned());
                let slot = match summary.repeats.iter().position(|r| r.origin == origin) {
                    Some(index) => index,
                    None => {
                        summary.repeats.push(OriginRepeat {
                            origin,
                            ..OriginRepeat::default()
                        });
                        summary.repeats.len() - 1
                    }
                };
                let entry = &mut summary.repeats[slot];
                entry.phrases += 1;
                match record.repeat.as_deref() {
                    Some("exact") => entry.exact += 1,
                    Some("near") => entry.near += 1,
                    _ => {}
                }
                if let Some(turn) = record.turn.as_ref() {
                    let counts = turn_phrases.entry(turn.clone()).or_default();
                    counts.0 += 1;
                    if record.route.as_deref() == Some("cached") {
                        counts.1 += 1;
                    }
                }
                if let (Some(turn), Some(ms)) = (record.turn.as_ref(), record.logical_ms)
                    && answered.insert(turn.clone())
                    && let Some(started) = input_ms.get(turn)
                {
                    let took = ms.saturating_sub(*started);
                    summary.first_audio_ms.push(took);
                    first_audio_by_turn.push((turn.clone(), took));
                }
            }
            // A turn counts once however many records name its fallback.
            "route" | "fallback" if record.fallback.is_some() => match record.turn {
                Some(turn) => {
                    if fallback_turns.insert(turn) {
                        summary.fallbacks += 1;
                    }
                }
                None => summary.fallbacks += 1,
            },
            "crowd" => {
                if let Some(turn) = record.turn.clone() {
                    summary.crowd_routes.push(CrowdRouteSummary {
                        class: record.kind.clone().unwrap_or_default(),
                        members: record.members.unwrap_or(0),
                        actors: record.actors.unwrap_or(0),
                        turn,
                        ..CrowdRouteSummary::default()
                    });
                }
            }
            "note" if record.kind.as_deref() == Some("initiative_repeat_suppressed") => {
                summary.repeat_refusals += 1;
                bump(
                    &mut summary.repeat_refusals_by_origin,
                    record.matched_origin.as_deref().unwrap_or("unknown"),
                );
                bump(
                    &mut summary.repeat_refusals_by_shown,
                    match record.matched_shown {
                        Some(true) => "true",
                        Some(false) => "false",
                        None => "unknown",
                    },
                );
                if let Some(age) = record.matched_age_ms {
                    summary.repeat_refusal_age_ms.push(age);
                }
            }
            "note" if record.kind.as_deref() == Some("path") => {
                if record.fallback.as_deref() == Some("deferred")
                    && let Some(turn) = record.turn.clone()
                {
                    deferred_turns.insert(turn);
                }
            }
            "capped" => summary.capped = true,
            _ => {}
        }
    }
    let crowd_turns: HashSet<String> = summary
        .crowd_routes
        .iter()
        .map(|route| route.turn.clone())
        .collect();
    for (turn, took) in &first_audio_by_turn {
        if crowd_turns.contains(turn) {
            summary.first_audio_crowd_ms.push(*took);
        } else if deferred_turns.contains(turn) {
            summary.first_audio_deferred_ms.push(*took);
        } else {
            summary.first_audio_direct_ms.push(*took);
        }
    }
    for route in &mut summary.crowd_routes {
        let (phrases, cached) = turn_phrases.get(&route.turn).copied().unwrap_or_default();
        route.phrases = phrases;
        route.cached_phrases = cached;
        route.first_audio_ms = first_audio_by_turn
            .iter()
            .find(|(turn, _)| *turn == route.turn)
            .map(|(_, took)| *took);
        if let Some(points) = timing_points.get(&route.turn) {
            route.timing = points.intervals();
        }
    }
    summary.first_audio_ms.sort_unstable();
    summary.first_audio_direct_ms.sort_unstable();
    summary.first_audio_deferred_ms.sort_unstable();
    summary.first_audio_crowd_ms.sort_unstable();
    summary.initiative_spacing_ms.sort_unstable();
    summary.repeat_refusal_age_ms.sort_unstable();
    summary.repeat_refusals_by_origin.sort();
    summary.repeat_refusals_by_shown.sort();
    summary.turns.sort();
    summary.repeats.sort_by(|a, b| a.origin.cmp(&b.origin));
    summary
}

fn rate(part: u64, whole: u64) -> String {
    if whole == 0 {
        "n/a".to_owned()
    } else {
        format!("{:.3}", part as f64 / whole as f64)
    }
}

fn opt(value: Option<u64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |v| v.to_string())
}

/// Plain `key=value` lines, like the rest of `aivtuberctl`. Counts and
/// timings only: no logged text is ever printed.
pub fn render_summary(summary: &SessionSummary) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "lines={} malformed={} capped={} truncated_read={}\n",
        summary.lines, summary.malformed, summary.capped, summary.truncated_read
    ));
    let total: u64 = summary.turns.iter().map(|(_, n)| n).sum();
    out.push_str(&format!("turns total={total}"));
    for (origin, count) in &summary.turns {
        out.push_str(&format!(" {origin}={count}"));
    }
    out.push('\n');
    out.push_str(&format!(
        "phrases total={} fallbacks={}\n",
        summary.phrases, summary.fallbacks
    ));
    for entry in &summary.repeats {
        out.push_str(&format!(
            "repeat origin={} phrases={} exact={} near={} exact_rate={} near_rate={}\n",
            entry.origin,
            entry.phrases,
            entry.exact,
            entry.near,
            rate(entry.exact, entry.phrases),
            rate(entry.near, entry.phrases),
        ));
    }
    out.push_str(&format!(
        "repeat_refused total={} age_ms_p50={} age_ms_max={}
",
        summary.repeat_refusals,
        opt(percentile(&summary.repeat_refusal_age_ms, 50)),
        opt(summary.repeat_refusal_age_ms.last().copied()),
    ));
    for (origin, count) in &summary.repeat_refusals_by_origin {
        out.push_str(&format!(
            "repeat_refused matched_origin={origin} count={count}
"
        ));
    }
    for (shown, count) in &summary.repeat_refusals_by_shown {
        out.push_str(&format!(
            "repeat_refused matched_shown={shown} count={count}
"
        ));
    }
    out.push_str(&format!(
        "first_audio_ms count={} p50={} p95={} max={}\n",
        summary.first_audio_ms.len(),
        opt(percentile(&summary.first_audio_ms, 50)),
        opt(percentile(&summary.first_audio_ms, 95)),
        opt(summary.first_audio_ms.last().copied()),
    ));
    for (path, values) in [
        ("direct", &summary.first_audio_direct_ms),
        ("deferred", &summary.first_audio_deferred_ms),
        ("crowd", &summary.first_audio_crowd_ms),
    ] {
        out.push_str(&format!(
            "first_audio_ms path={path} count={} p50={} p95={} max={}
",
            values.len(),
            opt(percentile(values, 50)),
            opt(percentile(values, 95)),
            opt(values.last().copied()),
        ));
    }
    out.push_str(&format!(
        "crowd_routes count={}
",
        summary.crowd_routes.len()
    ));
    for route in &summary.crowd_routes {
        out.push_str(&format!(
            "crowd_route turn={} class={} members={} actors={} phrases={} cached_phrases={} first_audio_ms={}
",
            route.turn,
            route.class,
            route.members,
            route.actors,
            route.phrases,
            route.cached_phrases,
            opt(route.first_audio_ms),
        ));
    }
    // Keep the legacy input-to-audio fields above unchanged. These intervals
    // are deliberately named after their log boundaries, not physical audio.
    for (stage, get) in [
        (
            "input_to_route_observation",
            (|t: &CrowdTiming| t.input_to_route_ms) as fn(&CrowdTiming) -> Option<u64>,
        ),
        ("route_observation_to_dispatch", |t: &CrowdTiming| {
            t.route_to_dispatch_ms
        }),
        ("dispatch_to_first_phrase", |t: &CrowdTiming| {
            t.dispatch_to_phrase_ms
        }),
        ("route_observation_to_first_phrase", |t: &CrowdTiming| {
            t.route_to_phrase_ms
        }),
    ] {
        let mut values: Vec<_> = summary
            .crowd_routes
            .iter()
            .filter_map(|route| get(&route.timing))
            .collect();
        values.sort_unstable();
        out.push_str(&format!(
            "crowd_timing_ms stage={stage} count={} routes={} p50={} p95={} max={}\n",
            values.len(),
            summary.crowd_routes.len(),
            opt(percentile(&values, 50)),
            opt(percentile(&values, 95)),
            opt(values.last().copied()),
        ));
    }
    for route in &summary.crowd_routes {
        let t = &route.timing;
        out.push_str(&format!(
            "crowd_timing turn={} input_to_route_ms={} route_to_dispatch_ms={} dispatch_to_phrase_ms={} route_to_phrase_ms={}\n",
            route.turn, opt(t.input_to_route_ms), opt(t.route_to_dispatch_ms),
            opt(t.dispatch_to_phrase_ms), opt(t.route_to_phrase_ms),
        ));
    }
    out.push_str(&format!(
        "initiative_spacing_ms count={} min={} p50={} max={}\n",
        summary.initiative_spacing_ms.len(),
        opt(summary.initiative_spacing_ms.first().copied()),
        opt(percentile(&summary.initiative_spacing_ms, 50)),
        opt(summary.initiative_spacing_ms.last().copied()),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aivtuber-session-log-{name}-{}-{}",
            std::process::id(),
            random_hex(4)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn files(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = fs::read_dir(dir)
            .map(|entries| entries.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default();
        found.sort();
        found
    }

    fn lines(dir: &Path) -> Vec<Record> {
        let all = files(dir);
        let path = all.last().expect("a session file");
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is one record"))
            .collect()
    }

    fn redactor() -> SecretRedactor {
        SecretRedactor::new(["sk-secret-value"])
    }

    #[test]
    fn the_summary_splits_first_audio_by_path_and_lists_crowd_routes() {
        let lines = [
            r#"{"v":1,"t":"input","seq":1,"turn":"t-0001","logical_ms":1000,"origin":"chat","viewer":"viewer-01","text":"hi"}"#,
            r#"{"v":1,"t":"note","seq":2,"turn":"t-0001","logical_ms":1000,"kind":"path","fallback":"direct"}"#,
            r#"{"v":1,"t":"phrase","seq":3,"turn":"t-0001","logical_ms":3000,"origin":"chat","route":"generated","idx":0}"#,
            r#"{"v":1,"t":"input","seq":4,"turn":"t-0002","logical_ms":2000,"origin":"chat","viewer":"viewer-02","text":"q"}"#,
            r#"{"v":1,"t":"note","seq":5,"turn":"t-0002","logical_ms":9000,"kind":"path","fallback":"deferred"}"#,
            r#"{"v":1,"t":"phrase","seq":6,"turn":"t-0002","logical_ms":10000,"origin":"chat","route":"generated","idx":0}"#,
            r#"{"v":1,"t":"input","seq":7,"turn":"t-0003","logical_ms":4000,"origin":"chat","viewer":"viewer-03","text":"GG"}"#,
            r#"{"v":1,"t":"crowd","seq":8,"turn":"t-0003","logical_ms":5000,"kind":"congratulation","members":5,"actors":4}"#,
            r#"{"v":1,"t":"phrase","seq":9,"turn":"t-0003","logical_ms":6500,"origin":"cached","route":"cached","idx":0}"#,
        ]
        .join("
");
        let summary = summarize(lines.as_bytes());
        assert_eq!(summary.first_audio_direct_ms, vec![2000]);
        assert_eq!(summary.first_audio_deferred_ms, vec![8000]);
        assert_eq!(summary.first_audio_crowd_ms, vec![2500]);
        assert_eq!(summary.crowd_routes.len(), 1);
        let route = &summary.crowd_routes[0];
        assert_eq!(
            (
                route.class.as_str(),
                route.members,
                route.actors,
                route.phrases
            ),
            ("congratulation", 5, 4, 1)
        );
        assert_eq!(route.cached_phrases, 1);
        assert_eq!(route.timing.input_to_route_ms, Some(1000));
        assert_eq!(route.timing.route_to_phrase_ms, Some(1500));
        assert_eq!(route.timing.route_to_dispatch_ms, None);
        assert_eq!(route.timing.dispatch_to_phrase_ms, None);
        let text = render_summary(&summary);
        assert!(text.contains("first_audio_ms path=crowd count=1 p50=2500"));
        assert!(text.contains("crowd_route turn=t-0003 class=congratulation members=5 actors=4"));
        assert!(!text.contains("viewer"), "no viewer ids in the summary");
    }

    #[test]
    fn crowd_timing_decomposes_direct_and_deferred_paths_without_text() {
        let mut lines = Vec::new();
        for (turn, dispatch, first, path) in
            [("a", 2000, 2500, "direct"), ("b", 4000, 4700, "deferred")]
        {
            for (kind, ms, extra) in [
                (
                    "input",
                    1000,
                    serde_json::json!({"origin":"chat","text":"PRIVATE"}),
                ),
                (
                    "crowd",
                    2000,
                    serde_json::json!({"kind":"praise","members":3,"actors":3}),
                ),
                (
                    "note",
                    dispatch,
                    serde_json::json!({"kind":"path","fallback":path}),
                ),
                (
                    "phrase",
                    first,
                    serde_json::json!({"idx":0,"route":"cached","text":"PRIVATE"}),
                ),
                (
                    "phrase",
                    first + 100,
                    serde_json::json!({"idx":1,"route":"cached"}),
                ),
            ] {
                let mut record = serde_json::json!({"v":1,"t":kind,"seq":lines.len(),"turn":turn,"logical_ms":ms});
                record
                    .as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                lines.push(record.to_string());
            }
        }
        let summary = summarize(lines.join("\n").as_bytes());
        assert_eq!(summary.first_audio_crowd_ms, vec![1500, 3700]);
        assert_eq!(
            summary.crowd_routes[0].timing,
            CrowdTiming {
                input_to_route_ms: Some(1000),
                route_to_dispatch_ms: Some(0),
                dispatch_to_phrase_ms: Some(500),
                route_to_phrase_ms: Some(500)
            }
        );
        assert_eq!(
            summary.crowd_routes[1].timing,
            CrowdTiming {
                input_to_route_ms: Some(1000),
                route_to_dispatch_ms: Some(2000),
                dispatch_to_phrase_ms: Some(700),
                route_to_phrase_ms: Some(2700)
            }
        );
        let text = render_summary(&summary);
        assert!(
            text.contains(
                "stage=route_observation_to_first_phrase count=2 routes=2 p50=500 p95=2700"
            )
        );
        assert!(!text.contains("PRIVATE"));
    }

    #[test]
    fn crowd_timing_excludes_ambiguous_or_reversed_points_and_unvoiced_audio() {
        let records = [
            r#"{"v":1,"t":"input","seq":1,"turn":"a","logical_ms":1000}"#,
            r#"{"v":1,"t":"crowd","seq":2,"turn":"a","logical_ms":2000}"#,
            r#"{"v":1,"t":"note","seq":3,"turn":"a","logical_ms":1500,"kind":"path","fallback":"direct"}"#,
            r#"{"v":1,"t":"phrase","seq":4,"turn":"a","logical_ms":2500,"idx":0}"#,
            r#"{"v":1,"t":"crowd","seq":5,"turn":"b","logical_ms":2000}"#,
            r#"{"v":1,"t":"crowd","seq":6,"turn":"b","logical_ms":2000}"#,
            r#"{"v":1,"t":"phrase","seq":7,"turn":"b","logical_ms":3000,"idx":0}"#,
            r#"{"v":1,"t":"input","seq":8,"turn":"c","logical_ms":1000}"#,
            r#"{"v":1,"t":"crowd","seq":9,"turn":"c","logical_ms":2000}"#,
            r#"{"v":1,"t":"fallback","seq":10,"turn":"c","logical_ms":3000,"kind":"silent","fallback":"crowd_cooldown"}"#,
            r#"{"v":1,"t":"crowd","seq":11,"turn":"d","logical_ms":2000}"#,
            r#"{"v":1,"t":"phrase","seq":12,"turn":"d","logical_ms":3000,"idx":1}"#,
            r#"{"v":1,"t":"crowd","seq":13,"turn":"e"}"#,
            r#"{"v":1,"t":"phrase","seq":14,"turn":"e","logical_ms":3000,"idx":0}"#,
        ];
        let summary = summarize(records.join("\n").as_bytes());
        assert_eq!(summary.crowd_routes.len(), 6);
        for route in &summary.crowd_routes {
            assert_eq!(route.timing.route_to_phrase_ms, None, "{}", route.turn);
        }
        let unvoiced = &summary.crowd_routes[3];
        assert_eq!(unvoiced.turn, "c");
        assert_eq!(unvoiced.phrases, 0);
        assert_eq!(unvoiced.timing.input_to_route_ms, Some(1000));
        assert!(
            render_summary(&summary)
                .contains("stage=route_observation_to_first_phrase count=0 routes=6 p50=n/a")
        );
    }

    #[test]
    fn repeat_refusals_are_counted_by_matched_origin_and_shown() {
        let lines = [
            r#"{"v":1,"t":"note","seq":1,"turn":"a","logical_ms":1,"kind":"initiative_repeat_suppressed","fallback":"exact","matched_origin":"initiative","matched_age_ms":600,"matched_shown":true}"#,
            r#"{"v":1,"t":"note","seq":2,"turn":"b","logical_ms":2,"kind":"initiative_repeat_suppressed","fallback":"near","matched_origin":"chat_reply","matched_age_ms":9000,"matched_shown":false}"#,
            r#"{"v":1,"t":"note","seq":3,"turn":"c","logical_ms":3,"kind":"initiative_repeat_suppressed","fallback":"exact"}"#,
        ];
        let summary = summarize(
            lines
                .join(
                    "
",
                )
                .as_bytes(),
        );
        assert_eq!(summary.repeat_refusals, 3);
        assert_eq!(
            summary.repeat_refusals_by_origin,
            vec![
                ("chat_reply".to_owned(), 1),
                ("initiative".to_owned(), 1),
                ("unknown".to_owned(), 1)
            ]
        );
        assert_eq!(
            summary.repeat_refusals_by_shown,
            vec![
                ("false".to_owned(), 1),
                ("true".to_owned(), 1),
                ("unknown".to_owned(), 1)
            ]
        );
        assert_eq!(summary.repeat_refusal_age_ms, vec![600, 9000]);
        let out = render_summary(&summary);
        assert!(out.contains("repeat_refused total=3 age_ms_p50=600 age_ms_max=9000"));
        assert!(out.contains("repeat_refused matched_origin=chat_reply count=1"));
        assert!(out.contains("repeat_refused matched_shown=unknown count=1"));
    }

    #[test]
    fn a_crowd_record_carries_counts_and_no_viewer_text() {
        let dir = temp_dir("crowd");
        let mut log = SessionLog::new(dir.join("logs"), None, true);
        log.input(
            &redactor(),
            "e1",
            "chat",
            Some("UCabc"),
            Some("GG"),
            None,
            1,
        );
        log.crowd("e1", "celebration", 5, 4, 2);
        log.flush();
        let records = lines(&dir.join("logs"));
        let crowd = records.iter().find(|r| r.t == "crowd").expect("crowd");
        assert_eq!(crowd.turn.as_deref(), Some("t-0001"));
        assert_eq!((crowd.members, crowd.actors), (Some(5), Some(4)));
        assert!(crowd.text.is_none() && crowd.viewer.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_launcher_names_are_pinned() {
        // `aivtuber-setup` repeats these literals (the crates are not linked).
        assert_eq!(SESSION_LOG_DIR_VAR, "AIVTUBER_SESSION_LOG_DIR");
        assert_eq!(SESSION_LOG_SETTINGS_VAR, "AIVTUBER_SESSION_LOG_SETTINGS");
    }

    #[test]
    fn off_by_default_writes_nothing_and_creates_nothing() {
        let dir = temp_dir("off");
        let settings = dir.join("settings.json");
        let mut log = SessionLog::from_settings(dir.join("logs"), settings.clone());
        assert!(!log.enabled());
        log.input(
            &redactor(),
            "e1",
            "chat",
            Some("UCabc"),
            Some("hello"),
            None,
            1,
        );
        log.phrase(&redactor(), "e1", true, "reply", "none", 2);
        log.route("e1", "generated", None, None, 2);
        log.note_hashes(Some("aa"), None);
        log.flush();
        assert!(!dir.join("logs").exists(), "no directory while off");
        assert!(!settings.exists());
        assert_eq!(log.status().lines_written, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_choice_is_remembered_and_anything_odd_reads_as_off() {
        let dir = temp_dir("settings");
        let path = dir.join("s.json");
        let mut log = SessionLog::from_settings(dir.join("logs"), path.clone());
        log.set_enabled(true).unwrap();
        assert!(read_enabled(&path));
        assert!(SessionLog::from_settings(dir.join("logs"), path.clone()).enabled());
        log.set_enabled(false).unwrap();
        assert!(!read_enabled(&path));
        for bad in [
            "",
            "{}",
            "not json",
            r#"{"schema":"local-session-log-settings/1","enabled":true,"x":1}"#,
            r#"{"schema":"other/1","enabled":true}"#,
            &format!(
                r#"{{"schema":"local-session-log-settings/1","enabled":true,"pad":"{}"}}"#,
                "x".repeat(300)
            ),
        ] {
            fs::write(&path, bad).unwrap();
            assert!(!read_enabled(&path), "{bad:?}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn viewers_are_pseudonymised_and_raw_ids_never_reach_the_file() {
        let dir = temp_dir("pseudo");
        let logs = dir.join("logs");
        let mut log = SessionLog::new(logs.clone(), None, true);
        let r = redactor();
        log.input(
            &r,
            "e1",
            "chat",
            Some("UCrawchannel1"),
            Some("hi"),
            None,
            10,
        );
        log.input(
            &r,
            "e2",
            "chat",
            Some("UCrawchannel2"),
            Some("I am UCrawchannel1"),
            None,
            20,
        );
        log.input(
            &r,
            "e3",
            "chat",
            Some("UCrawchannel1"),
            Some("again"),
            None,
            30,
        );
        // A reply that repeats a raw id is scrubbed too.
        log.phrase(&r, "e2", true, "こんにちは UCrawchannel2 さん", "none", 40);
        log.flush();
        let text = fs::read_to_string(files(&logs).last().unwrap()).unwrap();
        assert!(!text.contains("UCraw"), "{text}");
        let records = lines(&logs);
        let viewers: Vec<_> = records.iter().filter_map(|r| r.viewer.clone()).collect();
        assert_eq!(viewers, ["viewer-01", "viewer-02", "viewer-01"]);
        assert!(text.contains("I am viewer-01"));
        assert!(text.contains("こんにちは viewer-02 さん"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn secrets_urls_and_long_text_are_removed_or_bounded() {
        let dir = temp_dir("scrub");
        let logs = dir.join("logs");
        let mut log = SessionLog::new(logs.clone(), None, true);
        let long = "あ".repeat(FIELD_MAX_CHARS * 3);
        log.input(
            &redactor(),
            "e1",
            "chat",
            Some("UCx123"),
            Some(
                "key sk-secret-value and https://example.com/a?token=abc#f and HTTP://X.test/p ok",
            ),
            None,
            1,
        );
        log.phrase(&redactor(), "e1", true, &long, "none", 2);
        log.flush();
        let records = lines(&logs);
        let input = records.iter().find(|r| r.t == "input").unwrap();
        assert_eq!(
            input.text.as_deref(),
            Some("key [REDACTED] and [url] and [url] ok")
        );
        let phrase = records.iter().find(|r| r.t == "phrase").unwrap();
        let cut = phrase.text.as_deref().unwrap();
        assert_eq!(cut.chars().count(), FIELD_MAX_CHARS + 1);
        assert!(cut.ends_with('…'));
        for line in fs::read_to_string(files(&logs).last().unwrap())
            .unwrap()
            .lines()
        {
            assert!(line.len() <= MAX_LINE_BYTES);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn raw_event_ids_are_never_written_and_phrases_share_their_turn() {
        let dir = temp_dir("turns");
        let logs = dir.join("logs");
        let mut log = SessionLog::new(logs.clone(), None, true);
        let r = redactor();
        log.input(&r, "evt-secretish-1", "chat", Some("a"), Some("q"), None, 5);
        log.phrase(&r, "evt-secretish-1", true, "first", "none", 6);
        log.phrase(&r, "evt-secretish-1#p1", true, "second", "near", 7);
        log.phrase(&r, "cached-reaction", false, "cached one", "exact", 8);
        log.flush();
        let text = fs::read_to_string(files(&logs).last().unwrap()).unwrap();
        assert!(!text.contains("evt-secretish"));
        let records = lines(&logs);
        let phrases: Vec<_> = records.iter().filter(|r| r.t == "phrase").collect();
        assert_eq!(phrases[0].turn, phrases[1].turn);
        assert_eq!(phrases[0].idx, Some(0));
        assert_eq!(phrases[1].idx, Some(1));
        assert_eq!(phrases[1].repeat.as_deref(), Some("near"));
        assert_ne!(phrases[0].turn, phrases[2].turn);
        assert_eq!(phrases[2].origin.as_deref(), Some("cached"));
        // `seq` is strictly increasing: the ordering key.
        let seqs: Vec<u64> = records.iter().map(|r| r.seq).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_stops_at_its_bound_and_says_so() {
        let dir = temp_dir("cap");
        let logs = dir.join("logs");
        let mut log = SessionLog::new(logs.clone(), None, true);
        let r = redactor();
        let text = "x".repeat(FIELD_MAX_CHARS);
        let mut sent = 0;
        for _ in 0..400 {
            for _ in 0..64 {
                log.input(
                    &r,
                    &format!("e{sent}"),
                    "chat",
                    Some("v"),
                    Some(&text),
                    None,
                    sent,
                );
                sent += 1;
            }
            log.flush();
            if log.status().capped {
                break;
            }
        }
        log.input(&r, "late", "chat", Some("v"), Some("after"), None, 9);
        log.flush();
        let status = log.status();
        assert!(status.capped);
        assert!(status.dropped_file_full >= 1);
        let size = fs::metadata(files(&logs).last().unwrap()).unwrap().len();
        assert!(size <= FILE_MAX_BYTES, "{size}");
        let records = lines(&logs);
        assert_eq!(records.last().unwrap().t, "capped");
        assert!(summarize(fs::read(files(&logs).last().unwrap()).unwrap().as_slice()).capped);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_newest_twenty_sessions_are_kept_and_foreign_files_are_left_alone() {
        let dir = temp_dir("retain");
        let logs = dir.join("logs");
        fs::create_dir_all(&logs).unwrap();
        for n in 0..25 {
            let name = format!("session-202001{:02}T000000Z-aaaa{n:04}.jsonl", n + 1);
            fs::write(logs.join(name), "{}\n").unwrap();
        }
        fs::write(logs.join("notes.txt"), "mine").unwrap();
        fs::write(logs.join("session-keep.txt"), "mine").unwrap();
        let mut log = SessionLog::new(logs.clone(), None, true);
        log.input(&redactor(), "e", "chat", None, Some("hi"), None, 1);
        log.flush();
        let sessions: Vec<_> = files(&logs)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        assert_eq!(sessions.len(), RETAIN_SESSIONS);
        assert!(
            !logs
                .join("session-20200101T000000Z-aaaa0000.jsonl")
                .exists()
        );
        assert!(logs.join("notes.txt").exists());
        assert!(logs.join("session-keep.txt").exists());
        assert_eq!(log.status().files_kept, RETAIN_SESSIONS as u64);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_all_removes_every_session_file_and_recording_resumes_cleanly() {
        let dir = temp_dir("delete");
        let logs = dir.join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("session-20200101T000000Z-bbbb0000.jsonl"), "{}\n").unwrap();
        fs::write(logs.join("other.txt"), "mine").unwrap();
        let mut log = SessionLog::new(logs.clone(), None, true);
        log.input(&redactor(), "e", "chat", None, Some("secret chat"), None, 1);
        log.flush();
        assert_eq!(files(&logs).len(), 3);
        assert!(log.delete_all());
        log.flush();
        assert_eq!(files(&logs), vec![logs.join("other.txt")]);
        assert_eq!(log.status().files_kept, 0);
        // Still on: the next line starts a fresh file with its header.
        log.input(&redactor(), "e2", "chat", None, Some("next"), None, 2);
        log.flush();
        let records = lines(&logs);
        assert_eq!(records[0].t, "session");
        assert!(
            records
                .iter()
                .all(|r| r.text.as_deref() != Some("secret chat"))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_failure_is_counted_and_never_blocks() {
        let dir = temp_dir("fail");
        fs::create_dir_all(&dir).unwrap();
        // The log directory path is a file, so it can never be created.
        let blocked = dir.join("blocked");
        fs::write(&blocked, "x").unwrap();
        let mut log = SessionLog::new(blocked.join("sessions"), None, true);
        let started = std::time::Instant::now();
        for n in 0..50 {
            log.input(
                &redactor(),
                &format!("e{n}"),
                "chat",
                None,
                Some("hi"),
                None,
                n,
            );
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        log.flush();
        let status = log.status();
        assert!(status.write_errors >= 1);
        assert_eq!(status.lines_written, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_queue_drops_and_counts_instead_of_blocking() {
        let dir = temp_dir("full");
        let (tx, _rx) = sync_channel(1);
        let mut log = SessionLog::new(dir.join("logs"), None, true);
        // Replace the writer with a channel nobody drains.
        log.tx = tx;
        let started = std::time::Instant::now();
        for n in 0..20 {
            log.input(
                &redactor(),
                &format!("e{n}"),
                "chat",
                None,
                Some("hi"),
                None,
                n,
            );
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(log.status().dropped_queue_full, 19);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_summary_counts_turns_repeats_latency_and_initiative_spacing() {
        let dir = temp_dir("summary");
        let logs = dir.join("logs");
        let mut log = SessionLog::new(logs.clone(), None, true);
        let r = redactor();
        log.input(&r, "c1", "chat", Some("a"), Some("q1"), None, 1_000);
        log.phrase(&r, "c1", true, "answer one", "none", 1_400);
        log.input(&r, "c2", "voice", None, Some("q2"), None, 2_000);
        log.phrase(&r, "c2", true, "answer one", "exact", 2_900);
        log.route(
            "c2",
            "cached_fallback",
            Some("memory"),
            Some("deadline"),
            2_900,
        );
        log.input(
            &r,
            "i1",
            "initiative",
            None,
            None,
            Some("idle_observation"),
            10_000,
        );
        log.phrase(&r, "i1", true, "idle words", "none", 10_500);
        log.input(
            &r,
            "i2",
            "initiative",
            None,
            None,
            Some("revisit_topic"),
            70_000,
        );
        log.phrase(&r, "i2", true, "idle words", "near", 70_300);
        log.flush();
        let bytes = fs::read(files(&logs).last().unwrap()).unwrap();
        let summary = summarize(bytes.as_slice());
        assert_eq!(summary.malformed, 0);
        assert_eq!(
            summary.turns,
            vec![
                ("chat".into(), 1),
                ("initiative".into(), 2),
                ("voice".into(), 1)
            ]
        );
        assert_eq!(summary.phrases, 4);
        assert_eq!(summary.first_audio_ms, vec![400, 900]);
        assert_eq!(summary.initiative_spacing_ms, vec![60_000]);
        assert_eq!(summary.fallbacks, 1);
        let initiative = summary
            .repeats
            .iter()
            .find(|e| e.origin == "initiative")
            .unwrap();
        assert_eq!(
            (initiative.phrases, initiative.exact, initiative.near),
            (2, 0, 1)
        );
        let rendered = render_summary(&summary);
        assert!(
            rendered.contains("turns total=4 chat=1 initiative=2 voice=1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("first_audio_ms count=2 p50=400 p95=900 max=900"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "repeat origin=initiative phrases=2 exact=0 near=1 exact_rate=0.000 near_rate=0.500"
            ),
            "{rendered}"
        );
        assert!(
            !rendered.contains("answer one"),
            "no logged text is printed"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_summary_survives_damaged_oversized_and_empty_input() {
        let mut data = Vec::new();
        data.extend_from_slice(b"not json\n");
        data.extend_from_slice(&vec![b'x'; SUMMARY_MAX_LINE_BYTES * 2]);
        data.extend_from_slice(
            b"\n{\"v\":1,\"t\":\"phrase\",\"seq\":1,\"origin\":\"chat\",\"repeat\":\"exact\"}\n",
        );
        let summary = summarize(data.as_slice());
        assert_eq!(summary.malformed, 2);
        assert_eq!(summary.phrases, 1);
        let empty = summarize(&b""[..]);
        assert_eq!(empty.lines, 0);
        assert!(render_summary(&empty).contains("first_audio_ms count=0 p50=n/a"));
    }

    #[test]
    fn summary_bounds_total_input_including_unterminated_oversized_records() {
        // A generated reader needs no allocation proportional to the bad line.
        let source = std::io::repeat(b'x').take(SUMMARY_MAX_BYTES + 1);
        let mut reader = std::io::BufReader::new(source);
        let summary = summarize(&mut reader);
        assert_eq!(summary.lines, 1);
        assert_eq!(summary.malformed, 1);
        assert!(summary.truncated_read);
        assert_eq!(summary.phrases, 0);
        // The next byte was not consumed by the summary.
        assert_eq!(reader.fill_buf().unwrap(), b"x");
    }

    #[test]
    fn summary_marks_io_failures_during_read_and_oversize_discard() {
        struct Fails;
        impl Read for Fails {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("synthetic read failure"))
            }
        }
        let valid = b"{\"v\":1,\"t\":\"phrase\",\"seq\":1,\"origin\":\"chat\"}\n";
        let input = std::io::BufReader::new(valid.as_slice().chain(Fails));
        let summary = summarize(input);
        assert_eq!(summary.phrases, 1);
        assert!(summary.truncated_read);
        assert!(render_summary(&summary).contains("truncated_read=true"));

        let oversized = std::io::repeat(b'x').take((SUMMARY_MAX_LINE_BYTES * 2) as u64);
        let summary = summarize(std::io::BufReader::new(oversized.chain(Fails)));
        assert_eq!(summary.malformed, 1);
        assert_eq!(summary.phrases, 0);
        assert!(summary.truncated_read);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let values: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&values, 50), Some(50));
        assert_eq!(percentile(&values, 95), Some(95));
        assert_eq!(percentile(&[7], 95), Some(7));
        assert_eq!(percentile(&[], 50), None);
    }

    #[test]
    fn the_wall_clock_formats_known_instants() {
        let at = |secs: u64| civil(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(at(1_795_341_296), (2026, 11, 22, 9, 54, 56));
        assert_eq!(wall_iso().len(), 20);
    }
}
