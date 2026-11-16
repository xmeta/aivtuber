//! Strict inspector for an uploaded VRM model (issue #362).
//!
//! The file is **untrusted data** that two parsers will read: this one, which
//! decides whether it may be stored and what licence summary the operator sees,
//! and the renderer page (three.js plus three-vrm), which loads it. This parser
//! therefore accepts only a small, fully checked subset of glTF binary:
//!
//! * the 12-byte header: magic `glTF`, version 2, and a total length that equals
//!   the file length exactly (no truncation, no trailing bytes);
//! * a first chunk of type `JSON` whose length is 4-byte aligned, within the
//!   file and no larger than [`MAX_JSON_BYTES`];
//! * after it, at most one `BIN` chunk and unknown chunks (which the glTF spec
//!   says to ignore), each with an aligned length that stays in bounds, and at
//!   most [`MAX_CHUNKS`] chunks in total;
//! * a JSON text that is valid UTF-8 and parses (serde_json's nesting limit
//!   applies) with a `VRMC_vrm` (VRM 1.0) or `VRM` (VRM 0.x) extension that has
//!   a `meta` object.
//!
//! Only the JSON chunk is ever read into memory (bounded); binary chunks are
//! skipped by seeking, so a 64 MiB model costs a few kilobytes here. Every meta
//! value is untrusted text: only strings and booleans are taken, control
//! characters are removed, lengths are capped, and the page shows them with
//! `textContent`. Nothing in the meta is ever used as a path, a URL to fetch, or
//! a command.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::io::{Read, Seek, SeekFrom};

/// Largest accepted model file (the same ceiling the static server and the
/// renderer page use).
pub const MAX_VRM_BYTES: u64 = 64 * 1024 * 1024;
/// Largest accepted JSON chunk. Real models carry 100 KB to a few MB.
pub const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
/// Most chunks a file may hold (a real model has two).
pub const MAX_CHUNKS: usize = 16;
/// Longest kept value of a short meta field, in characters.
pub const MAX_META_FIELD_CHARS: usize = 128;
/// Longest kept URL / copyright / contact text, in characters.
pub const MAX_META_LONG_CHARS: usize = 256;
/// Most author names kept.
pub const MAX_AUTHORS: usize = 8;
/// Longest credit line, in characters.
pub const MAX_CREDIT_CHARS: usize = 200;

const MAGIC: u32 = 0x4654_6C67; // "glTF", little endian
const CHUNK_JSON: u32 = 0x4E4F_534A; // "JSON"
const CHUNK_BIN: u32 = 0x004E_4942; // "BIN\0"
const HEADER_BYTES: u64 = 12;
const CHUNK_HEADER_BYTES: u64 = 8;

/// Why a file was refused. Closed: each has a stable [`VrmError::code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VrmError {
    /// More than [`MAX_VRM_BYTES`].
    TooLarge,
    /// Shorter than a header plus one chunk header.
    Truncated,
    /// The first four bytes are not `glTF` (a renamed image, for example).
    NotGlb,
    /// glTF binary, but not version 2.
    UnsupportedVersion,
    /// The header's total length differs from the file length.
    LengthMismatch,
    /// The first chunk is not `JSON`.
    FirstChunkNotJson,
    /// A chunk length that is not a multiple of four.
    UnalignedChunk,
    /// A chunk that runs past the end of the file, or a half chunk header.
    ChunkOutOfBounds,
    /// A JSON chunk larger than [`MAX_JSON_BYTES`] (or empty).
    JsonTooLarge,
    /// More than [`MAX_CHUNKS`] chunks.
    TooManyChunks,
    /// A second `BIN` chunk.
    DuplicateBin,
    /// The JSON chunk is not UTF-8.
    JsonNotUtf8,
    /// The JSON chunk does not parse (or nests too deeply).
    JsonInvalid,
    /// Neither a `VRMC_vrm` (1.0) nor a `VRM` (0.x) extension.
    NotVrm,
    /// A VRM extension without a `meta` object.
    MissingMeta,
    /// The file could not be read.
    Unreadable,
}

impl VrmError {
    /// Stable machine-readable name.
    pub fn code(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::Truncated => "truncated",
            Self::NotGlb => "not_glb",
            Self::UnsupportedVersion => "unsupported_version",
            Self::LengthMismatch => "length_mismatch",
            Self::FirstChunkNotJson => "first_chunk_not_json",
            Self::UnalignedChunk => "unaligned_chunk",
            Self::ChunkOutOfBounds => "chunk_out_of_bounds",
            Self::JsonTooLarge => "json_too_large",
            Self::TooManyChunks => "too_many_chunks",
            Self::DuplicateBin => "duplicate_bin",
            Self::JsonNotUtf8 => "json_not_utf8",
            Self::JsonInvalid => "json_invalid",
            Self::NotVrm => "not_vrm",
            Self::MissingMeta => "missing_meta",
            Self::Unreadable => "unreadable",
        }
    }
}

impl fmt::Display for VrmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "the file is larger than 64 MiB",
            Self::Truncated => "the file is truncated",
            Self::NotGlb => "not a glTF binary (the glTF magic is missing)",
            Self::UnsupportedVersion => "not glTF binary version 2",
            Self::LengthMismatch => "the glTF header length does not match the file size",
            Self::FirstChunkNotJson => "the first glTF chunk is not JSON",
            Self::UnalignedChunk => "a glTF chunk length is not a multiple of four",
            Self::ChunkOutOfBounds => "a glTF chunk runs past the end of the file",
            Self::JsonTooLarge => "the glTF JSON chunk is empty or too large",
            Self::TooManyChunks => "the file has too many glTF chunks",
            Self::DuplicateBin => "the file has more than one binary chunk",
            Self::JsonNotUtf8 => "the glTF JSON chunk is not UTF-8",
            Self::JsonInvalid => "the glTF JSON chunk is not valid JSON",
            Self::NotVrm => "not a VRM model (no VRM 1.0 or 0.x extension)",
            Self::MissingMeta => "the VRM extension has no meta (licence) block",
            Self::Unreadable => "the file could not be read",
        })
    }
}

impl std::error::Error for VrmError {}

/// Which VRM generation the file declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VrmSpec {
    /// `VRMC_vrm` (VRM 1.0).
    Vrm1,
    /// `VRM` (VRM 0.x).
    Vrm0,
}

/// What the operator must be warned about before streaming with the model.
/// Closed: the page maps each to its own text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VrmWarning {
    /// Only the author may use the avatar.
    AuthorOnly,
    /// Only persons the author separately licensed may use the avatar.
    LicensedPersonsOnly,
    /// Commercial use is not allowed (non-profit personal use only).
    NonCommercialOnly,
    /// Commercial use is limited to an individual (not a company).
    PersonalCommercialOnly,
    /// The licence asks for a credit; see the credit text.
    CreditRequired,
    /// The meta does not state who may use the avatar or for what.
    LicenceNotStated,
}

/// The licence summary read from the model. Every string is bounded, free of
/// control characters and **untrusted**: show it as text, never as markup, a
/// link target or a path. Values keep the file's own spelling (the VRM 0.x
/// names `OnlyAuthor`, `Disallow`, ... differ from 1.0's `onlyAuthor`, ...);
/// [`Self::warnings`] is the normalized reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VrmMeta {
    pub spec: VrmSpec,
    pub name: Option<String>,
    pub version: Option<String>,
    pub authors: Vec<String>,
    pub copyright: Option<String>,
    pub contact: Option<String>,
    pub license_url: Option<String>,
    pub other_license_url: Option<String>,
    /// VRM 0.x `licenseName` (`CC_BY`, `Redistribution_Prohibited`, ...).
    pub license_name: Option<String>,
    /// 1.0 `avatarPermission`; 0.x `allowedUserName`.
    pub avatar_permission: Option<String>,
    /// 1.0 `commercialUsage`; 0.x `commercialUssageName`.
    pub commercial_usage: Option<String>,
    /// 1.0 `allowRedistribution`.
    pub allow_redistribution: Option<bool>,
    /// 1.0 `modification`.
    pub modification: Option<String>,
    /// 1.0 `creditNotation`.
    pub credit_notation: Option<String>,
    /// 1.0 `allowExcessivelyViolentUsage`; 0.x `violentUssageName`.
    pub allow_violent_usage: Option<bool>,
    /// 1.0 `allowExcessivelySexualUsage`; 0.x `sexualUssageName`.
    pub allow_sexual_usage: Option<bool>,
    /// 1.0 `allowPoliticalOrReligiousUsage`.
    pub allow_political_or_religious_usage: Option<bool>,
    /// 1.0 `allowAntisocialOrHateUsage`.
    pub allow_antisocial_or_hate_usage: Option<bool>,
    pub warnings: Vec<VrmWarning>,
    /// The credit line to show next to the stream when the licence asks for
    /// one: the model name and its authors. `None` when no credit is required
    /// or the file names neither.
    pub credit: Option<String>,
}

/// Reads a model file: `len` is its exact size (the caller took it from the
/// file system; it is compared with the header, never trusted from the header).
pub fn inspect<R: Read + Seek>(reader: &mut R, len: u64) -> Result<VrmMeta, VrmError> {
    if len > MAX_VRM_BYTES {
        return Err(VrmError::TooLarge);
    }
    if len < HEADER_BYTES + CHUNK_HEADER_BYTES {
        return Err(VrmError::Truncated);
    }
    let mut header = [0_u8; HEADER_BYTES as usize];
    read_exact_at(reader, 0, &mut header)?;
    check_header(&header, len)?;
    let mut position = HEADER_BYTES;
    let mut json: Option<Vec<u8>> = None;
    let mut saw_bin = false;
    let mut chunks = 0_usize;
    while position < len {
        if len - position < CHUNK_HEADER_BYTES {
            return Err(VrmError::ChunkOutOfBounds);
        }
        chunks += 1;
        if chunks > MAX_CHUNKS {
            return Err(VrmError::TooManyChunks);
        }
        let mut chunk_header = [0_u8; CHUNK_HEADER_BYTES as usize];
        read_exact_at(reader, position, &mut chunk_header)?;
        let chunk_len = u64::from(u32::from_le_bytes([
            chunk_header[0],
            chunk_header[1],
            chunk_header[2],
            chunk_header[3],
        ]));
        let chunk_type = u32::from_le_bytes([
            chunk_header[4],
            chunk_header[5],
            chunk_header[6],
            chunk_header[7],
        ]);
        if chunks == 1 && chunk_type != CHUNK_JSON {
            return Err(VrmError::FirstChunkNotJson);
        }
        if !chunk_len.is_multiple_of(4) {
            return Err(VrmError::UnalignedChunk);
        }
        let body = position + CHUNK_HEADER_BYTES;
        if chunk_len > len - body {
            return Err(VrmError::ChunkOutOfBounds);
        }
        if chunks == 1 {
            if chunk_len == 0 || chunk_len > MAX_JSON_BYTES as u64 {
                return Err(VrmError::JsonTooLarge);
            }
            // Bounded just above: at most MAX_JSON_BYTES is allocated.
            let mut buffer = vec![0_u8; chunk_len as usize];
            read_exact_at(reader, body, &mut buffer)?;
            json = Some(buffer);
        } else if chunk_type == CHUNK_BIN {
            if saw_bin {
                return Err(VrmError::DuplicateBin);
            }
            saw_bin = true;
        }
        position = body + chunk_len;
    }
    let json = json.ok_or(VrmError::Truncated)?;
    meta_from_json(&json)
}

/// Opens and inspects a file on disk.
pub fn inspect_file(path: &std::path::Path) -> Result<VrmMeta, VrmError> {
    let mut file = std::fs::File::open(path).map_err(|_| VrmError::Unreadable)?;
    let len = file.metadata().map_err(|_| VrmError::Unreadable)?.len();
    inspect(&mut file, len)
}

/// Checks the first bytes of an upload against its declared body length, so a
/// renamed image or a header that lies about its length is refused before the
/// rest of the body is read. `prefix` holds at least the first
/// [`HEADER_BYTES`] bytes.
pub fn check_prefix(prefix: &[u8], declared_len: u64) -> Result<(), VrmError> {
    if declared_len > MAX_VRM_BYTES {
        return Err(VrmError::TooLarge);
    }
    if declared_len < HEADER_BYTES + CHUNK_HEADER_BYTES {
        return Err(VrmError::Truncated);
    }
    let Some(header) = prefix.get(..HEADER_BYTES as usize) else {
        return Err(VrmError::Truncated);
    };
    check_header(header, declared_len)
}

fn check_header(header: &[u8], len: u64) -> Result<(), VrmError> {
    let word =
        |i: usize| u32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
    if word(0) != MAGIC {
        return Err(VrmError::NotGlb);
    }
    if word(4) != 2 {
        return Err(VrmError::UnsupportedVersion);
    }
    if u64::from(word(8)) != len {
        return Err(VrmError::LengthMismatch);
    }
    Ok(())
}

fn read_exact_at<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    buffer: &mut [u8],
) -> Result<(), VrmError> {
    reader
        .seek(SeekFrom::Start(offset))
        .and_then(|_| reader.read_exact(buffer))
        .map_err(|_| VrmError::Unreadable)
}

fn meta_from_json(bytes: &[u8]) -> Result<VrmMeta, VrmError> {
    let text = std::str::from_utf8(bytes).map_err(|_| VrmError::JsonNotUtf8)?;
    let root: Value = serde_json::from_str(text).map_err(|_| VrmError::JsonInvalid)?;
    let extensions = root.get("extensions").and_then(Value::as_object);
    if let Some(extension) = extensions.and_then(|e| e.get("VRMC_vrm")) {
        let meta = extension
            .get("meta")
            .and_then(Value::as_object)
            .ok_or(VrmError::MissingMeta)?;
        return Ok(vrm1_meta(meta));
    }
    if let Some(extension) = extensions.and_then(|e| e.get("VRM")) {
        let meta = extension
            .get("meta")
            .and_then(Value::as_object)
            .ok_or(VrmError::MissingMeta)?;
        return Ok(vrm0_meta(meta));
    }
    Err(VrmError::NotVrm)
}

type Object = serde_json::Map<String, Value>;

/// A string value with control characters removed (whitespace controls become
/// spaces), trimmed and cut to `max` characters; `None` for anything that is
/// not a non-empty string.
fn clean_text(value: Option<&Value>, max: usize) -> Option<String> {
    let text = value?.as_str()?;
    let cleaned: String = text
        .chars()
        .filter_map(|c| {
            if c.is_control() {
                c.is_whitespace().then_some(' ')
            } else {
                Some(c)
            }
        })
        .take(max.saturating_mul(2))
        .collect();
    let trimmed: String = cleaned.trim().chars().take(max).collect();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn field(meta: &Object, key: &str, max: usize) -> Option<String> {
    clean_text(meta.get(key), max)
}

/// VRM 0.x spells its booleans `Allow` / `Disallow`.
fn allow_word(value: Option<String>) -> Option<bool> {
    match value?.as_str() {
        "Allow" => Some(true),
        "Disallow" => Some(false),
        _ => None,
    }
}

fn vrm1_meta(meta: &Object) -> VrmMeta {
    let authors: Vec<String> = meta
        .get("authors")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| clean_text(Some(a), MAX_META_FIELD_CHARS))
                .take(MAX_AUTHORS)
                .collect()
        })
        .unwrap_or_default();
    let boolean = |key: &str| meta.get(key).and_then(Value::as_bool);
    let mut out = VrmMeta {
        spec: VrmSpec::Vrm1,
        name: field(meta, "name", MAX_META_FIELD_CHARS),
        version: field(meta, "version", MAX_META_FIELD_CHARS),
        authors,
        copyright: field(meta, "copyrightInformation", MAX_META_LONG_CHARS),
        contact: field(meta, "contactInformation", MAX_META_LONG_CHARS),
        license_url: field(meta, "licenseUrl", MAX_META_LONG_CHARS),
        other_license_url: field(meta, "otherLicenseUrl", MAX_META_LONG_CHARS),
        license_name: None,
        avatar_permission: field(meta, "avatarPermission", MAX_META_FIELD_CHARS),
        commercial_usage: field(meta, "commercialUsage", MAX_META_FIELD_CHARS),
        allow_redistribution: boolean("allowRedistribution"),
        modification: field(meta, "modification", MAX_META_FIELD_CHARS),
        credit_notation: field(meta, "creditNotation", MAX_META_FIELD_CHARS),
        allow_violent_usage: boolean("allowExcessivelyViolentUsage"),
        allow_sexual_usage: boolean("allowExcessivelySexualUsage"),
        allow_political_or_religious_usage: boolean("allowPoliticalOrReligiousUsage"),
        allow_antisocial_or_hate_usage: boolean("allowAntisocialOrHateUsage"),
        warnings: Vec::new(),
        credit: None,
    };
    finish(&mut out);
    out
}

fn vrm0_meta(meta: &Object) -> VrmMeta {
    // 0.x has one `author` string; split nothing, keep it as the single author.
    let authors: Vec<String> = field(meta, "author", MAX_META_FIELD_CHARS)
        .into_iter()
        .collect();
    let mut out = VrmMeta {
        spec: VrmSpec::Vrm0,
        name: field(meta, "title", MAX_META_FIELD_CHARS),
        version: field(meta, "version", MAX_META_FIELD_CHARS),
        authors,
        copyright: None,
        contact: field(meta, "contactInformation", MAX_META_LONG_CHARS),
        license_url: field(meta, "otherPermissionUrl", MAX_META_LONG_CHARS),
        other_license_url: field(meta, "otherLicenseUrl", MAX_META_LONG_CHARS),
        license_name: field(meta, "licenseName", MAX_META_FIELD_CHARS),
        avatar_permission: field(meta, "allowedUserName", MAX_META_FIELD_CHARS),
        commercial_usage: field(meta, "commercialUssageName", MAX_META_FIELD_CHARS),
        allow_redistribution: None,
        modification: None,
        credit_notation: None,
        allow_violent_usage: allow_word(field(meta, "violentUssageName", MAX_META_FIELD_CHARS)),
        allow_sexual_usage: allow_word(field(meta, "sexualUssageName", MAX_META_FIELD_CHARS)),
        allow_political_or_religious_usage: None,
        allow_antisocial_or_hate_usage: None,
        warnings: Vec::new(),
        credit: None,
    };
    finish(&mut out);
    out
}

/// Derives the warnings and the credit line from the (already bounded) fields.
fn finish(meta: &mut VrmMeta) {
    let permission = meta.avatar_permission.as_deref();
    let commercial = meta.commercial_usage.as_deref();
    let licence = meta.license_name.as_deref().unwrap_or("");
    let mut warnings = Vec::new();
    if matches!(permission, Some("onlyAuthor" | "OnlyAuthor")) {
        warnings.push(VrmWarning::AuthorOnly);
    }
    if matches!(
        permission,
        Some("onlySeparatelyLicensedPerson" | "ExplicitlyLicensedPerson")
    ) {
        warnings.push(VrmWarning::LicensedPersonsOnly);
    }
    if matches!(commercial, Some("personalNonProfit" | "Disallow")) || licence.contains("_NC") {
        warnings.push(VrmWarning::NonCommercialOnly);
    } else if matches!(commercial, Some("personalProfit")) {
        warnings.push(VrmWarning::PersonalCommercialOnly);
    }
    // Every Creative Commons "BY" variant asks for attribution; CC0 does not.
    let credit_required =
        meta.credit_notation.as_deref() == Some("required") || licence.starts_with("CC_BY");
    if credit_required {
        warnings.push(VrmWarning::CreditRequired);
    }
    if permission.is_none() || commercial.is_none() {
        warnings.push(VrmWarning::LicenceNotStated);
    }
    meta.warnings = warnings;
    if credit_required {
        let by = meta.authors.join(", ");
        let line = match (meta.name.as_deref(), by.is_empty()) {
            (Some(name), false) => Some(format!("{name} / {by}")),
            (Some(name), true) => Some(name.to_owned()),
            (None, false) => Some(by),
            (None, true) => None,
        };
        meta.credit = line.map(|l| l.chars().take(MAX_CREDIT_CHARS).collect());
    }
}

/// A minimal valid VRM 1.0 model around `json`, for tests in this crate.
#[cfg(test)]
pub(crate) fn test_model(json: &str) -> Vec<u8> {
    fn chunk(kind: u32, body: &[u8]) -> Vec<u8> {
        let mut padded = body.to_vec();
        while !padded.len().is_multiple_of(4) {
            padded.push(if kind == CHUNK_JSON { b' ' } else { 0 });
        }
        let mut out = (padded.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&padded);
        out
    }
    let chunks = [
        chunk(CHUNK_JSON, json.as_bytes()),
        chunk(CHUNK_BIN, &[1, 2, 3]),
    ];
    let total = 12 + chunks.iter().map(Vec::len).sum::<usize>();
    let mut out = MAGIC.to_le_bytes().to_vec();
    out.extend_from_slice(&2_u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    for c in &chunks {
        out.extend_from_slice(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn chunk(kind: u32, body: &[u8]) -> Vec<u8> {
        let mut padded = body.to_vec();
        while !padded.len().is_multiple_of(4) {
            padded.push(if kind == CHUNK_JSON { b' ' } else { 0 });
        }
        let mut out = (padded.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&padded);
        out
    }

    fn glb(chunks: &[Vec<u8>]) -> Vec<u8> {
        let total = 12 + chunks.iter().map(Vec::len).sum::<usize>();
        let mut out = MAGIC.to_le_bytes().to_vec();
        out.extend_from_slice(&2_u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        for c in chunks {
            out.extend_from_slice(c);
        }
        out
    }

    fn model(json: &str) -> Vec<u8> {
        glb(&[
            chunk(CHUNK_JSON, json.as_bytes()),
            chunk(CHUNK_BIN, &[1, 2, 3]),
        ])
    }

    fn run(bytes: &[u8]) -> Result<VrmMeta, VrmError> {
        inspect(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    const V1: &str = r#"{"asset":{"version":"2.0"},"extensions":{"VRMC_vrm":{"specVersion":"1.0","meta":{
        "name":"Test Girl","version":"1.2","authors":["Ann","Bob"],"copyrightInformation":"(c) Ann",
        "contactInformation":"ann@example.invalid","licenseUrl":"https://vrm.dev/licenses/1.0/",
        "avatarPermission":"everyone","commercialUsage":"personalNonProfit","creditNotation":"required",
        "allowRedistribution":false,"modification":"prohibited",
        "allowExcessivelyViolentUsage":false,"allowExcessivelySexualUsage":false,
        "allowPoliticalOrReligiousUsage":true,"allowAntisocialOrHateUsage":false}}}}"#;

    const V0: &str = r#"{"extensions":{"VRM":{"exporterVersion":"UniVRM-0.99","meta":{
        "title":"Old Model","version":"0.9","author":"Carol","contactInformation":"c@example.invalid",
        "allowedUserName":"OnlyAuthor","violentUssageName":"Disallow","sexualUssageName":"Allow",
        "commercialUssageName":"Disallow","otherPermissionUrl":"https://example.invalid/permission",
        "licenseName":"CC_BY_NC","otherLicenseUrl":"https://example.invalid/licence"}}}}"#;

    #[test]
    fn a_vrm1_meta_is_read_field_by_field() {
        let meta = run(&model(V1)).unwrap();
        assert_eq!(meta.spec, VrmSpec::Vrm1);
        assert_eq!(meta.name.as_deref(), Some("Test Girl"));
        assert_eq!(meta.version.as_deref(), Some("1.2"));
        assert_eq!(meta.authors, ["Ann", "Bob"]);
        assert_eq!(meta.copyright.as_deref(), Some("(c) Ann"));
        assert_eq!(meta.contact.as_deref(), Some("ann@example.invalid"));
        assert_eq!(
            meta.license_url.as_deref(),
            Some("https://vrm.dev/licenses/1.0/")
        );
        assert_eq!(meta.avatar_permission.as_deref(), Some("everyone"));
        assert_eq!(meta.commercial_usage.as_deref(), Some("personalNonProfit"));
        assert_eq!(meta.allow_redistribution, Some(false));
        assert_eq!(meta.modification.as_deref(), Some("prohibited"));
        assert_eq!(meta.credit_notation.as_deref(), Some("required"));
        assert_eq!(meta.allow_violent_usage, Some(false));
        assert_eq!(meta.allow_political_or_religious_usage, Some(true));
        assert_eq!(
            meta.warnings,
            [VrmWarning::NonCommercialOnly, VrmWarning::CreditRequired]
        );
        assert_eq!(meta.credit.as_deref(), Some("Test Girl / Ann, Bob"));
    }

    #[test]
    fn a_vrm0_meta_is_read_with_its_own_spellings() {
        let meta = run(&model(V0)).unwrap();
        assert_eq!(meta.spec, VrmSpec::Vrm0);
        assert_eq!(meta.name.as_deref(), Some("Old Model"));
        assert_eq!(meta.authors, ["Carol"]);
        assert_eq!(meta.avatar_permission.as_deref(), Some("OnlyAuthor"));
        assert_eq!(meta.commercial_usage.as_deref(), Some("Disallow"));
        assert_eq!(meta.license_name.as_deref(), Some("CC_BY_NC"));
        assert_eq!(
            meta.license_url.as_deref(),
            Some("https://example.invalid/permission")
        );
        assert_eq!(
            meta.other_license_url.as_deref(),
            Some("https://example.invalid/licence")
        );
        assert_eq!(meta.allow_violent_usage, Some(false));
        assert_eq!(meta.allow_sexual_usage, Some(true));
        assert_eq!(
            meta.warnings,
            [
                VrmWarning::AuthorOnly,
                VrmWarning::NonCommercialOnly,
                VrmWarning::CreditRequired
            ]
        );
        assert_eq!(meta.credit.as_deref(), Some("Old Model / Carol"));
    }

    #[test]
    fn a_permissive_model_has_no_warning_and_no_credit() {
        let json = r#"{"extensions":{"VRMC_vrm":{"meta":{"name":"Free","authors":["Dee"],
            "avatarPermission":"everyone","commercialUsage":"corporation","creditNotation":"unnecessary"}}}}"#;
        let meta = run(&model(json)).unwrap();
        assert!(meta.warnings.is_empty(), "{:?}", meta.warnings);
        assert_eq!(meta.credit, None);
    }

    #[test]
    fn the_restrictive_values_each_warn() {
        let warn = |permission: &str, commercial: &str| {
            let json = format!(
                r#"{{"extensions":{{"VRMC_vrm":{{"meta":{{"avatarPermission":"{permission}","commercialUsage":"{commercial}"}}}}}}}}"#
            );
            run(&model(&json)).unwrap().warnings
        };
        assert_eq!(warn("onlyAuthor", "corporation"), [VrmWarning::AuthorOnly]);
        assert_eq!(
            warn("onlySeparatelyLicensedPerson", "corporation"),
            [VrmWarning::LicensedPersonsOnly]
        );
        assert_eq!(
            warn("everyone", "personalNonProfit"),
            [VrmWarning::NonCommercialOnly]
        );
        assert_eq!(
            warn("everyone", "personalProfit"),
            [VrmWarning::PersonalCommercialOnly]
        );
    }

    #[test]
    fn a_missing_permission_is_reported_as_not_stated() {
        let json = r#"{"extensions":{"VRMC_vrm":{"meta":{"name":"Silent"}}}}"#;
        assert_eq!(
            run(&model(json)).unwrap().warnings,
            [VrmWarning::LicenceNotStated]
        );
    }

    #[test]
    fn meta_text_is_untrusted_and_bounded() {
        let long = "あ".repeat(10_000);
        let json = format!(
            r#"{{"extensions":{{"VRMC_vrm":{{"meta":{{"name":"a\u0000b\u001b[31m\n<script>x</script>","authors":["{long}","","x"],
            "licenseUrl":"{long}","avatarPermission":7,"allowRedistribution":"yes","unknownField":{{"a":1}}}}}}}}}}"#
        );
        let meta = run(&model(&json)).unwrap();
        let name = meta.name.unwrap();
        assert!(!name.chars().any(char::is_control), "{name:?}");
        assert_eq!(name, "ab[31m <script>x</script>");
        assert_eq!(meta.authors[0].chars().count(), MAX_META_FIELD_CHARS);
        assert_eq!(meta.authors[1], "x", "an empty author is dropped");
        assert_eq!(
            meta.license_url.unwrap().chars().count(),
            MAX_META_LONG_CHARS
        );
        assert_eq!(meta.avatar_permission, None, "a number is not text");
        assert_eq!(meta.allow_redistribution, None, "a string is not a boolean");
    }

    #[test]
    fn at_most_eight_authors_are_kept() {
        let authors: Vec<String> = (0..30).map(|i| format!("\"a{i}\"")).collect();
        let json = format!(
            r#"{{"extensions":{{"VRMC_vrm":{{"meta":{{"authors":[{}]}}}}}}}}"#,
            authors.join(",")
        );
        assert_eq!(run(&model(&json)).unwrap().authors.len(), MAX_AUTHORS);
    }

    #[test]
    fn the_credit_line_is_bounded() {
        let long = "x".repeat(120);
        let json = format!(
            r#"{{"extensions":{{"VRMC_vrm":{{"meta":{{"name":"{long}","authors":["{long}"],"creditNotation":"required"}}}}}}}}"#
        );
        let credit = run(&model(&json)).unwrap().credit.unwrap();
        assert_eq!(credit.chars().count(), MAX_CREDIT_CHARS);
    }

    #[test]
    fn json_padding_and_unknown_chunks_are_accepted() {
        let bytes = glb(&[
            chunk(CHUNK_JSON, V1.as_bytes()),
            chunk(0x1234_5678, &[9; 8]),
            chunk(CHUNK_BIN, &[1; 16]),
        ]);
        assert!(run(&bytes).is_ok());
        // BIN is optional.
        assert!(run(&glb(&[chunk(CHUNK_JSON, V1.as_bytes())])).is_ok());
    }

    #[test]
    fn not_a_vrm_and_missing_meta_are_typed() {
        let plain = r#"{"asset":{"version":"2.0"}}"#;
        assert_eq!(run(&model(plain)).unwrap_err(), VrmError::NotVrm);
        let other = r#"{"extensions":{"KHR_materials_unlit":{}}}"#;
        assert_eq!(run(&model(other)).unwrap_err(), VrmError::NotVrm);
        let no_meta1 = r#"{"extensions":{"VRMC_vrm":{"specVersion":"1.0"}}}"#;
        assert_eq!(run(&model(no_meta1)).unwrap_err(), VrmError::MissingMeta);
        let no_meta0 = r#"{"extensions":{"VRM":{"meta":"text"}}}"#;
        assert_eq!(run(&model(no_meta0)).unwrap_err(), VrmError::MissingMeta);
    }

    #[test]
    fn a_renamed_image_and_other_wrong_magic_are_refused() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.resize(4096, 0);
        assert_eq!(run(&png).unwrap_err(), VrmError::NotGlb);
        let mut bytes = model(V1);
        bytes[0] = b'G';
        assert_eq!(run(&bytes).unwrap_err(), VrmError::NotGlb);
        let mut zip = b"PK\x03\x04".to_vec();
        zip.resize(64, 0);
        assert_eq!(run(&zip).unwrap_err(), VrmError::NotGlb);
    }

    #[test]
    fn a_wrong_version_is_refused() {
        for version in [0_u32, 1, 3, u32::MAX] {
            let mut bytes = model(V1);
            bytes[4..8].copy_from_slice(&version.to_le_bytes());
            assert_eq!(run(&bytes).unwrap_err(), VrmError::UnsupportedVersion);
        }
    }

    #[test]
    fn every_truncation_is_refused_and_never_panics() {
        let bytes = model(V1);
        for cut in 0..bytes.len() {
            let result = run(&bytes[..cut]);
            assert!(result.is_err(), "cut at {cut} was accepted");
        }
        assert!(run(&bytes).is_ok());
        assert_eq!(run(&bytes[..10]).unwrap_err(), VrmError::Truncated);
        // A cut keeps the header's old total length: the mismatch is the error.
        assert_eq!(
            run(&bytes[..bytes.len() - 4]).unwrap_err(),
            VrmError::LengthMismatch
        );
    }

    #[test]
    fn trailing_bytes_and_a_lying_total_are_refused() {
        let mut bytes = model(V1);
        bytes.extend_from_slice(&[0; 4]);
        assert_eq!(run(&bytes).unwrap_err(), VrmError::LengthMismatch);
        let mut short = model(V1);
        let total = short.len() as u32 - 4;
        short[8..12].copy_from_slice(&total.to_le_bytes());
        assert_eq!(run(&short).unwrap_err(), VrmError::LengthMismatch);
        let mut huge = model(V1);
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(run(&huge).unwrap_err(), VrmError::LengthMismatch);
    }

    #[test]
    fn a_chunk_that_lies_about_its_length_is_refused() {
        // The JSON chunk claims more than the file holds.
        let mut bytes = model(V1);
        bytes[12..16].copy_from_slice(&(u32::MAX - 3).to_le_bytes());
        assert_eq!(run(&bytes).unwrap_err(), VrmError::ChunkOutOfBounds);
        let mut bytes = model(V1);
        let claimed = (bytes.len() as u32) & !3; // aligned, but past the end
        bytes[12..16].copy_from_slice(&claimed.to_le_bytes());
        assert_eq!(run(&bytes).unwrap_err(), VrmError::ChunkOutOfBounds);
        // The BIN chunk does.
        let json = chunk(CHUNK_JSON, V1.as_bytes());
        let mut bin = chunk(CHUNK_BIN, &[0; 8]);
        bin[0..4].copy_from_slice(&1_000_u32.to_le_bytes());
        assert_eq!(
            run(&glb(&[json.clone(), bin])).unwrap_err(),
            VrmError::ChunkOutOfBounds
        );
        // Unaligned.
        let mut odd = chunk(CHUNK_BIN, &[0; 8]);
        odd[0..4].copy_from_slice(&5_u32.to_le_bytes());
        assert_eq!(
            run(&glb(&[json, odd])).unwrap_err(),
            VrmError::UnalignedChunk
        );
    }

    #[test]
    fn a_huge_json_length_is_refused_without_allocating_it() {
        // Claims a 1 GiB JSON chunk inside a tiny file: refused before any read
        // of that size.
        let mut bytes = glb(&[chunk(CHUNK_JSON, b"{}  ")]);
        bytes[12..16].copy_from_slice(&(1_u32 << 30).to_le_bytes());
        assert_eq!(run(&bytes).unwrap_err(), VrmError::ChunkOutOfBounds);
        // Within the file but over the JSON cap.
        let big = vec![b' '; MAX_JSON_BYTES + 4];
        let bytes = glb(&[chunk(CHUNK_JSON, &big)]);
        assert_eq!(run(&bytes).unwrap_err(), VrmError::JsonTooLarge);
        // Empty.
        let bytes = glb(&[chunk(CHUNK_JSON, b""), chunk(CHUNK_BIN, &[0; 4])]);
        assert_eq!(run(&bytes).unwrap_err(), VrmError::JsonTooLarge);
    }

    #[test]
    fn the_first_chunk_must_be_json_and_a_second_bin_is_refused() {
        let bytes = glb(&[chunk(CHUNK_BIN, &[0; 4]), chunk(CHUNK_JSON, V1.as_bytes())]);
        assert_eq!(run(&bytes).unwrap_err(), VrmError::FirstChunkNotJson);
        let bytes = glb(&[
            chunk(CHUNK_JSON, V1.as_bytes()),
            chunk(CHUNK_BIN, &[0; 4]),
            chunk(CHUNK_BIN, &[0; 4]),
        ]);
        assert_eq!(run(&bytes).unwrap_err(), VrmError::DuplicateBin);
    }

    #[test]
    fn too_many_chunks_and_a_half_chunk_header_are_refused() {
        let mut chunks = vec![chunk(CHUNK_JSON, V1.as_bytes())];
        chunks.extend((0..MAX_CHUNKS).map(|_| chunk(0x1111_1111, &[0; 4])));
        assert_eq!(run(&glb(&chunks)).unwrap_err(), VrmError::TooManyChunks);
        // Four stray bytes after the last chunk cannot be a chunk header.
        let mut bytes = glb(&[chunk(CHUNK_JSON, V1.as_bytes())]);
        bytes.extend_from_slice(&[0; 4]);
        let total = bytes.len() as u32;
        bytes[8..12].copy_from_slice(&total.to_le_bytes());
        assert_eq!(run(&bytes).unwrap_err(), VrmError::ChunkOutOfBounds);
    }

    #[test]
    fn json_that_is_not_utf8_or_not_json_is_refused() {
        let bytes = glb(&[chunk(CHUNK_JSON, &[0xFF, 0xFE, b'{', b'}'])]);
        assert_eq!(run(&bytes).unwrap_err(), VrmError::JsonNotUtf8);
        let bytes = glb(&[chunk(CHUNK_JSON, b"{\"a\": nope}")]);
        assert_eq!(run(&bytes).unwrap_err(), VrmError::JsonInvalid);
        // Nesting beyond the parser's recursion limit.
        let deep = format!("{}{}", "[".repeat(5_000), "]".repeat(5_000));
        let bytes = glb(&[chunk(CHUNK_JSON, deep.as_bytes())]);
        assert_eq!(run(&bytes).unwrap_err(), VrmError::JsonInvalid);
    }

    #[test]
    fn a_file_over_the_cap_is_refused_before_any_read() {
        let mut empty = Cursor::new(Vec::<u8>::new());
        assert_eq!(
            inspect(&mut empty, MAX_VRM_BYTES + 1).unwrap_err(),
            VrmError::TooLarge
        );
    }

    #[test]
    fn the_prefix_check_refuses_before_the_body_is_read() {
        let bytes = model(V1);
        assert!(check_prefix(&bytes[..12], bytes.len() as u64).is_ok());
        assert_eq!(
            check_prefix(&bytes[..12], bytes.len() as u64 + 4).unwrap_err(),
            VrmError::LengthMismatch
        );
        assert_eq!(
            check_prefix(b"\x89PNG\r\n\x1a\n\0\0\0\0", 4096).unwrap_err(),
            VrmError::NotGlb
        );
        assert_eq!(
            check_prefix(&bytes[..11], 4096).unwrap_err(),
            VrmError::Truncated
        );
        assert_eq!(
            check_prefix(&bytes[..12], MAX_VRM_BYTES + 1).unwrap_err(),
            VrmError::TooLarge
        );
    }

    #[test]
    fn mutated_headers_never_panic() {
        // Deterministic fuzz: every single-byte change in the header region and
        // the first chunk header, with a few values each.
        let bytes = model(V1);
        for index in 0..20 {
            for value in [0_u8, 1, 0x7F, 0x80, 0xFF] {
                let mut mutated = bytes.clone();
                mutated[index] = value;
                let _ = run(&mutated);
            }
        }
    }

    #[test]
    fn the_codes_are_stable_and_distinct() {
        let all = [
            VrmError::TooLarge,
            VrmError::Truncated,
            VrmError::NotGlb,
            VrmError::UnsupportedVersion,
            VrmError::LengthMismatch,
            VrmError::FirstChunkNotJson,
            VrmError::UnalignedChunk,
            VrmError::ChunkOutOfBounds,
            VrmError::JsonTooLarge,
            VrmError::TooManyChunks,
            VrmError::DuplicateBin,
            VrmError::JsonNotUtf8,
            VrmError::JsonInvalid,
            VrmError::NotVrm,
            VrmError::MissingMeta,
            VrmError::Unreadable,
        ];
        let codes: std::collections::BTreeSet<_> = all.iter().map(|e| e.code()).collect();
        assert_eq!(codes.len(), all.len());
        assert!(all.iter().all(|e| !e.to_string().is_empty()));
    }
}
