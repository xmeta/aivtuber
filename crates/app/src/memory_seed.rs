//! Operator-owned memory seed file and its export (#321, ADR-0008 S2).
//!
//! The seed file is the cheapest benefit test for cross-session continuity: a
//! short list of performer-level facts the operator curates by hand, loaded
//! into working memory at startup, with no runtime disk write anywhere. This
//! module owns the file contract (`schemas/memory-seed.schema.json`), its
//! bounds and typed errors, the deterministic content hash recorded as a replay
//! input, and the counters for the ADR-0008 Q4 falsifier.
//!
//! Boundaries (all tested):
//!
//! * The format has no field that can name a viewer or an actor, and unknown
//!   fields are refused, so viewer-attributed data cannot be expressed in a
//!   seed file or in an export.
//! * Loading is not a new authority. Every entry goes through
//!   [`aivtuber_runtime::SecurityRuntime::authorize_operator_seed_memory_write`]
//!   and the permit-gated durable path; the file path is process configuration
//!   the operator owns, never an input chat or a model can reach.
//! * The runtime never writes a seed file. The export is returned to an
//!   authenticated operator command and the *client* decides where it lands.
//! * Bounds are fixed: entry count, claim bytes and file bytes. Anything over a
//!   bound is refused, never truncated.
//! * Loaded entries are ordinary actorless durable entries, so the #305
//!   operator erase removes them for the running session. The file is not
//!   touched by an erase and reloads on the next start.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use aivtuber_domain::ThinkingContent;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::memory_writer::{MEMORY_WRITER_EVENT_PREFIX, fold};

/// `schema_version` this loader accepts.
pub const MEMORY_SEED_SCHEMA_VERSION: &str = "0.1.0";
/// Event-id prefix of every seed-minted entry; the falsifier counter keys on it.
pub const MEMORY_SEED_EVENT_PREFIX: &str = "memory-seed.";
/// Entries one file may carry. Equal to the writer ceiling so an export of a
/// full writer always fits, and at most half of the default store.
pub const MAX_SEED_ENTRIES: usize = 128;
/// UTF-8 bytes in one claim. Over-length is refused whole, never truncated.
pub const MAX_SEED_CLAIM_BYTES: usize = 240;
/// File size refused before parsing.
pub const MAX_SEED_FILE_BYTES: usize = 64 * 1024;

const SEED_ID_MAX: usize = 63;
const REVISION_MAX: usize = 32;
/// `seed_id` of an export. The operator renames it when promoting.
pub const EXPORT_SEED_ID: &str = "memory-export";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemorySeedError {
    Io(String),
    TooLarge {
        bytes: usize,
    },
    Parse(String),
    Invalid {
        path: String,
        reason: String,
    },
    /// The security runtime refused an entry (for example text the public
    /// output gate would withhold). Names the entry index, never the text.
    Refused {
        index: usize,
    },
    /// Seeding needs the adaptation runtime that owns working memory.
    AdaptationUnavailable,
    /// The store cannot hold the seed beside its own bounds.
    StoreCapacity {
        entries: usize,
        limit: usize,
    },
    StoreClaimBound {
        index: usize,
        limit: usize,
    },
    Store(String),
    /// The export needs the authenticated `memory.admin` capability.
    Unauthorized,
    /// There is no writer, so there is nothing to export.
    WriterDisabled,
    /// An erase transaction is open; its scope must finish before an export
    /// may read the store.
    ErasePending,
}

impl fmt::Display for MemorySeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(detail) => write!(f, "memory seed io error: {detail}"),
            Self::TooLarge { bytes } => {
                write!(
                    f,
                    "memory seed is {bytes} bytes, over {MAX_SEED_FILE_BYTES}"
                )
            }
            Self::Parse(detail) => write!(f, "memory seed is not valid: {detail}"),
            Self::Invalid { path, reason } => write!(f, "memory seed {path}: {reason}"),
            Self::Refused { index } => write!(
                f,
                "memory seed entry {index} was refused by the memory write gate"
            ),
            Self::AdaptationUnavailable => {
                write!(f, "memory seed needs the adaptation runtime to be composed")
            }
            Self::StoreCapacity { entries, limit } => write!(
                f,
                "memory seed has {entries} entries but the store may give it at most {limit}"
            ),
            Self::StoreClaimBound { index, limit } => write!(
                f,
                "memory seed entry {index} is longer than the store claim bound of {limit} bytes"
            ),
            Self::Store(detail) => write!(f, "memory seed could not be stored: {detail}"),
            Self::Unauthorized => write!(f, "memory export requires the memory.admin capability"),
            Self::WriterDisabled => write!(f, "memory writer is not enabled; nothing to export"),
            Self::ErasePending => write!(f, "a memory erase is still open; retry the export"),
        }
    }
}

impl Error for MemorySeedError {}

fn invalid(path: impl Into<String>, reason: impl Into<String>) -> MemorySeedError {
    MemorySeedError::Invalid {
        path: path.into(),
        reason: reason.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeedKind {
    PersonaFact,
    RunningJoke,
    Commitment,
    SegmentSummary,
}

impl SeedKind {
    /// Memory topic the entry is stored under. The last three are the writer's
    /// own topics, so seed and writer entries rank alike in retrieval.
    pub fn topic(self) -> &'static str {
        match self {
            Self::PersonaFact => "persona-fact",
            Self::RunningJoke => "running-joke",
            Self::Commitment => "commitment",
            Self::SegmentSummary => "segment-summary",
        }
    }

    pub(crate) fn from_topic(topic: &str) -> Option<Self> {
        match topic {
            "persona-fact" => Some(Self::PersonaFact),
            "running-joke" => Some(Self::RunningJoke),
            "commitment" => Some(Self::Commitment),
            "segment-summary" => Some(Self::SegmentSummary),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedEntry {
    pub kind: SeedKind,
    pub claim: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedFile {
    schema_version: String,
    seed_id: String,
    revision: String,
    entries: Vec<SeedEntry>,
}

/// A validated seed. Constructed only through [`MemorySeed::parse`],
/// [`MemorySeed::load`] or [`MemorySeed::from_entries`], so a value of this type
/// has already passed every bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySeed {
    seed_id: String,
    revision: String,
    entries: Vec<SeedEntry>,
}

/// Identity of a loaded seed, recorded as a replay input: the same seed content
/// always yields the same hash, independent of whitespace in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySeedIdentity {
    pub seed_id: String,
    pub revision: String,
    pub content_hash: String,
    pub entries: usize,
}

fn valid_seed_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= SEED_ID_MAX
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

fn valid_revision(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= REVISION_MAX
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Why a claim does not meet the per-claim bounds, if it does not.
fn claim_problem(claim: &str) -> Option<String> {
    if claim.trim().is_empty() {
        Some("must not be empty".to_owned())
    } else if claim.trim() != claim {
        Some("must not have leading or trailing whitespace".to_owned())
    } else if claim.len() > MAX_SEED_CLAIM_BYTES {
        Some(format!(
            "{} bytes, over {MAX_SEED_CLAIM_BYTES}; shorten it, it is never truncated",
            claim.len()
        ))
    } else if claim.chars().any(char::is_control) {
        Some("must be one line without control characters".to_owned())
    } else {
        None
    }
}

/// Build an export from the writer's live `(topic, claim)` pairs. A pair that
/// does not meet the seed bounds, repeats an earlier claim, names an unknown
/// topic or would pass [`MAX_SEED_ENTRIES`] is counted as skipped, never
/// altered.
pub(crate) fn export_from_claims<'a>(
    claims: impl Iterator<Item = (&'static str, &'a str)>,
) -> Result<MemorySeedExport, MemorySeedError> {
    let mut entries: Vec<SeedEntry> = Vec::new();
    let mut seen = HashSet::new();
    let mut skipped = 0;
    for (topic, claim) in claims {
        let kind = SeedKind::from_topic(topic);
        let fits = kind.is_some()
            && claim_problem(claim).is_none()
            && entries.len() < MAX_SEED_ENTRIES
            && !seen.contains(&fold(claim));
        match kind {
            Some(kind) if fits => {
                seen.insert(fold(claim));
                entries.push(SeedEntry {
                    kind,
                    claim: claim.to_owned(),
                });
            }
            _ => skipped += 1,
        }
    }
    Ok(MemorySeedExport {
        seed: MemorySeed::for_export(entries)?,
        skipped,
    })
}

fn len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn digest_entries(entries: &[SeedEntry]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"aivtuber-memory-seed-entries/1\0");
    hasher.update((entries.len() as u64).to_be_bytes());
    for entry in entries {
        len_prefixed(&mut hasher, entry.kind.topic().as_bytes());
        len_prefixed(&mut hasher, entry.claim.as_bytes());
    }
    hasher.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

impl MemorySeed {
    /// Validate a seed from its parts (the export path and tests).
    pub fn from_entries(
        seed_id: impl Into<String>,
        revision: impl Into<String>,
        entries: Vec<SeedEntry>,
    ) -> Result<Self, MemorySeedError> {
        let seed = Self {
            seed_id: seed_id.into(),
            revision: revision.into(),
            entries,
        };
        seed.validate()?;
        Ok(seed)
    }

    /// Parse and validate seed JSON. Refuses more than [`MAX_SEED_FILE_BYTES`]
    /// before parsing; unknown fields, a wrong version and every bound are typed
    /// errors.
    pub fn parse(json: &str) -> Result<Self, MemorySeedError> {
        if json.len() > MAX_SEED_FILE_BYTES {
            return Err(MemorySeedError::TooLarge { bytes: json.len() });
        }
        let file: SeedFile =
            serde_json::from_str(json).map_err(|e| MemorySeedError::Parse(e.to_string()))?;
        if file.schema_version != MEMORY_SEED_SCHEMA_VERSION {
            return Err(invalid(
                "schema_version",
                format!(
                    "expected {MEMORY_SEED_SCHEMA_VERSION}, got {}",
                    file.schema_version
                ),
            ));
        }
        Self::from_entries(file.seed_id, file.revision, file.entries)
    }

    /// Read a seed file, refusing more than [`MAX_SEED_FILE_BYTES`] bytes
    /// before parsing. This is the only read path; there is no write path.
    pub fn load(path: &Path) -> Result<Self, MemorySeedError> {
        let io = |e: std::io::Error| MemorySeedError::Io(format!("{}: {e}", path.display()));
        let file = File::open(path).map_err(io)?;
        let mut bytes = Vec::new();
        file.take(MAX_SEED_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() > MAX_SEED_FILE_BYTES {
            return Err(MemorySeedError::TooLarge { bytes: bytes.len() });
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| MemorySeedError::Parse("file is not valid UTF-8".to_owned()))?;
        Self::parse(&text)
    }

    fn validate(&self) -> Result<(), MemorySeedError> {
        if !valid_seed_id(&self.seed_id) {
            return Err(invalid("seed_id", "must match ^[a-z0-9][a-z0-9._-]{0,62}$"));
        }
        if !valid_revision(&self.revision) {
            return Err(invalid("revision", "must match ^[A-Za-z0-9._-]{1,32}$"));
        }
        if self.entries.len() > MAX_SEED_ENTRIES {
            return Err(invalid(
                "entries",
                format!(
                    "at most {MAX_SEED_ENTRIES} entries, got {}",
                    self.entries.len()
                ),
            ));
        }
        let mut seen = HashSet::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            let path = format!("entries[{index}].claim");
            if let Some(reason) = claim_problem(&entry.claim) {
                return Err(invalid(path, reason));
            }
            if !seen.insert(fold(&entry.claim)) {
                return Err(invalid(
                    path,
                    "repeats an earlier claim (compared ignoring case and spacing)",
                ));
            }
        }
        Ok(())
    }

    pub fn seed_id(&self) -> &str {
        &self.seed_id
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn entries(&self) -> &[SeedEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Hash of the seed's content (id, revision and ordered entries), recorded
    /// as a replay input. File whitespace and key order do not change it.
    pub fn content_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"aivtuber-memory-seed/1\0");
        len_prefixed(&mut hasher, self.seed_id.as_bytes());
        len_prefixed(&mut hasher, self.revision.as_bytes());
        hasher.update(digest_entries(&self.entries));
        hex(&hasher.finalize())
    }

    pub fn identity(&self) -> MemorySeedIdentity {
        MemorySeedIdentity {
            seed_id: self.seed_id.clone(),
            revision: self.revision.clone(),
            content_hash: self.content_hash(),
            entries: self.entries.len(),
        }
    }

    /// Deterministic seed-format JSON: fixed key order, two-space indent, a
    /// trailing newline. The same entries always yield the same bytes.
    pub fn to_json(&self) -> String {
        let file = SeedFile {
            schema_version: MEMORY_SEED_SCHEMA_VERSION.to_owned(),
            seed_id: self.seed_id.clone(),
            revision: self.revision.clone(),
            entries: self.entries.clone(),
        };
        let mut text = serde_json::to_string_pretty(&file).expect("a seed file always serializes");
        text.push('\n');
        text
    }

    /// Seed for an export: fixed id, and a revision derived from the entries so
    /// the same live entries always export the same file.
    pub(crate) fn for_export(entries: Vec<SeedEntry>) -> Result<Self, MemorySeedError> {
        let revision = format!("export-{}", &hex(&digest_entries(&entries))[..12]);
        Self::from_entries(EXPORT_SEED_ID, revision, entries)
    }
}

/// What an export produced: the seed, and how many live writer entries were
/// left out because they do not meet the seed bounds (for example a claim over
/// the byte bound when the writer is configured with a larger one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySeedExport {
    pub seed: MemorySeed,
    pub skipped: usize,
}

/// Counters for the ADR-0008 Q4 falsifier (see `docs/memory-adaptation.adoc`).
///
/// Every counter is a count of claims delivered into a generation request, so a
/// claim the performer was *given*. The delivered set is exactly what
/// `retained_memory_context` returned, the same set the writer's own
/// `retrieved_used` counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryFalsifierMetrics {
    /// Entries the seed file loaded at startup.
    pub seed_entries_loaded: usize,
    /// Generation requests that carried at least one retained claim.
    pub requests_with_memory: u64,
    /// Seed-file claims delivered.
    pub seed_used: u64,
    /// Writer claims delivered whose text is also in the seed file, so a
    /// reloaded seed would have reproduced them.
    pub writer_used_reproducible_from_seed: u64,
    /// Writer claims delivered that the seed file does not contain: the claims
    /// only a persisted writer store could bring back.
    pub writer_only_used: u64,
    /// Requests that carried at least one such claim.
    pub requests_with_writer_only: u64,
    /// Operator exports served.
    pub exports: u64,
}

#[derive(Debug, Default)]
pub(crate) struct MemoryFalsifier {
    identity: Option<MemorySeedIdentity>,
    seed_folds: HashSet<String>,
    metrics: MemoryFalsifierMetrics,
}

impl MemoryFalsifier {
    pub(crate) fn note_seed(&mut self, seed: &MemorySeed) {
        self.identity = Some(seed.identity());
        self.seed_folds = seed.entries.iter().map(|e| fold(&e.claim)).collect();
        self.metrics.seed_entries_loaded = seed.entries.len();
    }

    pub(crate) fn identity(&self) -> Option<&MemorySeedIdentity> {
        self.identity.as_ref()
    }

    pub(crate) fn metrics(&self) -> MemoryFalsifierMetrics {
        self.metrics
    }

    pub(crate) fn note_export(&mut self) {
        self.metrics.exports += 1;
    }

    /// Classify one request's delivered claims by where they came from.
    pub(crate) fn note_delivered(&mut self, delivered: &[ThinkingContent]) {
        let mut any = false;
        let mut writer_only = false;
        for item in delivered {
            let Some(event_id) = item.event_id.as_deref() else {
                continue;
            };
            if event_id.starts_with(MEMORY_SEED_EVENT_PREFIX) {
                any = true;
                self.metrics.seed_used += 1;
            } else if event_id.starts_with(MEMORY_WRITER_EVENT_PREFIX) {
                any = true;
                if self.seed_folds.contains(&fold(&item.text)) {
                    self.metrics.writer_used_reproducible_from_seed += 1;
                } else {
                    self.metrics.writer_only_used += 1;
                    writer_only = true;
                }
            }
        }
        if any {
            self.metrics.requests_with_memory += 1;
        }
        if writer_only {
            self.metrics.requests_with_writer_only += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn examples(dir: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/memory-seeds")
            .join(dir)
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

    fn entry(kind: SeedKind, claim: &str) -> SeedEntry {
        SeedEntry {
            kind,
            claim: claim.to_owned(),
        }
    }

    #[test]
    fn repository_fixtures_agree_with_the_schema_classification() {
        assert!(!json_files("valid").is_empty());
        for path in json_files("valid") {
            MemorySeed::load(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
        // `invalid` fails the schema; `inconsistent` passes the schema and
        // fails a rule only the loader enforces. Both are rejected.
        for dir in ["invalid", "inconsistent"] {
            assert!(!json_files(dir).is_empty());
            for path in json_files(dir) {
                assert!(MemorySeed::load(&path).is_err(), "{}", path.display());
            }
        }
        assert!(matches!(
            MemorySeed::load(&examples("valid/missing.json")),
            Err(MemorySeedError::Io(_))
        ));
    }

    #[test]
    fn bounds_in_the_schema_match_the_loader_constants() {
        let schema: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/memory-seed.schema.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let entries = &schema["properties"]["entries"];
        assert_eq!(entries["maxItems"], MAX_SEED_ENTRIES);
        let item = &entries["items"]["properties"];
        assert_eq!(item["claim"]["maxLength"], MAX_SEED_CLAIM_BYTES);
        assert_eq!(
            schema["properties"]["schema_version"]["const"],
            MEMORY_SEED_SCHEMA_VERSION
        );
        let kinds: Vec<&str> = item["kind"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for kind in [
            SeedKind::PersonaFact,
            SeedKind::RunningJoke,
            SeedKind::Commitment,
            SeedKind::SegmentSummary,
        ] {
            assert!(kinds.contains(&kind.topic()), "{kind:?}");
            assert_eq!(SeedKind::from_topic(kind.topic()), Some(kind));
        }
        assert_eq!(kinds.len(), 4);
    }

    #[test]
    fn bounds_are_exact_and_refusals_are_typed() {
        let at_limit = MemorySeed::from_entries(
            "s",
            "1",
            vec![entry(
                SeedKind::PersonaFact,
                &"a".repeat(MAX_SEED_CLAIM_BYTES),
            )],
        );
        assert!(at_limit.is_ok());
        let over = MemorySeed::from_entries(
            "s",
            "1",
            vec![entry(
                SeedKind::PersonaFact,
                &"a".repeat(MAX_SEED_CLAIM_BYTES + 1),
            )],
        );
        assert!(matches!(over, Err(MemorySeedError::Invalid { .. })));

        let many = |n: usize| -> Vec<SeedEntry> {
            (0..n)
                .map(|i| entry(SeedKind::PersonaFact, &format!("fact {i}")))
                .collect()
        };
        assert!(MemorySeed::from_entries("s", "1", many(MAX_SEED_ENTRIES)).is_ok());
        assert!(matches!(
            MemorySeed::from_entries("s", "1", many(MAX_SEED_ENTRIES + 1)),
            Err(MemorySeedError::Invalid { .. })
        ));

        let huge = format!(
            "{}{}",
            MemorySeed::from_entries("s", "1", vec![])
                .unwrap()
                .to_json(),
            " ".repeat(MAX_SEED_FILE_BYTES)
        );
        assert!(matches!(
            MemorySeed::parse(&huge),
            Err(MemorySeedError::TooLarge { .. })
        ));
        for bad in [
            "{",
            r#"{"schema_version":"0.1.0","seed_id":"s","revision":"1","entries":[],"x":1}"#,
            r#"{"schema_version":"0.1.0","seed_id":"s","revision":"1","entries":[{"kind":"persona-fact","claim":"hi there","actor_id":"u1"}]}"#,
            r#"{"schema_version":"0.1.0","seed_id":"s","revision":"1","entries":[{"kind":"viewer-claim","claim":"hi there"}]}"#,
        ] {
            assert!(
                matches!(MemorySeed::parse(bad), Err(MemorySeedError::Parse(_))),
                "{bad}"
            );
        }
        assert!(matches!(
            MemorySeed::parse(
                r#"{"schema_version":"9","seed_id":"s","revision":"1","entries":[]}"#
            ),
            Err(MemorySeedError::Invalid { .. })
        ));
        for (id, rev) in [
            ("S", "1"),
            ("", "1"),
            ("s", ""),
            ("s", "a b"),
            ("s", &"r".repeat(33)[..]),
        ] {
            assert!(
                MemorySeed::from_entries(id, rev, vec![]).is_err(),
                "{id}/{rev}"
            );
        }
    }

    #[test]
    fn json_round_trip_and_hash_are_deterministic() {
        let seed = MemorySeed::load(&examples("valid/persona.json")).unwrap();
        let text = seed.to_json();
        assert_eq!(MemorySeed::parse(&text).unwrap(), seed);
        assert_eq!(MemorySeed::parse(&text).unwrap().to_json(), text);
        assert_eq!(seed.content_hash(), seed.content_hash());
        assert_eq!(seed.content_hash().len(), 64);

        // Whitespace in the file does not change identity; content does.
        let compact =
            serde_json::to_string(&serde_json::from_str::<serde_json::Value>(&text).unwrap())
                .unwrap();
        assert_eq!(
            MemorySeed::parse(&compact).unwrap().content_hash(),
            seed.content_hash()
        );
        let mut changed = seed.entries().to_vec();
        changed[0].claim.push('!');
        assert_ne!(
            MemorySeed::from_entries("example-persona", "1", changed)
                .unwrap()
                .content_hash(),
            seed.content_hash()
        );
        let reordered: Vec<_> = seed.entries().iter().rev().cloned().collect();
        assert_ne!(
            MemorySeed::from_entries("example-persona", "1", reordered)
                .unwrap()
                .content_hash(),
            seed.content_hash()
        );
    }

    #[test]
    fn export_revision_is_derived_from_the_entries() {
        let entries = vec![entry(SeedKind::Commitment, "I will play chess next stream")];
        let a = MemorySeed::for_export(entries.clone()).unwrap();
        let b = MemorySeed::for_export(entries).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_json(), b.to_json());
        assert!(a.revision().starts_with("export-"));
        let other = MemorySeed::for_export(vec![entry(
            SeedKind::Commitment,
            "I will play go next stream",
        )])
        .unwrap();
        assert_ne!(a.revision(), other.revision());
    }

    fn item(event_id: &str, text: &str) -> ThinkingContent {
        ThinkingContent {
            source: aivtuber_domain::ThinkingContextSource::Memory,
            event_id: Some(event_id.to_owned()),
            event_kind: None,
            source_class: None,
            trust_level: aivtuber_domain::TrustLevel::Trusted,
            privacy: aivtuber_domain::PrivacyClass::Public,
            epistemic_class: Some(aivtuber_domain::EpistemicClass::PerformerExperience),
            text: text.to_owned(),
        }
    }

    #[test]
    fn falsifier_separates_seed_reproducible_and_writer_only_claims() {
        let seed =
            MemorySeed::from_entries("s", "1", vec![entry(SeedKind::PersonaFact, "I love cats")])
                .unwrap();
        let mut falsifier = MemoryFalsifier::default();
        falsifier.note_seed(&seed);
        falsifier.note_delivered(&[
            item("memory-seed.1", "I love cats"),
            item("memory-writer.1", "  i LOVE cats "),
            item("memory-writer.2", "I will play chess next stream"),
            item("evt-1", "some unrelated memory"),
        ]);
        falsifier.note_delivered(&[item("memory-seed.1", "I love cats")]);
        falsifier.note_delivered(&[item("evt-1", "unrelated")]);
        let m = falsifier.metrics();
        assert_eq!(m.seed_entries_loaded, 1);
        assert_eq!(m.requests_with_memory, 2);
        assert_eq!(m.seed_used, 2);
        assert_eq!(m.writer_used_reproducible_from_seed, 1);
        assert_eq!(m.writer_only_used, 1);
        assert_eq!(m.requests_with_writer_only, 1);
    }
}
