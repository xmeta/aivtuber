//! Startup audit of the active Performance Asset pack (issue #289).
//!
//! `AssetStore` already refuses descriptors whose compatibility identity does
//! not match the runtime, and the audio player cannot play a file that is not
//! there. Both used to be silent: the operator saw `usable=0` or an `intent`
//! that produced no sound, with nothing naming the cause. This module turns
//! the already-indexed pack plus the audio root into one bounded, counted
//! report and (at most) one warning block. It never changes what is usable.

use aivtuber_asset_store::{
    AssetStore, CompatibilityMismatch, CompatibilityStatus, load_asset_file,
};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

const AUDIO_SCHEME: &str = "audio://";

/// Why descriptors are unusable. The counts are per descriptor and not
/// exclusive: one descriptor can be both missing audio and mismatched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackReadiness {
    pub indexed: usize,
    /// Descriptors with no problem at all.
    pub ready: usize,
    /// Descriptors that fail at least one check.
    pub unusable: usize,
    /// Descriptors that name an `audio://` file that is absent (or whose
    /// reference cannot be resolved under the audio root).
    pub missing_audio: usize,
    /// Descriptors whose `voice_model` differs from the configured voice.
    pub voice_model_mismatch: usize,
    /// Descriptors incompatible on any other field, by field name.
    pub other_mismatch: Vec<(&'static str, usize)>,
    /// The `voice_model` values the unusable-by-voice descriptors expect.
    pub expected_voice_models: BTreeSet<String>,
    /// Descriptors that could not be read again after indexing.
    pub unreadable: usize,
}

fn audio_path(audio_root: &Path, audio_ref: &str) -> Option<PathBuf> {
    // The same traversal rule the audio player enforces.
    let relative = Path::new(audio_ref.strip_prefix(AUDIO_SCHEME)?);
    let invalid = relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    (!invalid).then(|| audio_root.join(relative))
}

/// Audit every indexed descriptor. Call after the store has been indexed.
pub fn audit(assets: &AssetStore, audio_root: &Path) -> PackReadiness {
    let mut report = PackReadiness::default();
    let mut other: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for (_, entry) in assets.indexed_entries() {
        report.indexed += 1;
        let mut bad = false;
        if let CompatibilityStatus::Incompatible(mismatches) = &entry.compatibility {
            bad = true;
            let mut voice = false;
            let mut fields = BTreeSet::new();
            for CompatibilityMismatch {
                field,
                asset_requires,
                ..
            } in mismatches
            {
                if *field == "voice_model" {
                    voice = true;
                    report.expected_voice_models.insert(asset_requires.clone());
                } else {
                    fields.insert(*field);
                }
            }
            if voice {
                report.voice_model_mismatch += 1;
            }
            for field in fields {
                *other.entry(field).or_default() += 1;
            }
        }
        match load_asset_file(&entry.path) {
            Ok(asset) => {
                let audio_ref = asset.speech.and_then(|speech| speech.audio_ref);
                if let Some(audio_ref) = audio_ref
                    && !audio_path(audio_root, &audio_ref).is_some_and(|path| path.is_file())
                {
                    bad = true;
                    report.missing_audio += 1;
                }
            }
            Err(_) => {
                bad = true;
                report.unreadable += 1;
            }
        }
        if bad {
            report.unusable += 1;
        } else {
            report.ready += 1;
        }
    }
    report.other_mismatch = other.into_iter().collect();
    report
}

impl PackReadiness {
    pub fn is_ready(&self) -> bool {
        self.unusable == 0
    }

    /// Startup log lines: empty when every descriptor is ready, otherwise one
    /// counted warning (the first line) with the reasons and the remedy.
    pub fn warning_lines(&self, configured_voice: Option<&str>, audio_root: &Path) -> Vec<String> {
        if self.is_ready() {
            return Vec::new();
        }
        let expects = if self.expected_voice_models.is_empty() {
            "none".to_owned()
        } else {
            self.expected_voice_models
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        };
        let others = if self.other_mismatch.is_empty() {
            "none".to_owned()
        } else {
            self.other_mismatch
                .iter()
                .map(|(field, count)| format!("{field}={count}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut lines = vec![format!(
            "pack_readiness: WARNING {} of {} descriptors are unusable (cached reactions from them are silent or skipped): \
             missing_audio={} voice_model_mismatch={} other_incompatible={} unreadable={}",
            self.unusable,
            self.indexed,
            self.missing_audio,
            self.voice_model_mismatch,
            others,
            self.unreadable
        )];
        if self.voice_model_mismatch > 0 {
            lines.push(format!(
                "pack_readiness: voice_model: descriptors expect [{expects}], configured AIVTUBER_VOICE_MODEL={}; unset it or use cached assets authored for that voice",
                configured_voice.unwrap_or("<unset>")
            ));
        }
        if self.missing_audio > 0 {
            lines.push(format!(
                "pack_readiness: audio: {} referenced audio file(s) not found under {}; run `node scripts/synthesize-starter-audio.mjs` (docs/local-profile-quickstart.adoc) or supply them",
                self.missing_audio,
                audio_root.display()
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_asset_store::RuntimeCompatibility;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "aivtuber-readiness-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("test dir");
            Self(path)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pack() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/starter-reaction-pack")
    }

    fn runtime(voice: &str) -> RuntimeCompatibility {
        RuntimeCompatibility {
            compiler_version: "0.1.0".to_owned(),
            voice_model: Some(voice.to_owned()),
            avatar_profile: Some("example-live2d-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
            motion_library: Some("starter-v1".to_owned()),
        }
    }

    fn audited(voice: &str, audio_root: &Path) -> PackReadiness {
        let mut store = AssetStore::new(pack().join("descriptors"), runtime(voice));
        store.index_local().expect("index starter pack");
        audit(&store, audio_root)
    }

    /// Materialize every `audio://` file the starter pack references.
    fn write_audio(root: &Path) {
        let starter = root.join("starter");
        fs::create_dir_all(&starter).unwrap();
        for entry in fs::read_dir(pack().join("descriptors")).unwrap() {
            let asset = load_asset_file(entry.unwrap().path()).unwrap();
            if let Some(audio_ref) = asset.speech.and_then(|s| s.audio_ref) {
                fs::write(audio_path(root, &audio_ref).unwrap(), b"x").unwrap();
            }
        }
    }

    #[test]
    fn missing_audio_is_counted_once_with_the_reason() {
        let dir = Dir::new("missing");
        let report = audited("example-voice-v1", &dir.0);
        // Six speech descriptors name audio; the two reaction.neutral ones are speechless.
        assert_eq!(report.indexed, 8);
        assert_eq!(report.missing_audio, 6);
        assert_eq!(report.voice_model_mismatch, 0);
        assert_eq!(report.unusable, 6);
        assert_eq!(report.ready, 2);
        let lines = report.warning_lines(Some("example-voice-v1"), &dir.0);
        assert!(
            lines[0].contains("6 of 8 descriptors are unusable"),
            "{lines:?}"
        );
        assert!(lines[0].contains("missing_audio=6 voice_model_mismatch=0"));
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("synthesize-starter-audio"));
    }

    #[test]
    fn a_voice_mismatch_names_expected_and_configured_voice() {
        let dir = Dir::new("voice");
        write_audio(&dir.0);
        let report = audited("some-other-voice", &dir.0);
        // The two speechless reaction.neutral descriptors name no voice.
        assert_eq!(report.voice_model_mismatch, 6);
        assert_eq!(report.missing_audio, 0);
        assert_eq!(report.unusable, 6);
        assert!(report.other_mismatch.is_empty());
        let lines = report.warning_lines(Some("some-other-voice"), &dir.0);
        assert!(
            lines[0].contains("6 of 8 descriptors are unusable"),
            "{lines:?}"
        );
        assert!(lines[0].contains("voice_model_mismatch=6"));
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("[example-voice-v1]"));
        assert!(lines[1].contains("AIVTUBER_VOICE_MODEL=some-other-voice"));
    }

    #[test]
    fn both_reasons_are_reported_and_other_fields_are_named() {
        let dir = Dir::new("both");
        let mut store = AssetStore::new(
            pack().join("descriptors"),
            RuntimeCompatibility {
                motion_library: Some("other-motions".to_owned()),
                ..runtime("some-other-voice")
            },
        );
        store.index_local().unwrap();
        let report = audit(&store, &dir.0);
        assert_eq!(report.voice_model_mismatch, 6);
        assert_eq!(report.missing_audio, 6);
        assert_eq!(report.other_mismatch, vec![("motion_library", 8)]);
        assert_eq!(report.unusable, 8);
        let lines = report.warning_lines(None, &dir.0);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("other_incompatible=motion_library=8"));
        assert!(lines[1].contains("AIVTUBER_VOICE_MODEL=<unset>"));
    }

    #[test]
    fn a_fully_usable_pack_is_silent() {
        let dir = Dir::new("ready");
        write_audio(&dir.0);
        let report = audited("example-voice-v1", &dir.0);
        assert!(report.is_ready());
        assert_eq!((report.indexed, report.ready, report.unusable), (8, 8, 0));
        assert!(
            report
                .warning_lines(Some("example-voice-v1"), &dir.0)
                .is_empty()
        );
    }

    #[test]
    fn audio_references_cannot_escape_the_root() {
        let root = Path::new("/audio");
        assert!(audio_path(root, "audio://starter/a.opus").is_some());
        assert!(audio_path(root, "audio://../a.opus").is_none());
        assert!(audio_path(root, "audio://").is_none());
        assert!(audio_path(root, "file:///a.opus").is_none());
    }
}
