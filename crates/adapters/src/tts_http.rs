use crate::{HttpTransportError, JsonHttpTransport, SecretString, UreqJsonTransport};
use aivtuber_domain::{
    BackendIdentity, EngineError, EngineErrorKind, EngineFuture, SpeechArtifact, SpeechRequest,
    TtsBackendIdentity, TtsEngine,
};
use serde::Deserialize;
use serde_json::json;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct NormalizedHttpTtsConfig {
    pub endpoint: String,
    pub backend_name: String,
    pub model_alias: Option<String>,
    pub model_version: Option<String>,
    pub voice_model: Option<String>,
    pub viseme_mapping: Option<String>,
    pub timeout: Duration,
}

impl fmt::Debug for NormalizedHttpTtsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NormalizedHttpTtsConfig")
            .field("endpoint", &self.endpoint)
            .field("backend_name", &self.backend_name)
            .field("model_alias", &self.model_alias)
            .field("model_version", &self.model_version)
            .field("voice_model", &self.voice_model)
            .field("viseme_mapping", &self.viseme_mapping)
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[derive(Clone)]
pub struct NormalizedHttpTtsAdapter {
    config: NormalizedHttpTtsConfig,
    api_key: Option<Arc<SecretString>>,
    transport: Arc<dyn JsonHttpTransport>,
}

impl fmt::Debug for NormalizedHttpTtsAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NormalizedHttpTtsAdapter")
            .field("config", &self.config)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl NormalizedHttpTtsAdapter {
    pub fn new(
        config: NormalizedHttpTtsConfig,
        api_key: Option<SecretString>,
    ) -> Result<Self, EngineError> {
        Self::with_transport(config, api_key, Arc::new(UreqJsonTransport::default()))
    }

    pub fn with_transport(
        config: NormalizedHttpTtsConfig,
        api_key: Option<SecretString>,
        transport: Arc<dyn JsonHttpTransport>,
    ) -> Result<Self, EngineError> {
        validate_config(&config)?;
        Ok(Self {
            config,
            api_key: api_key.map(Arc::new),
            transport,
        })
    }

    pub fn synthesize_sync(&self, request: &SpeechRequest) -> Result<SpeechArtifact, EngineError> {
        if request.text.trim().is_empty() {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "TTS text must not be empty",
            ));
        }

        let body = serde_json::to_vec(&json!({
            "text": request.text,
            "style": request.style,
            "voice_model": self.config.voice_model,
            "viseme_mapping": self.config.viseme_mapping,
        }))
        .map_err(|error| EngineError::new(EngineErrorKind::InvalidRequest, error.to_string()))?;

        let response = self
            .transport
            .post_json(
                &self.config.endpoint,
                self.api_key.as_ref().map(|secret| secret.expose()),
                &body,
                self.config.timeout,
            )
            .map_err(map_transport_error)?;

        if response.status != 200 {
            return Err(map_status(response.status));
        }

        let payload: NormalizedTtsResponse =
            serde_json::from_slice(&response.body).map_err(|error| {
                EngineError::new(
                    EngineErrorKind::Backend,
                    format!("invalid normalized TTS response JSON: {error}"),
                )
            })?;
        if payload.audio_ref.trim().is_empty() || payload.duration_ms == 0 {
            return Err(EngineError::new(
                EngineErrorKind::Backend,
                "normalized TTS response requires audio_ref and positive duration_ms",
            ));
        }

        Ok(SpeechArtifact {
            audio_ref: payload.audio_ref,
            duration_ms: payload.duration_ms,
            viseme_ref: payload.viseme_ref,
        })
    }
}

impl TtsEngine for NormalizedHttpTtsAdapter {
    fn synthesize<'a>(&'a self, request: &'a SpeechRequest) -> EngineFuture<'a, SpeechArtifact> {
        Box::pin(async move { self.synthesize_sync(request) })
    }

    fn identity(&self) -> TtsBackendIdentity {
        TtsBackendIdentity {
            backend: BackendIdentity {
                name: self.config.backend_name.clone(),
                model_alias: self.config.model_alias.clone(),
                model_version: self.config.model_version.clone(),
            },
            voice_model: self.config.voice_model.clone(),
            viseme_mapping: self.config.viseme_mapping.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalizedTtsResponse {
    audio_ref: String,
    duration_ms: u64,
    #[serde(default)]
    viseme_ref: Option<String>,
}

fn validate_config(config: &NormalizedHttpTtsConfig) -> Result<(), EngineError> {
    if !(config.endpoint.starts_with("https://")
        || config.endpoint.starts_with("http://127.0.0.1")
        || config.endpoint.starts_with("http://localhost"))
    {
        return Err(EngineError::new(
            EngineErrorKind::InvalidRequest,
            "TTS endpoint must use HTTPS or localhost HTTP",
        ));
    }
    if config.backend_name.trim().is_empty() || config.timeout.is_zero() {
        return Err(EngineError::new(
            EngineErrorKind::InvalidRequest,
            "TTS backend_name and timeout must be non-empty",
        ));
    }
    Ok(())
}

fn map_transport_error(error: HttpTransportError) -> EngineError {
    match error {
        HttpTransportError::Timeout => {
            EngineError::new(EngineErrorKind::Timeout, error.to_string())
        }
        HttpTransportError::Unavailable(_) => {
            EngineError::new(EngineErrorKind::Unavailable, error.to_string())
        }
    }
}

fn map_status(status: u16) -> EngineError {
    let kind = match status {
        400 | 404 | 422 => EngineErrorKind::InvalidRequest,
        401 | 403 => EngineErrorKind::Authentication,
        429 => EngineErrorKind::RateLimited,
        503 | 529 => EngineErrorKind::Overloaded,
        _ if status >= 500 => EngineErrorKind::Unavailable,
        _ => EngineErrorKind::Backend,
    };
    EngineError::new(kind, format!("normalized TTS HTTP {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HttpResponse;
    use crate::http::test_support::MockHttpTransport;

    fn config() -> NormalizedHttpTtsConfig {
        NormalizedHttpTtsConfig {
            endpoint: "https://tts.example.invalid/v1/synthesize".to_owned(),
            backend_name: "example-tts".to_owned(),
            model_alias: Some("tts-fast".to_owned()),
            model_version: Some("2026-09".to_owned()),
            voice_model: Some("voice-ja-v2".to_owned()),
            viseme_mapping: Some("ja-5vowel-v2".to_owned()),
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn normalized_tts_maps_request_response_and_identity() {
        let transport = MockHttpTransport::new(Ok(HttpResponse {
            status: 200,
            body: serde_json::to_vec(&json!({
                "audio_ref": "audio://generated/test.opus",
                "duration_ms": 1234,
                "viseme_ref": "viseme/generated/test.json"
            }))
            .expect("response JSON"),
            content_type: Some("application/json".to_owned()),
        }));
        let adapter = NormalizedHttpTtsAdapter::with_transport(
            config(),
            Some(SecretString::new("tts-secret-value")),
            Arc::new(transport.clone()),
        )
        .expect("adapter");

        let artifact = adapter
            .synthesize_sync(&SpeechRequest {
                text: "生成音声です".to_owned(),
                style: Some("cheerful".to_owned()),
            })
            .expect("speech");
        assert_eq!(artifact.audio_ref, "audio://generated/test.opus");
        assert_eq!(artifact.duration_ms, 1234);
        assert_eq!(
            artifact.viseme_ref.as_deref(),
            Some("viseme/generated/test.json")
        );

        let requests = transport.requests();
        assert_eq!(
            requests[0].bearer_token.as_deref(),
            Some("tts-secret-value")
        );
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("request JSON");
        assert_eq!(body["text"], "生成音声です");
        assert_eq!(body["style"], "cheerful");
        assert_eq!(body["voice_model"], "voice-ja-v2");
        assert_eq!(body["viseme_mapping"], "ja-5vowel-v2");
        assert!(!String::from_utf8_lossy(&requests[0].body).contains("tts-secret-value"));

        let identity = adapter.identity();
        assert_eq!(identity.backend.name, "example-tts");
        assert_eq!(identity.backend.model_alias.as_deref(), Some("tts-fast"));
        assert_eq!(identity.backend.model_version.as_deref(), Some("2026-09"));
        assert_eq!(identity.voice_model.as_deref(), Some("voice-ja-v2"));
        assert_eq!(identity.viseme_mapping.as_deref(), Some("ja-5vowel-v2"));
    }

    #[test]
    fn normalized_tts_debug_redacts_key_and_rejects_empty_text() {
        let transport = MockHttpTransport::new(Ok(HttpResponse {
            status: 200,
            body: b"{}".to_vec(),
            content_type: Some("application/json".to_owned()),
        }));
        let adapter = NormalizedHttpTtsAdapter::with_transport(
            config(),
            Some(SecretString::new("tts-secret-value")),
            Arc::new(transport),
        )
        .expect("adapter");

        let debug = format!("{adapter:?}");
        assert!(!debug.contains("tts-secret-value"));
        assert!(debug.contains("REDACTED"));

        let error = adapter
            .synthesize_sync(&SpeechRequest {
                text: "   ".to_owned(),
                style: None,
            })
            .expect_err("empty text");
        assert_eq!(error.kind, EngineErrorKind::InvalidRequest);
    }
}
