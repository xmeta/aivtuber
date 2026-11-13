//! The VRM avatar choice for the `local` profile (issue #362).
//!
//! The operator uploads a `.vrm` in the operation panel, reads the licence the
//! file declares, confirms, and the avatar page switches to it live. This module
//! owns the state behind that, next to [`crate::voice_control`] and
//! [`crate::chat_ingress`]:
//!
//! * **The file is untrusted data.** The panel's HTTP layer streams the upload
//!   into `incoming/` under a name it made up, hashes it and inspects it as a VRM
//!   (`aivtuber_adapters::local_avatar::vrm`: glTF header, chunk bounds, a
//!   bounded JSON chunk, a VRM extension with a meta block). This module
//!   inspects it again before it takes it, because it is the one that decides
//!   what the avatar page will be served. The renderer page is the second
//!   parser; nothing here makes a model safe for it, so the threat model lists
//!   it as a parser of untrusted input.
//! * **Names, never paths.** A model is named by its SHA-256 (or `sample`).
//!   Every path here is built from the hash and the directories the launcher
//!   gave: a request cannot name a file.
//! * **Candidate, then active.** An upload becomes the single *pending*
//!   candidate (`pending/<hash>.vrm`) with its licence summary. Only an
//!   authenticated `confirm` (the operator has read the licence) moves it into
//!   `models/<hash>.vrm` and makes it active. At most [`MAX_STORED_MODELS`] are
//!   kept; adding one evicts the least recently used model that is not active.
//!   The shipped sample stays a choice and is never stored or deleted here.
//! * **Live, persisted.** The active choice is told to the avatar server
//!   ([`LocalAvatarHandle::set_active_model`]); the page notices on its next
//!   poll and reloads. The choice is one small settings file replaced
//!   atomically; a missing, oversized or invalid file means the sample.
//! * **Authority.** The only entry points are the `ProductionApp` methods that
//!   demand `avatar.configure`, minted by `LocalControlIngress` and audited like
//!   stop and mute. Audit details hold the model name (bounded, untrusted text)
//!   and the content hash, never file bytes.
//!
//! Nothing here blocks the composition loop beyond small file operations (a
//! rename, a header read): the upload's 64 MiB is streamed on the panel's
//! connection thread.

use crate::voice_control::write_atomic;
use aivtuber_adapters::LocalAvatarHandle;
use aivtuber_adapters::vrm::{self, MAX_VRM_BYTES, VrmMeta, VrmWarning};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const AVATAR_SETTINGS_VAR: &str = "AIVTUBER_AVATAR_SETTINGS";
pub const AVATAR_DIR_VAR: &str = "AIVTUBER_AVATAR_DIR";
/// Most uploaded models kept (the sample is extra).
pub const MAX_STORED_MODELS: usize = 5;
/// The choice that means the model shipped with the page.
pub const SAMPLE_ID: &str = "sample";
const SETTINGS_SCHEMA: &str = "local-avatar-settings/1";
/// A settings file larger than this is ignored, not read.
const MAX_SETTINGS_BYTES: u64 = 2048;
/// Most recently used hashes remembered (for eviction order).
const MAX_RECENT: usize = 8;
/// Most files looked at in a directory scan.
const SCAN_LIMIT: usize = 64;

/// Where the avatar store and its choice live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvatarEnvironment {
    /// The settings file (`local-avatar.json`).
    pub settings: PathBuf,
    /// Holds `incoming/`, `pending/` and `models/`; created if missing.
    pub dir: PathBuf,
    /// The sample the page directory ships (`model.vrm`), if it is there.
    pub sample: Option<PathBuf>,
}

impl AvatarEnvironment {
    pub fn staging_dir(&self) -> PathBuf {
        self.dir.join("incoming")
    }

    fn pending_dir(&self) -> PathBuf {
        self.dir.join("pending")
    }

    fn models_dir(&self) -> PathBuf {
        self.dir.join("models")
    }
}

/// One model as the operator surfaces show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvatarModelInfo {
    /// The content hash (SHA-256, lowercase hex).
    pub id: String,
    pub size: u64,
    pub name: Option<String>,
    pub warnings: Vec<VrmWarning>,
    /// The full licence summary read from the file. Only the pending candidate,
    /// the active model and the shipped sample (when active) carry it, so the
    /// status stays small however many models are stored; `None` otherwise (and
    /// for a sample whose meta could not be read).
    pub meta: Option<VrmMeta>,
}

impl AvatarModelInfo {
    fn new(id: String, size: u64, meta: Option<VrmMeta>) -> Self {
        Self {
            id,
            size,
            name: meta.as_ref().and_then(|m| m.name.clone()),
            warnings: meta
                .as_ref()
                .map(|m| m.warnings.clone())
                .unwrap_or_default(),
            meta,
        }
    }

    fn brief(&self) -> Self {
        Self {
            meta: None,
            ..self.clone()
        }
    }
}

/// The avatar choice for the operator surfaces. Bounded: at most
/// [`MAX_STORED_MODELS`] + 2 models, each with bounded text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvatarStatus {
    /// `sample`, or the hash of the active stored model.
    pub active: String,
    /// The hash of the file the avatar page is told to show.
    pub active_id: Option<String>,
    /// The active model's credit line, when its licence asks for one.
    pub credit: Option<String>,
    pub sample: Option<AvatarModelInfo>,
    pub models: Vec<AvatarModelInfo>,
    /// The uploaded model waiting for the operator's confirmation.
    pub pending: Option<AvatarModelInfo>,
    pub max_models: usize,
    pub max_upload_bytes: u64,
    /// The hash the connected avatar page says it loaded. When it differs from
    /// `active_id` the page could not show the active model.
    pub renderer_model: Option<String>,
    pub last_error: Option<String>,
    pub save_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Choice {
    Sample,
    Stored(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    schema: String,
    active: String,
    #[serde(default)]
    recent: Vec<String>,
}

pub struct AvatarController {
    env: AvatarEnvironment,
    handle: LocalAvatarHandle,
    active: Choice,
    /// Most recently used first; at most [`MAX_RECENT`].
    recent: Vec<String>,
    sample: Option<AvatarModelInfo>,
    models: Vec<AvatarModelInfo>,
    pending: Option<AvatarModelInfo>,
    last_error: Option<String>,
    save_error: Option<String>,
}

impl std::fmt::Debug for AvatarController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AvatarController")
            .field("active", &self.active)
            .field("models", &self.models.len())
            .finish_non_exhaustive()
    }
}

fn is_full_hash(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_VRM_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "larger than the model size limit",
            ));
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Deletes the regular files in `dir` (bounded); a directory is left alone.
fn sweep(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten().take(SCAN_LIMIT) {
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

fn display_name(info: &AvatarModelInfo) -> String {
    info.name.clone().unwrap_or_else(|| "(unnamed)".to_owned())
}

/// Reads the settings file: `None` unless it is exactly the documented shape.
fn read_settings(path: &Path) -> Option<(Choice, Vec<String>)> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return None;
    }
    let parsed: SettingsFile = serde_json::from_slice(&bytes).ok()?;
    if parsed.schema != SETTINGS_SCHEMA || parsed.recent.len() > MAX_RECENT {
        return None;
    }
    if !parsed.recent.iter().all(|h| is_full_hash(h)) {
        return None;
    }
    let choice = if parsed.active == SAMPLE_ID {
        Choice::Sample
    } else if is_full_hash(&parsed.active) {
        Choice::Stored(parsed.active)
    } else {
        return None;
    };
    Some((choice, parsed.recent))
}

impl AvatarController {
    /// Opens the store. Leftovers of an interrupted upload are cleared, every
    /// stored model is inspected again (one that no longer parses is ignored),
    /// the sample is hashed once, and the saved choice is applied, falling back
    /// to the sample when it is missing or names a model that is gone.
    pub fn new(env: AvatarEnvironment, handle: LocalAvatarHandle) -> Result<Self, String> {
        for dir in [env.staging_dir(), env.pending_dir(), env.models_dir()] {
            fs::create_dir_all(&dir).map_err(|e| {
                format!("cannot create the avatar directory {}: {e}", dir.display())
            })?;
        }
        sweep(&env.staging_dir());
        sweep(&env.pending_dir());
        let mut models = Vec::new();
        if let Ok(entries) = fs::read_dir(env.models_dir()) {
            for entry in entries.flatten().take(SCAN_LIMIT) {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(hash) = name.strip_suffix(".vrm").filter(|h| is_full_hash(h)) else {
                    continue;
                };
                let path = entry.path();
                let (Ok(meta), Ok(size)) = (
                    vrm::inspect_file(&path),
                    fs::metadata(&path).map(|m| m.len()),
                ) else {
                    eprintln!(
                        "avatar: ignoring stored model {} (it no longer parses)",
                        short(hash)
                    );
                    continue;
                };
                models.push(AvatarModelInfo::new(hash.to_owned(), size, Some(meta)));
            }
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        let sample = env.sample.as_deref().and_then(|path| {
            let size = fs::metadata(path).ok()?.len();
            let id = sha256_file(path).ok()?;
            Some(AvatarModelInfo::new(id, size, vrm::inspect_file(path).ok()))
        });
        let (mut active, recent) =
            read_settings(&env.settings).unwrap_or((Choice::Sample, Vec::new()));
        if let Choice::Stored(hash) = &active
            && !models.iter().any(|m| &m.id == hash)
        {
            eprintln!(
                "avatar: the saved model {} is not in the store; showing the sample",
                short(hash)
            );
            active = Choice::Sample;
        }
        let mut controller = Self {
            env,
            handle,
            active,
            recent,
            sample,
            models,
            pending: None,
            last_error: None,
            save_error: None,
        };
        controller.evict();
        controller.apply();
        Ok(controller)
    }

    /// The directory the panel stages uploads in.
    pub fn staging_dir(&self) -> PathBuf {
        self.env.staging_dir()
    }

    /// The active model, for the startup disclosure line.
    pub fn active_summary(&self) -> String {
        match self.active_info() {
            Some(info) => format!(
                "{} (sha256 {}){}",
                display_name(info),
                short(&info.id),
                info.meta
                    .as_ref()
                    .and_then(|m| m.credit.as_deref())
                    .map(|c| format!("; credit: {c}"))
                    .unwrap_or_default()
            ),
            None => "none (no sample model found)".to_owned(),
        }
    }

    fn active_info(&self) -> Option<&AvatarModelInfo> {
        match &self.active {
            Choice::Sample => self.sample.as_ref(),
            Choice::Stored(hash) => self.models.iter().find(|m| &m.id == hash),
        }
    }

    pub fn status(&self) -> AvatarStatus {
        let info = self.active_info();
        AvatarStatus {
            active: match &self.active {
                Choice::Sample => SAMPLE_ID.to_owned(),
                Choice::Stored(hash) => hash.clone(),
            },
            active_id: info.map(|i| i.id.clone()),
            credit: info
                .and_then(|i| i.meta.as_ref())
                .and_then(|m| m.credit.clone()),
            sample: self.sample.as_ref().map(|s| {
                if self.active == Choice::Sample {
                    s.clone()
                } else {
                    s.brief()
                }
            }),
            models: self
                .models
                .iter()
                .map(|m| {
                    if self.active == Choice::Stored(m.id.clone()) {
                        m.clone()
                    } else {
                        m.brief()
                    }
                })
                .collect(),
            pending: self.pending.clone(),
            max_models: MAX_STORED_MODELS,
            max_upload_bytes: MAX_VRM_BYTES,
            renderer_model: self.handle.renderer_model(),
            last_error: self.last_error.clone(),
            save_error: self.save_error.clone(),
        }
    }

    /// Tells the avatar server which file to serve.
    fn apply(&self) {
        match &self.active {
            Choice::Sample => {
                let id = self.sample.as_ref().map_or("", |s| s.id.as_str());
                self.handle.set_active_model(None, id);
            }
            Choice::Stored(hash) => {
                self.handle
                    .set_active_model(Some(self.model_path(hash)), hash);
            }
        }
    }

    fn model_path(&self, hash: &str) -> PathBuf {
        self.env.models_dir().join(format!("{hash}.vrm"))
    }

    fn pending_path(&self, hash: &str) -> PathBuf {
        self.env.pending_dir().join(format!("{hash}.vrm"))
    }

    fn save(&mut self) {
        let file = SettingsFile {
            schema: SETTINGS_SCHEMA.to_owned(),
            active: match &self.active {
                Choice::Sample => SAMPLE_ID.to_owned(),
                Choice::Stored(hash) => hash.clone(),
            },
            recent: self.recent.clone(),
        };
        let result = serde_json::to_string_pretty(&file)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                write_atomic(&self.env.settings, &format!("{text}\n"), false)
                    .map_err(|e| e.to_string())
            });
        self.save_error = result.err().map(|e| {
            eprintln!("avatar: could not save the choice ({e}); it applies until the next start");
            "the choice could not be saved; it applies until the next start".to_owned()
        });
    }

    fn touch(&mut self, hash: &str) {
        self.recent.retain(|h| h != hash);
        self.recent.insert(0, hash.to_owned());
        self.recent.truncate(MAX_RECENT);
    }

    /// Removes the least recently used models, never the active one, until the
    /// store holds at most [`MAX_STORED_MODELS`]. Returns how many went.
    fn evict(&mut self) -> usize {
        let mut evicted = 0;
        while self.models.len() > MAX_STORED_MODELS {
            let rank = |hash: &str| {
                self.recent
                    .iter()
                    .position(|h| h == hash)
                    .unwrap_or(usize::MAX)
            };
            let victim = self
                .models
                .iter()
                .filter(|m| self.active != Choice::Stored(m.id.clone()))
                .max_by(|a, b| rank(&a.id).cmp(&rank(&b.id)).then_with(|| a.id.cmp(&b.id)))
                .map(|m| m.id.clone());
            let Some(victim) = victim else { break };
            let _ = fs::remove_file(self.model_path(&victim));
            self.models.retain(|m| m.id != victim);
            self.recent.retain(|h| h != &victim);
            evicted += 1;
        }
        evicted
    }

    /// Resolves a full hash or an unambiguous prefix (at least 8 characters)
    /// among `candidates`.
    fn resolve<'a>(arg: &str, candidates: impl Iterator<Item = &'a str>) -> Result<String, String> {
        let arg = arg.trim();
        if !is_hash_prefix(arg) {
            return Err("refused: not a model id".to_owned());
        }
        let found: Vec<&str> = candidates.filter(|id| id.starts_with(arg)).collect();
        match found.as_slice() {
            [one] => Ok((*one).to_owned()),
            [] => Err("refused: no such model".to_owned()),
            _ => {
                Err("refused: the id matches more than one model; give more characters".to_owned())
            }
        }
    }

    /// An uploaded file the panel staged under `incoming/<hash>.vrm`: inspect
    /// it again and make it the pending candidate. It does not change what the
    /// page shows.
    pub fn upload(&mut self, argument: &str) -> Result<String, String> {
        let hash = argument.trim();
        if !is_full_hash(hash) {
            return Err("refused: not a staged upload".to_owned());
        }
        let staged = self.env.staging_dir().join(format!("{hash}.vrm"));
        let result = (|| {
            let metadata =
                fs::metadata(&staged).map_err(|_| "refused: no staged upload".to_owned())?;
            if !metadata.is_file() || metadata.len() > MAX_VRM_BYTES {
                return Err("refused: the staged file is not acceptable".to_owned());
            }
            let meta =
                vrm::inspect_file(&staged).map_err(|e| format!("refused: {}: {e}", e.code()))?;
            // The file name is the hash the HTTP layer computed while the bytes
            // arrived; hashing again here would read 64 MiB on this thread.
            let info = AvatarModelInfo::new(hash.to_owned(), metadata.len(), Some(meta));
            sweep(&self.env.pending_dir());
            let target = self.pending_path(hash);
            let _ = fs::remove_file(&target);
            fs::rename(&staged, &target)
                .map_err(|e| format!("refused: could not store the candidate ({e})"))?;
            Ok(info)
        })();
        match result {
            Ok(info) => {
                let detail = format!(
                    "candidate ready: {} sha256={}",
                    display_name(&info),
                    info.id
                );
                self.pending = Some(info);
                self.last_error = None;
                Ok(detail)
            }
            Err(detail) => {
                let _ = fs::remove_file(&staged);
                self.last_error = Some(detail.clone());
                Err(detail)
            }
        }
    }

    /// Confirms the pending candidate (the operator has read its licence): it
    /// is stored and becomes active at once.
    pub fn confirm(&mut self, argument: &str) -> Result<String, String> {
        let result = self.confirm_inner(argument);
        self.last_error = result.as_ref().err().cloned();
        result
    }

    fn confirm_inner(&mut self, argument: &str) -> Result<String, String> {
        let Some(pending) = self.pending.clone() else {
            return Err("refused: there is no candidate to confirm".to_owned());
        };
        let id = Self::resolve(argument, std::iter::once(pending.id.as_str())).map_err(|_| {
            "refused: that is not the candidate on screen; review the current one".to_owned()
        })?;
        let source = self.pending_path(&id);
        let target = self.model_path(&id);
        if self.models.iter().any(|m| m.id == id) {
            let _ = fs::remove_file(&source);
        } else {
            let _ = fs::remove_file(&target);
            fs::rename(&source, &target)
                .map_err(|e| format!("refused: could not store the model ({e})"))?;
            self.models.push(pending.clone());
            self.models.sort_by(|a, b| a.id.cmp(&b.id));
        }
        self.pending = None;
        self.active = Choice::Stored(id.clone());
        self.touch(&id);
        let evicted = self.evict();
        self.save();
        self.apply();
        Ok(self.activated_detail(&id, evicted))
    }

    /// Switches to a stored model or the sample.
    pub fn select(&mut self, argument: &str) -> Result<String, String> {
        let result = self.select_inner(argument);
        self.last_error = result.as_ref().err().cloned();
        result
    }

    fn select_inner(&mut self, argument: &str) -> Result<String, String> {
        let argument = argument.trim();
        if argument == SAMPLE_ID {
            if self.sample.is_none() {
                return Err("refused: the sample model is not available".to_owned());
            }
            self.active = Choice::Sample;
            self.save();
            self.apply();
            let id = self
                .sample
                .as_ref()
                .map(|s| s.id.clone())
                .unwrap_or_default();
            return Ok(self.activated_detail(&id, 0));
        }
        let id = Self::resolve(argument, self.models.iter().map(|m| m.id.as_str()))?;
        self.active = Choice::Stored(id.clone());
        self.touch(&id);
        self.save();
        self.apply();
        Ok(self.activated_detail(&id, 0))
    }

    /// Deletes a stored model; the active one cannot be deleted.
    pub fn delete(&mut self, argument: &str) -> Result<String, String> {
        let result = self.delete_inner(argument);
        self.last_error = result.as_ref().err().cloned();
        result
    }

    fn delete_inner(&mut self, argument: &str) -> Result<String, String> {
        let id = Self::resolve(argument, self.models.iter().map(|m| m.id.as_str()))?;
        if self.active == Choice::Stored(id.clone()) {
            return Err(
                "refused: the active model cannot be deleted; select another first".to_owned(),
            );
        }
        let name = self
            .models
            .iter()
            .find(|m| m.id == id)
            .map(display_name)
            .unwrap_or_default();
        fs::remove_file(self.model_path(&id))
            .map_err(|e| format!("refused: could not delete the model ({e})"))?;
        self.models.retain(|m| m.id != id);
        self.recent.retain(|h| h != &id);
        self.save();
        Ok(format!("deleted: {name} sha256={id}"))
    }

    /// Drops the pending candidate.
    pub fn discard(&mut self) -> Result<String, String> {
        let Some(pending) = self.pending.take() else {
            self.last_error = Some("refused: there is no candidate".to_owned());
            return Err("refused: there is no candidate".to_owned());
        };
        let _ = fs::remove_file(self.pending_path(&pending.id));
        self.last_error = None;
        Ok(format!(
            "candidate discarded: {} sha256={}",
            display_name(&pending),
            pending.id
        ))
    }

    fn activated_detail(&self, id: &str, evicted: usize) -> String {
        let info = self.active_info();
        let name = info.map(display_name).unwrap_or_default();
        let mut detail = format!("active: {name} sha256={id}");
        if let Some(credit) = info
            .and_then(|i| i.meta.as_ref())
            .and_then(|m| m.credit.as_deref())
        {
            detail.push_str(&format!("; credit: {credit}"));
        }
        if evicted > 0 {
            detail.push_str(&format!("; removed {evicted} least recently used"));
        }
        detail
    }
}

fn is_hash_prefix(text: &str) -> bool {
    aivtuber_adapters::is_hash_prefix(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_adapters::{LocalAvatarConfig, LocalAvatarServer};

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aivtuber-362-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// A minimal valid VRM 1.0 file around `meta_json`.
    pub(crate) fn model(meta_json: &str) -> Vec<u8> {
        let json = format!(r#"{{"extensions":{{"VRMC_vrm":{{"meta":{meta_json}}}}}}}"#);
        let mut json = json.into_bytes();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let bin = [1_u8, 2, 3, 4];
        let total = 12 + 8 + json.len() + 8 + bin.len();
        let mut out = 0x4654_6C67_u32.to_le_bytes().to_vec();
        out.extend_from_slice(&2_u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(&0x4E4F_534A_u32.to_le_bytes());
        out.extend_from_slice(&json);
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(&0x004E_4942_u32.to_le_bytes());
        out.extend_from_slice(&bin);
        out
    }

    pub(crate) fn hash_of(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    struct Rig {
        root: PathBuf,
        env: AvatarEnvironment,
        server: LocalAvatarServer,
    }

    impl Rig {
        fn new(name: &str) -> Self {
            let root = scratch(name);
            let page = root.join("page");
            fs::create_dir_all(&page).unwrap();
            fs::write(
                page.join("model.vrm"),
                model(r#"{"name":"Sample","authors":["pixiv"],"avatarPermission":"everyone","commercialUsage":"corporation"}"#),
            )
            .unwrap();
            let server = LocalAvatarServer::start(LocalAvatarConfig {
                port: 0,
                page_dir: Some(page.clone()),
                ..LocalAvatarConfig::default()
            })
            .unwrap();
            Self {
                env: AvatarEnvironment {
                    settings: root.join("local-avatar.json"),
                    dir: root.join("avatars"),
                    sample: Some(page.join("model.vrm")),
                },
                root,
                server,
            }
        }

        fn open(&self) -> AvatarController {
            AvatarController::new(self.env.clone(), self.server.handle()).unwrap()
        }

        /// Plays the panel's HTTP layer: stage `bytes` as `incoming/<hash>.vrm`.
        fn stage(&self, bytes: &[u8]) -> String {
            let hash = hash_of(bytes);
            let dir = self.env.staging_dir();
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("{hash}.vrm")), bytes).unwrap();
            hash
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn named(name: &str) -> Vec<u8> {
        model(&format!(
            r#"{{"name":"{name}","authors":["A"],"avatarPermission":"everyone","commercialUsage":"corporation"}}"#
        ))
    }

    #[test]
    fn a_fresh_store_shows_the_sample() {
        let rig = Rig::new("fresh");
        let c = rig.open();
        let status = c.status();
        assert_eq!(status.active, SAMPLE_ID);
        assert_eq!(
            status.active_id,
            status.sample.as_ref().map(|s| s.id.clone())
        );
        assert!(status.models.is_empty() && status.pending.is_none());
        assert_eq!(status.max_models, MAX_STORED_MODELS);
        assert_eq!(
            status.sample.unwrap().meta.unwrap().name.as_deref(),
            Some("Sample")
        );
    }

    #[test]
    fn upload_then_confirm_activates_and_persists() {
        let rig = Rig::new("flow");
        let mut c = rig.open();
        let bytes = named("Alice");
        let hash = rig.stage(&bytes);
        let detail = c.upload(&hash).unwrap();
        assert!(
            detail.contains("Alice") && detail.contains(&hash),
            "{detail}"
        );
        // A candidate does not change what the page shows.
        assert_eq!(c.status().active, SAMPLE_ID);
        assert_eq!(c.status().pending.as_ref().unwrap().id, hash);
        assert!(!rig.env.staging_dir().join(format!("{hash}.vrm")).exists());
        let detail = c.confirm(&hash).unwrap();
        assert!(detail.contains("active: Alice"), "{detail}");
        let status = c.status();
        assert_eq!(status.active, hash);
        assert_eq!(status.active_id.as_deref(), Some(hash.as_str()));
        assert!(status.pending.is_none());
        assert_eq!(status.models.len(), 1);
        assert_eq!(
            fs::read(rig.env.models_dir().join(format!("{hash}.vrm"))).unwrap(),
            bytes
        );
        assert!(!rig.env.pending_dir().join(format!("{hash}.vrm")).exists());
        // A restart keeps the choice.
        drop(c);
        let again = rig.open();
        assert_eq!(again.status().active, hash);
        assert_eq!(again.status().models.len(), 1);
    }

    #[test]
    fn the_settings_file_is_small_atomic_and_strict() {
        let rig = Rig::new("settings");
        let mut c = rig.open();
        let hash = rig.stage(&named("Alice"));
        c.upload(&hash).unwrap();
        c.confirm(&hash).unwrap();
        let text = fs::read_to_string(&rig.env.settings).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema"], SETTINGS_SCHEMA);
        assert_eq!(value["active"], hash.as_str());
        assert!(!rig.env.settings.with_extension("json.tmp").exists());
        drop(c);
        // Invalid, oversized or foreign settings mean the sample.
        for bad in [
            "not json".to_owned(),
            r#"{"schema":"other/1","active":"sample"}"#.to_owned(),
            format!(r#"{{"schema":"{SETTINGS_SCHEMA}","active":"../../x"}}"#),
            format!(r#"{{"schema":"{SETTINGS_SCHEMA}","active":"{hash}","extra":1}}"#),
            format!(r#"{{"schema":"{SETTINGS_SCHEMA}","active":"{hash}","recent":["nope"]}}"#),
            " ".repeat(5000),
        ] {
            fs::write(&rig.env.settings, &bad).unwrap();
            assert_eq!(rig.open().status().active, SAMPLE_ID, "{bad:.60}");
        }
        // A saved model that is gone falls back to the sample.
        fs::write(
            &rig.env.settings,
            format!(
                r#"{{"schema":"{SETTINGS_SCHEMA}","active":"{}"}}"#,
                "ab".repeat(32)
            ),
        )
        .unwrap();
        assert_eq!(rig.open().status().active, SAMPLE_ID);
    }

    #[test]
    fn a_stale_candidate_cannot_be_confirmed() {
        let rig = Rig::new("stale");
        let mut c = rig.open();
        let first = rig.stage(&named("First"));
        c.upload(&first).unwrap();
        let second = rig.stage(&named("Second"));
        c.upload(&second).unwrap();
        // The newer upload replaced the candidate and its file.
        assert!(!rig.env.pending_dir().join(format!("{first}.vrm")).exists());
        let error = c.confirm(&first).unwrap_err();
        assert!(error.contains("not the candidate"), "{error}");
        assert_eq!(c.status().active, SAMPLE_ID);
        assert!(
            c.confirm(&second[..12]).is_ok(),
            "a prefix names the candidate too"
        );
        assert!(c.confirm(&second).unwrap_err().contains("no candidate"));
    }

    #[test]
    fn uploading_the_sample_again_is_a_distinct_choice() {
        let rig = Rig::new("again");
        let mut c = rig.open();
        let bytes = fs::read(rig.env.sample.as_ref().unwrap()).unwrap();
        let hash = rig.stage(&bytes);
        c.upload(&hash).unwrap();
        c.confirm(&hash).unwrap();
        let status = c.status();
        assert_eq!(status.active, hash, "a stored copy, not the sample");
        assert_eq!(
            Some(hash.clone()),
            status.sample.map(|s| s.id),
            "same content"
        );
        c.select(SAMPLE_ID).unwrap();
        assert_eq!(c.status().active, SAMPLE_ID);
    }

    #[test]
    fn select_switches_live_and_the_page_is_told() {
        let rig = Rig::new("select");
        let mut c = rig.open();
        let hashes: Vec<String> = ["One", "Two"]
            .iter()
            .map(|n| {
                let h = rig.stage(&named(n));
                c.upload(&h).unwrap();
                c.confirm(&h).unwrap();
                h
            })
            .collect();
        assert_eq!(c.status().active, hashes[1]);
        let detail = c.select(&hashes[0][..10]).unwrap();
        assert!(detail.contains("One"), "{detail}");
        assert_eq!(c.status().active, hashes[0]);
        c.select("sample").unwrap();
        assert_eq!(c.status().active, SAMPLE_ID);
        // Refusals: unknown, ambiguous-looking, malformed.
        for bad in ["deadbeef", "ab", "../x", "Sample", &"f".repeat(64)] {
            let error = c.select(bad).unwrap_err();
            assert!(error.starts_with("refused:"), "{bad}: {error}");
        }
        assert_eq!(c.status().active, SAMPLE_ID);
        assert!(c.status().last_error.is_some());
        c.select(&hashes[1]).unwrap();
        assert!(c.status().last_error.is_none());
    }

    #[test]
    fn delete_refuses_the_active_model_and_removes_the_file() {
        let rig = Rig::new("delete");
        let mut c = rig.open();
        let one = rig.stage(&named("One"));
        c.upload(&one).unwrap();
        c.confirm(&one).unwrap();
        let two = rig.stage(&named("Two"));
        c.upload(&two).unwrap();
        c.confirm(&two).unwrap();
        let error = c.delete(&two).unwrap_err();
        assert!(error.contains("active model cannot be deleted"), "{error}");
        let detail = c.delete(&one).unwrap();
        assert!(detail.contains("One") && detail.contains(&one), "{detail}");
        assert!(!rig.env.models_dir().join(format!("{one}.vrm")).exists());
        assert_eq!(c.status().models.len(), 1);
        assert!(c.delete(&one).unwrap_err().contains("no such model"));
        // The sample and the candidate are not deletable models.
        assert!(c.delete("sample").is_err());
    }

    #[test]
    fn the_store_is_bounded_and_evicts_the_least_recently_used() {
        let rig = Rig::new("evict");
        let mut c = rig.open();
        let mut hashes = Vec::new();
        for i in 0..MAX_STORED_MODELS {
            let h = rig.stage(&named(&format!("M{i}")));
            c.upload(&h).unwrap();
            c.confirm(&h).unwrap();
            hashes.push(h);
        }
        assert_eq!(c.status().models.len(), MAX_STORED_MODELS);
        // Use the oldest again: it is now the most recently used.
        c.select(&hashes[0]).unwrap();
        let extra = rig.stage(&named("Extra"));
        c.upload(&extra).unwrap();
        let detail = c.confirm(&extra).unwrap();
        assert!(detail.contains("removed 1"), "{detail}");
        let status = c.status();
        assert_eq!(status.models.len(), MAX_STORED_MODELS);
        let kept: Vec<&str> = status.models.iter().map(|m| m.id.as_str()).collect();
        assert!(kept.contains(&hashes[0].as_str()), "recently used survives");
        assert!(kept.contains(&extra.as_str()));
        assert!(
            !kept.contains(&hashes[1].as_str()),
            "least recently used goes"
        );
        assert!(
            !rig.env
                .models_dir()
                .join(format!("{}.vrm", hashes[1]))
                .exists()
        );
        // The files on disk are exactly the kept ones.
        let on_disk = fs::read_dir(rig.env.models_dir()).unwrap().count();
        assert_eq!(on_disk, MAX_STORED_MODELS);
    }

    #[test]
    fn the_active_model_is_never_evicted() {
        let rig = Rig::new("keep-active");
        let mut c = rig.open();
        let mut hashes = Vec::new();
        for i in 0..MAX_STORED_MODELS {
            let h = rig.stage(&named(&format!("M{i}")));
            c.upload(&h).unwrap();
            c.confirm(&h).unwrap();
            hashes.push(h);
        }
        // Make the oldest one active, then push it to the end of the recency list.
        c.select(&hashes[0]).unwrap();
        c.recent.clear();
        let extra = rig.stage(&named("Extra"));
        c.upload(&extra).unwrap();
        c.confirm(&extra).unwrap();
        // The new model became active, so the previously active one is evictable;
        // but while a model is active it always survives.
        let status = c.status();
        assert_eq!(status.active, extra);
        assert!(status.models.iter().any(|m| m.id == extra));
        assert_eq!(status.models.len(), MAX_STORED_MODELS);
    }

    #[test]
    fn bad_staged_files_are_refused_and_removed() {
        let rig = Rig::new("bad");
        let mut c = rig.open();
        // Not a hash.
        for bad in ["", "x", "../x", &"A".repeat(64)] {
            assert!(c.upload(bad).unwrap_err().starts_with("refused:"), "{bad}");
        }
        // Nothing staged under that name.
        assert!(
            c.upload(&"ab".repeat(32))
                .unwrap_err()
                .contains("no staged")
        );
        // A staged file that is not a VRM (a hand-planted one).
        let junk = b"glTF but not really, long enough to be read as a header".to_vec();
        let hash = rig.stage(&junk);
        let error = c.upload(&hash).unwrap_err();
        assert!(error.starts_with("refused: unsupported_version"), "{error}");
        assert!(!rig.env.staging_dir().join(format!("{hash}.vrm")).exists());
        assert!(c.status().pending.is_none());
    }

    #[test]
    fn leftovers_are_cleared_at_start_and_unparsable_models_are_ignored() {
        let rig = Rig::new("leftovers");
        fs::create_dir_all(rig.env.staging_dir()).unwrap();
        fs::create_dir_all(rig.env.pending_dir()).unwrap();
        fs::create_dir_all(rig.env.models_dir()).unwrap();
        fs::write(rig.env.staging_dir().join("up-1.part"), b"x").unwrap();
        fs::write(
            rig.env
                .pending_dir()
                .join(format!("{}.vrm", "cd".repeat(32))),
            b"x",
        )
        .unwrap();
        let good = named("Kept");
        let good_hash = hash_of(&good);
        fs::write(rig.env.models_dir().join(format!("{good_hash}.vrm")), &good).unwrap();
        fs::write(
            rig.env
                .models_dir()
                .join(format!("{}.vrm", "ef".repeat(32))),
            b"junk",
        )
        .unwrap();
        fs::write(rig.env.models_dir().join("notes.txt"), b"x").unwrap();
        let c = rig.open();
        assert_eq!(fs::read_dir(rig.env.staging_dir()).unwrap().count(), 0);
        assert_eq!(fs::read_dir(rig.env.pending_dir()).unwrap().count(), 0);
        assert_eq!(c.status().models.len(), 1);
        assert_eq!(c.status().models[0].id, good_hash);
    }

    #[test]
    fn discard_drops_the_candidate_and_its_file() {
        let rig = Rig::new("discard");
        let mut c = rig.open();
        assert!(c.discard().is_err());
        let hash = rig.stage(&named("Maybe"));
        c.upload(&hash).unwrap();
        let detail = c.discard().unwrap();
        assert!(detail.contains("Maybe"), "{detail}");
        assert!(c.status().pending.is_none());
        assert!(!rig.env.pending_dir().join(format!("{hash}.vrm")).exists());
    }

    #[test]
    fn a_missing_sample_still_lets_uploaded_models_work() {
        let mut rig = Rig::new("nosample");
        rig.env.sample = None;
        let mut c = rig.open();
        assert!(c.status().sample.is_none() && c.status().active_id.is_none());
        assert!(c.select(SAMPLE_ID).unwrap_err().contains("not available"));
        let hash = rig.stage(&named("Only"));
        c.upload(&hash).unwrap();
        c.confirm(&hash).unwrap();
        assert_eq!(c.status().active_id.as_deref(), Some(hash.as_str()));
    }

    #[test]
    fn the_credit_of_the_active_model_is_surfaced() {
        let rig = Rig::new("credit");
        let mut c = rig.open();
        let bytes = model(
            r#"{"name":"Credited","authors":["Dana"],"avatarPermission":"everyone","commercialUsage":"corporation","creditNotation":"required"}"#,
        );
        let hash = rig.stage(&bytes);
        c.upload(&hash).unwrap();
        let detail = c.confirm(&hash).unwrap();
        assert!(detail.contains("credit: Credited / Dana"), "{detail}");
        assert_eq!(c.status().credit.as_deref(), Some("Credited / Dana"));
        assert!(c.active_summary().contains("credit: Credited / Dana"));
        c.select(SAMPLE_ID).unwrap();
        assert_eq!(c.status().credit, None);
    }

    #[test]
    fn the_status_stays_small_with_a_full_store() {
        let rig = Rig::new("size");
        let mut c = rig.open();
        let long = "あ".repeat(10_000);
        for i in 0..(MAX_STORED_MODELS + 1) {
            let bytes = model(&format!(
                r#"{{"name":"{long}{i}","authors":["{long}","{long}","{long}","{long}","{long}","{long}","{long}","{long}","{long}"],
                "copyrightInformation":"{long}","contactInformation":"{long}","licenseUrl":"{long}","otherLicenseUrl":"{long}"}}"#
            ));
            let h = rig.stage(&bytes);
            c.upload(&h).unwrap();
            if i < MAX_STORED_MODELS {
                c.confirm(&h).unwrap();
            }
        }
        let json = serde_json::to_string(&c.status()).unwrap();
        // Well inside the panel's 64 KiB state document, which also carries the
        // rest of the status and the replies.
        assert!(json.len() < 40 * 1024, "{}", json.len());
    }
}
