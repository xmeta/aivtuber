//! The single-source pin manifest (`packaging/local-profile-pins.json`).
//!
//! The same file is read by the Node fetch scripts. It is embedded at build
//! time, so the shipped binary needs no file beside it, and a pin cannot be
//! swapped by editing something next to the executable.

use serde::Deserialize;
use std::collections::BTreeMap;

pub const EMBEDDED: &str = include_str!("../../../packaging/local-profile-pins.json");
pub const SCHEMA: &str = "local-profile-pins/1";

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub schema: String,
    pub llama: Llama,
    /// The default profile's model (the Node scripts read this key).
    pub llm: Asset,
    pub profiles: BTreeMap<String, Profile>,
    pub vad: Asset,
    pub avatar: Asset,
    pub sherpa_lib: SherpaLib,
    pub models: BTreeMap<String, Model>,
    pub disclosures: Vec<Disclosure>,
}

/// One downloadable file: exact size, SHA-256, HTTPS URL.
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub file: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    /// Upper bound on the bytes an archive may expand to (zip/tar bomb guard).
    #[serde(default)]
    pub max_extracted_bytes: Option<u64>,
    #[serde(default)]
    pub licence: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Llama {
    pub tag: String,
    pub licence: String,
    /// Visual C++ runtime DLLs the Windows zips import but do not ship.
    pub windows_runtime_dlls: Vec<String>,
    /// Keyed `<platform>-<arch>-<flavor>`, for example `win32-x64-vulkan`.
    pub assets: BTreeMap<String, Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SherpaLib {
    pub version: String,
    pub assets: BTreeMap<String, Asset>,
}

/// A file with a pin but possibly no URL of its own (it comes out of an archive).
#[derive(Debug, Clone, Deserialize)]
pub struct PinnedFile {
    pub file: String,
    #[serde(default)]
    pub url: Option<String>,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    #[serde(default)]
    pub archive: Option<Asset>,
    #[serde(default)]
    pub inner_dir: Option<String>,
    pub files: Vec<PinnedFile>,
}

/// A selectable llama-server setup. `default` has no files of its own and uses
/// the top-level `llm` pin; others carry their own.
#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub description: String,
    /// Model name the app sends and llama-server serves (`--alias`).
    pub alias: String,
    pub model_version: String,
    /// llama-server `-c`: the total context, shared by the slots.
    pub context: u32,
    /// llama-server `--parallel`; absent keeps the server's own default.
    #[serde(default)]
    pub parallel: Option<u32>,
    /// Documented hard requirement, printed before anything is downloaded.
    #[serde(default)]
    pub min_vram_gb: Option<u32>,
    #[serde(default)]
    pub llm: Option<Asset>,
    #[serde(default)]
    pub mmproj: Option<Asset>,
}

/// A profile with its model files resolved.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedProfile<'a> {
    pub name: &'a str,
    pub spec: &'a Profile,
    pub llm: &'a Asset,
    pub mmproj: Option<&'a Asset>,
}

pub const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone, Deserialize)]
pub struct Disclosure {
    pub id: String,
    pub component: String,
    pub licence: String,
    /// Step ids (`tts`, `stt`, `llama`, `llm`, `vad`, `avatar`) or `always`.
    pub applies_to: Vec<String>,
    pub duty: String,
}

impl Manifest {
    pub fn embedded() -> Self {
        Self::parse(EMBEDDED).expect("the embedded pin manifest is valid (checked by tests)")
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let manifest: Self = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if manifest.schema != SCHEMA {
            return Err(format!(
                "manifest schema {:?}, expected {SCHEMA}",
                manifest.schema
            ));
        }
        Ok(manifest)
    }

    pub fn profile_names(&self) -> Vec<&str> {
        self.profiles.keys().map(String::as_str).collect()
    }

    pub fn profile<'a>(&'a self, name: &str) -> Result<ResolvedProfile<'a>, String> {
        let (key, spec) = self.profiles.get_key_value(name).ok_or_else(|| {
            format!(
                "unknown profile {name:?} (pinned: {})",
                self.profile_names().join(", ")
            )
        })?;
        Ok(ResolvedProfile {
            name: key,
            spec,
            llm: spec.llm.as_ref().unwrap_or(&self.llm),
            mmproj: spec.mmproj.as_ref(),
        })
    }

    /// The llama.cpp asset for `platform` (`win32-x64`, `linux-x64`) and `flavor`.
    pub fn llama_asset(&self, platform: &str, flavor: &str) -> Result<&Asset, String> {
        let key = format!("{platform}-{flavor}");
        self.llama.assets.get(&key).ok_or_else(|| {
            format!(
                "no pinned llama.cpp asset for {key} (pinned: {})",
                self.llama
                    .assets
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }

    /// Disclosures that apply to the steps actually provisioned (plus `always`).
    pub fn disclosures_for<'a>(&'a self, steps: &[&str]) -> Vec<&'a Disclosure> {
        self.disclosures
            .iter()
            .filter(|d| {
                d.applies_to
                    .iter()
                    .any(|a| a == "always" || steps.contains(&a.as_str()))
            })
            .collect()
    }
}

pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_asset(m: &Manifest) -> Vec<&Asset> {
        let mut out: Vec<&Asset> = vec![&m.llm, &m.vad, &m.avatar];
        for p in m.profiles.values() {
            out.extend(p.llm.iter().chain(p.mmproj.iter()));
        }
        out.extend(m.llama.assets.values());
        out.extend(m.sherpa_lib.assets.values());
        for model in m.models.values() {
            out.extend(model.archive.iter());
        }
        out
    }

    #[test]
    fn the_embedded_manifest_parses_and_every_pin_is_well_formed() {
        let m = Manifest::embedded();
        for a in every_asset(&m) {
            assert!(is_sha256_hex(&a.sha256), "{}", a.file);
            assert!(a.size > 0, "{}", a.file);
            assert!(a.url.starts_with("https://"), "{} is not https", a.url);
            assert!(!a.file.contains(['/', '\\']), "{}", a.file);
        }
        for model in m.models.values() {
            for f in &model.files {
                assert!(is_sha256_hex(&f.sha256) && f.size > 0, "{}", f.file);
                if model.archive.is_none() {
                    let url = f.url.as_deref().expect("a non-archive file has a URL");
                    assert!(url.starts_with("https://"), "{url}");
                } else {
                    assert!(model.inner_dir.is_some());
                }
                assert!(!f.file.contains(['/', '\\']), "{}", f.file);
            }
        }
        for a in m.llama.assets.values() {
            assert!(a.max_extracted_bytes.is_some(), "{}", a.file);
        }
        assert!(!m.llama.windows_runtime_dlls.is_empty());
        for d in &m.llama.windows_runtime_dlls {
            assert!(d.ends_with(".dll") && !d.contains(['/', '\\']), "{d}");
        }
    }

    /// The Node scripts must not carry a second copy of any pin: they read the
    /// manifest. A 64-hex string or a download URL in a script would be one.
    #[test]
    fn the_fetch_scripts_hold_no_pins_of_their_own() {
        let scripts = [
            include_str!("../../../scripts/fetch-local-profile.mjs"),
            include_str!("../../../scripts/fetch-sherpa-onnx.mjs"),
            include_str!("../../../renderer/local-avatar/smoke/fetch-model.mjs"),
        ];
        for script in scripts {
            assert!(script.contains("local-profile-pins.json"));
            let has_hex = script
                .split(|c: char| !c.is_ascii_hexdigit())
                .any(|run| run.len() >= 64);
            assert!(!has_hex, "a script embeds a SHA-256 digest");
            assert!(
                !script.contains("https://github.com/")
                    && !script.contains("https://huggingface.co/")
            );
        }
    }

    /// The runtime pins in the adapters crate (verified by the sidecars at every
    /// start) agree with the manifest: digest and size, file by file.
    #[test]
    fn the_manifest_model_pins_equal_the_sidecar_pins() {
        use aivtuber_adapters::speech_stt::pins::{K2V2_INT8, PARAKEET_JA_INT8};
        use aivtuber_adapters::speech_tts::pins::SUPERTONIC_3_INT8;
        let m = Manifest::embedded();
        for (name, pin) in [
            ("supertonic-3-int8", SUPERTONIC_3_INT8),
            ("k2v2-int8", K2V2_INT8),
            ("parakeet-ja", PARAKEET_JA_INT8),
        ] {
            let model = &m.models[name];
            assert_eq!(model.files.len(), pin.files.len(), "{name}");
            for file in pin.files {
                let entry = model
                    .files
                    .iter()
                    .find(|f| f.file == file.file_name)
                    .unwrap_or_else(|| panic!("{name}: manifest lacks {}", file.file_name));
                assert_eq!(entry.sha256, file.sha256, "{}", file.file_name);
                assert_eq!(entry.size, file.size, "{}", file.file_name);
            }
        }
    }

    #[test]
    fn every_profile_resolves_and_perception_is_the_documented_one() {
        let m = Manifest::embedded();
        let d = m.profile(DEFAULT_PROFILE).unwrap();
        assert_eq!(d.llm.file, m.llm.file, "default uses the top-level pin");
        assert!(d.mmproj.is_none() && d.spec.parallel.is_none());
        assert_eq!(d.spec.alias, "gemma-4-e4b-it-qat");
        assert_eq!(d.spec.context, 4096);
        let p = m.profile("perception").unwrap();
        assert_eq!(p.spec.alias, "qwen3-vl-4b-instruct");
        assert_eq!((p.spec.context, p.spec.parallel), (8192, Some(2)));
        assert_eq!(p.spec.min_vram_gb, Some(12));
        assert!(p.mmproj.is_some());
        assert_ne!(p.llm.file, d.llm.file);
        let err = m.profile("nope").unwrap_err();
        assert!(
            err.contains("default") && err.contains("perception"),
            "{err}"
        );
        // Each profile has exactly one disclosure naming it.
        for name in m.profile_names() {
            let id = format!("profile:{name}");
            let n = m
                .disclosures
                .iter()
                .filter(|x| x.applies_to.contains(&id))
                .count();
            assert_eq!(n, 1, "{name}");
        }
        for a in [p.llm, p.mmproj.unwrap()] {
            assert_eq!(a.licence.as_deref(), Some("Apache-2.0"), "{}", a.file);
        }
    }

    #[test]
    fn disclosures_follow_the_provisioned_steps() {
        let m = Manifest::embedded();
        let ids: Vec<_> = m
            .disclosures_for(&["tts", "stt"])
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert!(ids.contains(&"supertonic-3-weights"));
        assert!(ids.contains(&"reazonspeech-k2-v2"));
        assert!(ids.contains(&"ai-performer"));
        assert!(!ids.contains(&"parakeet-ja"));
        assert!(!ids.contains(&"sample-vrm"));
    }

    #[test]
    fn a_wrong_schema_or_unknown_flavor_is_refused() {
        assert!(Manifest::parse("{\"schema\":\"x\"}").is_err());
        let m = Manifest::embedded();
        assert!(m.llama_asset("win32-x64", "metal").is_err());
        assert!(m.llama_asset("win32-x64", "vulkan").is_ok());
    }
}
