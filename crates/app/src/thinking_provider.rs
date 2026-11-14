//! Env resolution of the Thinking wire and the OpenRouter preset (issue #363).
//!
//! `AIVTUBER_THINKING_PROVIDER=openrouter` fills the endpoint, the
//! chat-completions wire, the attribution headers and the data-collection
//! default; every explicit `AIVTUBER_OPENAI_*` value still wins over it. The
//! API key is not handled here: it stays `AIVTUBER_OPENAI_API_KEY`.
//!
//! The function is pure over an env lookup so the precedence rules are tested
//! without touching the process environment.

use aivtuber_adapters::{DataCollection, ExtraHeaders, OpenAiResponsesConfig, ThinkingWire};

pub const OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const OPENROUTER_DEFAULT_TITLE: &str = "aivtuber";

/// Apply `AIVTUBER_THINKING_PROVIDER`, `AIVTUBER_OPENAI_WIRE` and the
/// OpenRouter knobs to `config`. Call it right after `default()` and before
/// the explicit endpoint/model overrides, so those keep precedence.
pub fn apply_thinking_provider_env(
    config: &mut OpenAiResponsesConfig,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let provider = get("AIVTUBER_THINKING_PROVIDER").map(|v| v.trim().to_ascii_lowercase());
    let openrouter = match provider.as_deref() {
        None | Some("") | Some("generic") => false,
        Some("openrouter") => true,
        Some(other) => {
            return Err(format!(
                "AIVTUBER_THINKING_PROVIDER must be openrouter or generic (got {other:?})"
            ));
        }
    };
    let referer = get("AIVTUBER_OPENROUTER_REFERER");
    let title = get("AIVTUBER_OPENROUTER_TITLE");
    let data_collection = get("AIVTUBER_OPENROUTER_DATA_COLLECTION");
    if !openrouter && (referer.is_some() || title.is_some() || data_collection.is_some()) {
        return Err(
            "AIVTUBER_OPENROUTER_REFERER/TITLE/DATA_COLLECTION need AIVTUBER_THINKING_PROVIDER=openrouter"
                .to_owned(),
        );
    }

    if openrouter {
        // The built-in default model is an OpenAI name, not an OpenRouter id
        // (`vendor/model`); require a deliberate choice instead of spending
        // credits on a guess.
        if get("AIVTUBER_OPENAI_MODEL").is_none() {
            return Err(
                "AIVTUBER_THINKING_PROVIDER=openrouter needs AIVTUBER_OPENAI_MODEL set to an OpenRouter model id (vendor/model)"
                    .to_owned(),
            );
        }
        config.endpoint = OPENROUTER_ENDPOINT.to_owned();
        config.wire = ThinkingWire::ChatCompletions;
        // No default Referer: the project is not public, so a URL is sent only
        // when the operator sets one.
        let mut pairs = Vec::with_capacity(2);
        if let Some(referer) = referer.as_deref() {
            pairs.push(("HTTP-Referer", referer));
        }
        pairs.push((
            "X-Title",
            title.as_deref().unwrap_or(OPENROUTER_DEFAULT_TITLE),
        ));
        config.extra_headers = ExtraHeaders::new(&pairs)
            .map_err(|error| format!("OpenRouter attribution headers: {error}"))?;
        config.data_collection = Some(match data_collection.as_deref() {
            None => DataCollection::Deny,
            Some(value) => DataCollection::parse(value).ok_or_else(|| {
                format!("AIVTUBER_OPENROUTER_DATA_COLLECTION must be deny or allow (got {value:?})")
            })?,
        });
    }

    if let Some(value) = get("AIVTUBER_OPENAI_WIRE") {
        config.wire = ThinkingWire::parse(&value).ok_or_else(|| {
            format!("AIVTUBER_OPENAI_WIRE must be responses or chat-completions (got {value:?})")
        })?;
    }
    if config.data_collection.is_some() && config.wire != ThinkingWire::ChatCompletions {
        return Err(
            "AIVTUBER_OPENROUTER_* needs AIVTUBER_OPENAI_WIRE=chat-completions (the openrouter preset sets it)"
                .to_owned(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn resolve(pairs: &[(&str, &str)]) -> Result<OpenAiResponsesConfig, String> {
        let env: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let mut config = OpenAiResponsesConfig::default();
        apply_thinking_provider_env(&mut config, &|name| env.get(name).cloned())?;
        Ok(config)
    }

    fn header<'a>(config: &'a OpenAiResponsesConfig, name: &str) -> Option<&'a str> {
        config
            .extra_headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v)
    }

    #[test]
    fn without_env_nothing_changes() {
        let config = resolve(&[]).expect("default");
        let default = OpenAiResponsesConfig::default();
        assert_eq!(config.wire, ThinkingWire::Responses);
        assert_eq!(config.endpoint, default.endpoint);
        assert!(config.extra_headers.is_empty());
        assert_eq!(config.data_collection, None);
        assert!(resolve(&[("AIVTUBER_THINKING_PROVIDER", "generic")]).is_ok());
    }

    #[test]
    fn the_openrouter_preset_fills_endpoint_wire_headers_and_denies_data_collection() {
        let config = resolve(&[
            ("AIVTUBER_THINKING_PROVIDER", "OpenRouter"),
            ("AIVTUBER_OPENAI_MODEL", "vendor/model"),
        ])
        .expect("preset");
        assert!(
            resolve(&[("AIVTUBER_THINKING_PROVIDER", "openrouter")]).is_err(),
            "the preset needs an explicit model"
        );
        assert_eq!(config.endpoint, OPENROUTER_ENDPOINT);
        assert_eq!(config.wire, ThinkingWire::ChatCompletions);
        assert_eq!(header(&config, "HTTP-Referer"), None);
        assert_eq!(config.extra_headers.len(), 1);
        assert_eq!(header(&config, "X-Title"), Some("aivtuber"));
        assert_eq!(config.data_collection, Some(DataCollection::Deny));
    }

    #[test]
    fn explicit_values_win_over_the_preset() {
        let config = resolve(&[
            ("AIVTUBER_THINKING_PROVIDER", "openrouter"),
            ("AIVTUBER_OPENAI_MODEL", "vendor/model"),
            ("AIVTUBER_OPENROUTER_REFERER", "https://example.invalid/me"),
            ("AIVTUBER_OPENROUTER_TITLE", "My VTuber"),
            ("AIVTUBER_OPENROUTER_DATA_COLLECTION", "allow"),
        ])
        .expect("overrides");
        assert_eq!(
            header(&config, "HTTP-Referer"),
            Some("https://example.invalid/me")
        );
        assert_eq!(header(&config, "X-Title"), Some("My VTuber"));
        assert_eq!(config.data_collection, Some(DataCollection::Allow));

        // An explicit wire overrides the preset's; the data-collection field
        // would then be dropped silently, so that combination is an error.
        let error = resolve(&[
            ("AIVTUBER_THINKING_PROVIDER", "openrouter"),
            ("AIVTUBER_OPENAI_MODEL", "vendor/model"),
            ("AIVTUBER_OPENAI_WIRE", "responses"),
        ])
        .expect_err("responses wire cannot carry provider preferences");
        assert!(error.contains("chat-completions"));
    }

    #[test]
    fn the_wire_can_be_chosen_without_a_provider_preset() {
        let config = resolve(&[("AIVTUBER_OPENAI_WIRE", "chat-completions")]).expect("wire");
        assert_eq!(config.wire, ThinkingWire::ChatCompletions);
        assert_eq!(
            config.data_collection, None,
            "generic chat sends no provider field"
        );
        assert!(resolve(&[("AIVTUBER_OPENAI_WIRE", "responses")]).is_ok());
    }

    #[test]
    fn unknown_values_and_orphan_openrouter_knobs_fail_at_startup() {
        for pairs in [
            &[("AIVTUBER_THINKING_PROVIDER", "anthropic")][..],
            &[("AIVTUBER_OPENAI_WIRE", "grpc")][..],
            &[
                ("AIVTUBER_THINKING_PROVIDER", "openrouter"),
                ("AIVTUBER_OPENAI_MODEL", "vendor/model"),
                ("AIVTUBER_OPENROUTER_DATA_COLLECTION", "maybe"),
            ][..],
            &[("AIVTUBER_OPENROUTER_TITLE", "x")][..],
            &[
                ("AIVTUBER_THINKING_PROVIDER", "openrouter"),
                ("AIVTUBER_OPENAI_MODEL", "vendor/model"),
                ("AIVTUBER_OPENROUTER_TITLE", "bad\r\nX-Evil: 1"),
            ][..],
        ] {
            assert!(resolve(pairs).is_err(), "accepted: {pairs:?}");
        }
    }
}
