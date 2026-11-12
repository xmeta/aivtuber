//! The bounded set of local Thinking models an operator may switch between,
//! and the small settings file that remembers the choice (issue #357).
//!
//! The launcher (`aivtuber-setup start`) builds the catalog from the models it
//! found on disk and hands it to the app; the app refuses to select anything
//! outside it. Both sides share this module so they cannot disagree about what
//! a valid alias is. The settings file holds an alias and nothing else: never a
//! path, never free text, and it is only ever trusted after it has been
//! checked against the catalog the launcher computed for this run. This module
//! only parses and renders the document; reading and writing the file is done
//! by the app (write) and the launcher (read), not by the domain.

use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_THINKING_MODEL_ALIAS_CHARS: usize = 64;
pub const MAX_THINKING_MODEL_VERSION_CHARS: usize = 32;
pub const MAX_THINKING_MODEL_CHOICES: usize = 8;
/// A settings file larger than this is ignored, not read.
pub const MAX_MODEL_SETTINGS_BYTES: u64 = 512;
pub const MODEL_SETTINGS_SCHEMA: &str = "local-model-settings/1";

/// Lowercase ASCII letters, digits, `.`, `_` and `-`; starts with a letter or
/// digit. Narrow on purpose: the alias is sent to the model server and shown
/// in audit records, so it can never carry markup, paths or control text.
pub fn is_valid_model_alias(alias: &str) -> bool {
    let mut chars = alias.chars();
    alias.chars().count() <= MAX_THINKING_MODEL_ALIAS_CHARS
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && alias
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Letters, digits, `.`, `_` and `-` (quantization names such as `UD-Q4_K_XL`).
pub fn is_valid_model_version(version: &str) -> bool {
    !version.is_empty()
        && version.chars().count() <= MAX_THINKING_MODEL_VERSION_CHARS
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingModelError {
    InvalidAlias,
    InvalidVersion,
    Empty,
    TooMany,
    Duplicate(String),
    /// The alias is well formed but not in the catalog.
    NotInstalled,
    MalformedChoice,
}

impl fmt::Display for ThinkingModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAlias => write!(
                f,
                "a model alias is 1..={MAX_THINKING_MODEL_ALIAS_CHARS} characters of a-z, 0-9, '.', '_' or '-'"
            ),
            Self::InvalidVersion => write!(
                f,
                "a model version is 1..={MAX_THINKING_MODEL_VERSION_CHARS} characters of letters, digits, '.', '_' or '-'"
            ),
            Self::Empty => f.write_str("the model list is empty"),
            Self::TooMany => write!(f, "at most {MAX_THINKING_MODEL_CHOICES} models"),
            Self::Duplicate(alias) => write!(f, "model {alias:?} is listed twice"),
            Self::NotInstalled => f.write_str("that model is not installed"),
            Self::MalformedChoice => f.write_str("expected alias:version"),
        }
    }
}

impl std::error::Error for ThinkingModelError {}

/// One selectable model: the name the model server routes on and the version
/// string recorded in disclosure and audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingModel {
    pub alias: String,
    pub version: String,
}

impl ThinkingModel {
    pub fn new(alias: &str, version: &str) -> Result<Self, ThinkingModelError> {
        if !is_valid_model_alias(alias) {
            return Err(ThinkingModelError::InvalidAlias);
        }
        if !is_valid_model_version(version) {
            return Err(ThinkingModelError::InvalidVersion);
        }
        Ok(Self {
            alias: alias.to_owned(),
            version: version.to_owned(),
        })
    }
}

/// The allowlist. Never empty, bounded, aliases unique.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingModelCatalog {
    models: Vec<ThinkingModel>,
}

impl ThinkingModelCatalog {
    pub fn new(models: Vec<ThinkingModel>) -> Result<Self, ThinkingModelError> {
        if models.is_empty() {
            return Err(ThinkingModelError::Empty);
        }
        if models.len() > MAX_THINKING_MODEL_CHOICES {
            return Err(ThinkingModelError::TooMany);
        }
        for (index, model) in models.iter().enumerate() {
            if models[..index].iter().any(|m| m.alias == model.alias) {
                return Err(ThinkingModelError::Duplicate(model.alias.clone()));
            }
        }
        Ok(Self { models })
    }

    /// `alias:version[,alias:version...]`, the form carried in
    /// `AIVTUBER_OPENAI_MODEL_CHOICES`.
    pub fn parse_choices(text: &str) -> Result<Self, ThinkingModelError> {
        let models = text
            .split(',')
            .map(|part| {
                let (alias, version) = part
                    .trim()
                    .split_once(':')
                    .ok_or(ThinkingModelError::MalformedChoice)?;
                ThinkingModel::new(alias, version)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(models)
    }

    pub fn render_choices(&self) -> String {
        self.models
            .iter()
            .map(|m| format!("{}:{}", m.alias, m.version))
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn models(&self) -> &[ThinkingModel] {
        &self.models
    }

    pub fn get(&self, alias: &str) -> Option<&ThinkingModel> {
        self.models.iter().find(|m| m.alias == alias)
    }

    /// The only way a requested alias becomes a model: a malformed alias and a
    /// well-formed but unknown one are different refusals.
    pub fn select(&self, alias: &str) -> Result<&ThinkingModel, ThinkingModelError> {
        if !is_valid_model_alias(alias) {
            return Err(ThinkingModelError::InvalidAlias);
        }
        self.get(alias).ok_or(ThinkingModelError::NotInstalled)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    schema: String,
    model: String,
}

/// The alias a settings document names, or `None` for anything that is not
/// exactly the documented shape with a well-formed alias.
pub fn parse_model_settings(bytes: &[u8]) -> Option<String> {
    if bytes.len() as u64 > MAX_MODEL_SETTINGS_BYTES {
        return None;
    }
    let file: SettingsFile = serde_json::from_slice(bytes).ok()?;
    (file.schema == MODEL_SETTINGS_SCHEMA && is_valid_model_alias(&file.model))
        .then_some(file.model)
}

pub fn render_model_settings(alias: &str) -> Result<String, ThinkingModelError> {
    if !is_valid_model_alias(alias) {
        return Err(ThinkingModelError::InvalidAlias);
    }
    let mut text = serde_json::to_string(&SettingsFile {
        schema: MODEL_SETTINGS_SCHEMA.to_owned(),
        model: alias.to_owned(),
    })
    .map_err(|_| ThinkingModelError::InvalidAlias)?;
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> ThinkingModelCatalog {
        ThinkingModelCatalog::parse_choices(
            "gemma-4-e4b-it-qat:UD-Q4_K_XL,qwen3-4b-instruct-2507:Q4_K_M",
        )
        .unwrap()
    }

    #[test]
    fn aliases_are_narrow() {
        for ok in ["gemma-4-e4b-it-qat", "a", "qwen3.5_4b", &"a".repeat(64)] {
            assert!(is_valid_model_alias(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            "Gemma",
            "a b",
            "a/b",
            "..\\x",
            "x\n",
            "モデル",
            &"a".repeat(65),
        ] {
            assert!(!is_valid_model_alias(bad), "{bad:?}");
        }
        assert!(is_valid_model_version("UD-Q4_K_XL"));
        assert!(!is_valid_model_version("") && !is_valid_model_version("Q4 K"));
    }

    #[test]
    fn the_catalog_is_bounded_unique_and_round_trips() {
        let c = catalog();
        assert_eq!(c.models().len(), 2);
        assert_eq!(
            ThinkingModelCatalog::parse_choices(&c.render_choices()).unwrap(),
            c
        );
        assert_eq!(
            ThinkingModelCatalog::parse_choices("a:1,a:2"),
            Err(ThinkingModelError::Duplicate("a".to_owned()))
        );
        assert_eq!(
            ThinkingModelCatalog::parse_choices(""),
            Err(ThinkingModelError::MalformedChoice)
        );
        assert_eq!(
            ThinkingModelCatalog::parse_choices("a"),
            Err(ThinkingModelError::MalformedChoice)
        );
        let nine = (0..9)
            .map(|i| format!("m{i}:v"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            ThinkingModelCatalog::parse_choices(&nine),
            Err(ThinkingModelError::TooMany)
        );
        assert_eq!(
            ThinkingModelCatalog::new(Vec::new()),
            Err(ThinkingModelError::Empty)
        );
    }

    #[test]
    fn select_separates_malformed_from_unknown() {
        let c = catalog();
        assert_eq!(
            c.select("qwen3-4b-instruct-2507").unwrap().version,
            "Q4_K_M"
        );
        assert_eq!(c.select("other"), Err(ThinkingModelError::NotInstalled));
        assert_eq!(c.select("../x"), Err(ThinkingModelError::InvalidAlias));
        assert_eq!(c.select(""), Err(ThinkingModelError::InvalidAlias));
    }

    #[test]
    fn the_settings_document_has_one_shape() {
        let text = render_model_settings("qwen3-4b-instruct-2507").unwrap();
        assert_eq!(
            parse_model_settings(text.as_bytes()).as_deref(),
            Some("qwen3-4b-instruct-2507")
        );
        for bad in [
            &b""[..],
            b"not json",
            br#"{"schema":"local-model-settings/1"}"#,
            br#"{"schema":"other/1","model":"a"}"#,
            br#"{"schema":"local-model-settings/1","model":"C:\\evil.gguf"}"#,
            br#"{"schema":"local-model-settings/1","model":"a","extra":1}"#,
            br#"{"schema":"local-model-settings/1","model":"A B"}"#,
        ] {
            assert_eq!(
                parse_model_settings(bad),
                None,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let huge = format!(
            r#"{{"schema":"local-model-settings/1","model":"a","pad":"{}"}}"#,
            "x".repeat(600)
        );
        assert_eq!(parse_model_settings(huge.as_bytes()), None);
        assert!(render_model_settings("Bad Alias").is_err());
    }
}
