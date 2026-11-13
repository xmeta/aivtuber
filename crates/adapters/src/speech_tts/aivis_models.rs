//! Aivis Cloud model search and API-key check for the operation panel
//! (issue #360).
//!
//! Two small read-only calls beside the synthesis one in [`super::aivis_cloud`]:
//!
//! * `GET /v1/aivm-models/search` is public (no key is sent). The answer is
//!   untrusted: it is mapped into a minimal typed DTO ([`ModelSearchPage`]) and
//!   nothing else of the wire reaches the panel. Every text is length-capped
//!   and stripped of control characters, identifiers are validated (UUIDs, style
//!   ids 0..=31), the number of models, speakers and styles is capped, and the
//!   response is read through a byte limit.
//! * `GET /v1/users/me` is the cheapest authenticated call: it tells the
//!   operator whether the key is valid. Only the HTTP status is looked at; the
//!   body is never read, logged or returned.
//!
//! Both go through the same origin rule as synthesis ([`validate_origin`]: TLS
//! for any remote host, loopback `http://` only for a test fake, no
//! credentials), never follow a redirect, run under one deadline and return
//! fixed-string errors that never carry a key, a URL or a body.

use super::aivis_cloud::{MAX_STYLE_ID, is_uuid, validate_origin};
use crate::SecretString;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Read;
use std::time::Duration;

pub const SEARCH_ROUTE: &str = "/v1/aivm-models/search";
pub const ME_ROUTE: &str = "/v1/users/me";
/// At most this many models per page (the API's default page size).
pub const MAX_SEARCH_RESULTS: usize = 24;
/// Largest search answer read. 24 models measured at about 280 KB.
pub const MAX_SEARCH_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_KEYWORD_CHARS: usize = 64;
pub const MAX_SEARCH_PAGE: u32 = 100;
const MAX_ME_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_DESCRIPTION_CHARS: usize = 400;
const MAX_NAME_CHARS: usize = 80;
const MAX_LABEL_CHARS: usize = 40;
const MAX_SPEAKERS: usize = 8;
const MAX_STYLES: usize = 32;
const MAX_LICENSES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelSort {
    Download,
    Like,
    Recent,
}

impl ModelSort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Like => "like",
            Self::Recent => "recent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSearchQuery {
    pub keyword: String,
    pub sort: ModelSort,
    /// 1-based.
    pub page: u32,
}

impl ModelSearchQuery {
    /// A trimmed keyword of at most [`MAX_KEYWORD_CHARS`] without control
    /// characters, and a page in `1..=MAX_SEARCH_PAGE`.
    pub fn new(keyword: &str, sort: ModelSort, page: u32) -> Result<Self, ModelsError> {
        let keyword = keyword.trim();
        if keyword.chars().count() > MAX_KEYWORD_CHARS || keyword.chars().any(char::is_control) {
            return Err(ModelsError::BadQuery);
        }
        if !(1..=MAX_SEARCH_PAGE).contains(&page) {
            return Err(ModelsError::BadQuery);
        }
        Ok(Self {
            keyword: keyword.to_owned(),
            sort,
            page,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsError {
    BadQuery,
    /// The configured origin was refused.
    Origin,
    Timeout,
    Unreachable,
    /// A non-success status from the service.
    Status(u16),
    TooLarge,
    /// The answer is not the documented shape.
    Malformed,
}

impl fmt::Display for ModelsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadQuery => f.write_str("the search request is not valid"),
            Self::Origin => f.write_str("the Aivis Cloud endpoint is not allowed"),
            Self::Timeout => f.write_str("Aivis Cloud did not answer in time"),
            Self::Unreachable => f.write_str("Aivis Cloud is unreachable"),
            Self::Status(status) => write!(f, "Aivis Cloud answered HTTP {status}"),
            Self::TooLarge => f.write_str("the answer is too large"),
            Self::Malformed => f.write_str("the answer is not what was expected"),
        }
    }
}

impl std::error::Error for ModelsError {}

/// What the key check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCheck {
    Valid,
    /// 401 / 403: the service does not accept the key.
    Rejected,
    /// Timeout, refused connection, DNS: nothing was learned about the key.
    NetworkError,
    /// Any other status (for example an outage): nothing was learned either.
    Unexpected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VoiceStyleDto {
    pub name: String,
    /// The `style_id` the synthesis call takes (0..=31).
    pub style_id: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VoiceSpeakerDto {
    pub uuid: String,
    pub name: String,
    pub styles: Vec<VoiceStyleDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VoiceModelDto {
    pub uuid: String,
    pub name: String,
    /// Free text from the model's author (credit lines usually live here).
    /// Untrusted: shown as text only.
    pub description: String,
    pub category: String,
    pub voice_timbre: String,
    /// Distinct licence names of the model's files, e.g. `ACML 1.0`.
    pub licenses: Vec<String>,
    pub speakers: Vec<VoiceSpeakerDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelSearchPage {
    pub total: u64,
    pub page: u32,
    pub models: Vec<VoiceModelDto>,
}

// The wire shape: only what is read. Everything is optional so an unrelated
// change on the service side cannot fail a search; unknown fields are ignored.
#[derive(Deserialize)]
struct WireSearch {
    #[serde(default)]
    total: u64,
    #[serde(default)]
    aivm_models: Vec<WireModel>,
}

#[derive(Deserialize)]
struct WireModel {
    #[serde(default)]
    aivm_model_uuid: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    voice_timbre: Option<String>,
    #[serde(default)]
    model_files: Vec<WireFile>,
    #[serde(default)]
    speakers: Vec<WireSpeaker>,
}

#[derive(Deserialize)]
struct WireFile {
    #[serde(default)]
    license_type: Option<String>,
}

#[derive(Deserialize)]
struct WireSpeaker {
    #[serde(default)]
    aivm_speaker_uuid: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    styles: Vec<WireStyle>,
}

#[derive(Deserialize)]
struct WireStyle {
    #[serde(default)]
    name: String,
    #[serde(default)]
    local_id: Option<u64>,
}

/// Control characters become spaces (newlines are kept where `multiline`),
/// then the text is trimmed and cut to `max` characters.
fn clean(text: &str, max: usize, multiline: bool) -> String {
    let text: String = text
        .chars()
        .map(|c| {
            if c == '\n' && multiline {
                c
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    text.trim().chars().take(max).collect()
}

/// Maps the service's answer into the DTO. A model without a valid UUID or a
/// name is dropped; so is a speaker without a valid UUID and a style outside
/// the API's range, so what reaches the panel can always be sent back to
/// synthesis.
pub fn parse_model_search(bytes: &[u8], page: u32) -> Result<ModelSearchPage, ModelsError> {
    if bytes.len() > MAX_SEARCH_RESPONSE_BYTES {
        return Err(ModelsError::TooLarge);
    }
    // A derived struct would also accept a JSON array; only an object is a page.
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(ModelsError::Malformed);
    }
    let wire: WireSearch = serde_json::from_slice(bytes).map_err(|_| ModelsError::Malformed)?;
    let models = wire
        .aivm_models
        .into_iter()
        .filter(|m| {
            is_uuid(&m.aivm_model_uuid) && !clean(&m.name, MAX_NAME_CHARS, false).is_empty()
        })
        .take(MAX_SEARCH_RESULTS)
        .map(|m| {
            let mut licenses: Vec<String> = Vec::new();
            for license in m
                .model_files
                .iter()
                .filter_map(|f| f.license_type.as_deref())
            {
                let license = clean(license, MAX_LABEL_CHARS, false);
                if !license.is_empty()
                    && !licenses.contains(&license)
                    && licenses.len() < MAX_LICENSES
                {
                    licenses.push(license);
                }
            }
            let speakers = m
                .speakers
                .into_iter()
                .filter(|s| is_uuid(&s.aivm_speaker_uuid))
                .take(MAX_SPEAKERS)
                .map(|s| VoiceSpeakerDto {
                    uuid: s.aivm_speaker_uuid,
                    name: clean(&s.name, MAX_NAME_CHARS, false),
                    styles: s
                        .styles
                        .into_iter()
                        .filter_map(|style| {
                            let id = u8::try_from(style.local_id?).ok()?;
                            (id <= MAX_STYLE_ID).then(|| VoiceStyleDto {
                                name: clean(&style.name, MAX_NAME_CHARS, false),
                                style_id: id,
                            })
                        })
                        .take(MAX_STYLES)
                        .collect(),
                })
                .collect();
            VoiceModelDto {
                uuid: m.aivm_model_uuid,
                name: clean(&m.name, MAX_NAME_CHARS, false),
                description: clean(&m.description, MAX_DESCRIPTION_CHARS, true),
                category: clean(m.category.as_deref().unwrap_or(""), MAX_LABEL_CHARS, false),
                voice_timbre: clean(
                    m.voice_timbre.as_deref().unwrap_or(""),
                    MAX_LABEL_CHARS,
                    false,
                ),
                licenses,
                speakers,
            }
        })
        .collect();
    Ok(ModelSearchPage {
        total: wire.total,
        page,
        models,
    })
}

/// One bounded GET. `bearer` is sent as the `Authorization` header only.
fn get(
    url: &url::Url,
    bearer: Option<&SecretString>,
    timeout: Duration,
    cap: usize,
) -> Result<(u16, Vec<u8>), ModelsError> {
    let agent = ureq::Agent::new_with_defaults();
    let configured = agent
        .get(url.as_str())
        .config()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        // A redirect would carry a bearer key to another host.
        .max_redirects(0);
    // A loopback fake is never reached through a proxy from the environment.
    let configured = if url.scheme() == "http" {
        configured.proxy(None)
    } else {
        configured
    };
    let mut request = configured.build();
    if let Some(key) = bearer {
        request = request.header("authorization", format!("Bearer {}", key.expose()));
    }
    let mut response = request.call().map_err(|error| match error {
        ureq::Error::Timeout(_) => ModelsError::Timeout,
        // Never `error.to_string()`: keep the request details out of the log.
        _ => ModelsError::Unreachable,
    })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Ok((status, Vec::new()));
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .with_config()
        .limit(cap as u64 + 1)
        .reader()
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                ModelsError::Timeout
            } else {
                ModelsError::Unreachable
            }
        })?;
    if bytes.len() > cap {
        return Err(ModelsError::TooLarge);
    }
    Ok((status, bytes))
}

/// Search the public model list. No key is sent.
pub fn search_models(
    endpoint: &str,
    query: &ModelSearchQuery,
    timeout: Duration,
) -> Result<ModelSearchPage, ModelsError> {
    let origin = validate_origin(endpoint).map_err(|_| ModelsError::Origin)?;
    let mut url = origin.join(SEARCH_ROUTE).map_err(|_| ModelsError::Origin)?;
    {
        let mut pairs = url.query_pairs_mut();
        if !query.keyword.is_empty() {
            pairs.append_pair("keyword", &query.keyword);
        }
        pairs.append_pair("sort", query.sort.as_str());
        pairs.append_pair("limit", &MAX_SEARCH_RESULTS.to_string());
        if query.page > 1 {
            pairs.append_pair("page", &query.page.to_string());
        }
    }
    let (status, body) = get(&url, None, timeout, MAX_SEARCH_RESPONSE_BYTES)?;
    if status != 200 {
        return Err(ModelsError::Status(status));
    }
    parse_model_search(&body, query.page)
}

/// Ask the service whether it accepts `key`. Only the status is used.
pub fn check_api_key(
    endpoint: &str,
    key: &SecretString,
    timeout: Duration,
) -> Result<KeyCheck, ModelsError> {
    let origin = validate_origin(endpoint).map_err(|_| ModelsError::Origin)?;
    let url = origin.join(ME_ROUTE).map_err(|_| ModelsError::Origin)?;
    Ok(match get(&url, Some(key), timeout, MAX_ME_RESPONSE_BYTES) {
        Ok((200..=299, _)) => KeyCheck::Valid,
        Ok((401 | 403, _)) => KeyCheck::Rejected,
        Ok(_) | Err(ModelsError::TooLarge | ModelsError::Malformed) => KeyCheck::Unexpected,
        Err(_) => KeyCheck::NetworkError,
    })
}

#[cfg(test)]
mod tests {
    use super::super::aivis_cloud_fake::{FakeAivisBehaviour, FakeAivisCloud};
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/aivis-model-search.json");
    const KEY: &str = "sk-models-SECRETKEY-789";
    const TIMEOUT: Duration = Duration::from_secs(5);

    fn query(keyword: &str) -> ModelSearchQuery {
        ModelSearchQuery::new(keyword, ModelSort::Download, 1).unwrap()
    }

    #[test]
    fn the_recorded_answer_maps_to_the_minimal_dto() {
        let page = parse_model_search(FIXTURE, 1).unwrap();
        assert_eq!((page.total, page.page, page.models.len()), (77, 1, 3));
        let mao = &page.models[0];
        assert_eq!(mao.uuid, "a59cb814-0083-4369-8542-f51a29e72af7");
        assert_eq!(mao.name, "まお");
        assert_eq!(mao.licenses, ["ACML 1.0"]);
        assert!(mao.description.contains("AivisSpeech: まお"));
        assert_eq!(mao.speakers.len(), 1);
        assert_eq!(mao.speakers[0].uuid, "e756b8e4-b606-4e15-99b1-3f9c6a1b2317");
        assert_eq!(mao.speakers[0].styles.len(), 6);
        assert_eq!(mao.speakers[0].styles[0].name, "ノーマル");
        assert_eq!(mao.speakers[0].styles[0].style_id, 0);
        let licenses: Vec<&str> = page
            .models
            .iter()
            .flat_map(|m| m.licenses.iter().map(String::as_str))
            .collect();
        assert_eq!(licenses, ["ACML 1.0", "ACML-NC 1.0", "Custom"]);
        // Nothing but the DTO fields reaches the panel: no user, icon or sample URL.
        let json = serde_json::to_string(&page).unwrap();
        for absent in [
            "icon_url",
            "audio_url",
            "ozchat",
            "handle",
            "transcript",
            "user",
        ] {
            assert!(!json.contains(absent), "{absent}");
        }
    }

    #[test]
    fn hostile_answers_are_bounded_cleaned_and_filtered() {
        let long = "あ".repeat(5000);
        let models: Vec<serde_json::Value> = (0..40)
            .map(|i| {
                serde_json::json!({
                    "aivm_model_uuid": format!("00000000-0000-0000-0000-{i:012}"),
                    "name": format!("m{i}\u{0007}\n<script>alert(1)</script>"),
                    "description": format!("{long}\u{0000}"),
                    "model_files": [{"license_type": "A"}, {"license_type": "B"},
                        {"license_type": "C"}, {"license_type": "D"}, {"license_type": "A"}],
                    "speakers": (0..20).map(|s| serde_json::json!({
                        "aivm_speaker_uuid": format!("00000000-0000-0000-0001-{s:012}"),
                        "name": "s",
                        "styles": (0..60).map(|t| serde_json::json!({"name": "t", "local_id": t}))
                            .collect::<Vec<_>>()
                    })).chain([
                        serde_json::json!({"aivm_speaker_uuid": "not-a-uuid", "name": "bad", "styles": []}),
                    ]).collect::<Vec<_>>(),
                })
            })
            .chain([
                serde_json::json!({"aivm_model_uuid": "nope", "name": "bad uuid"}),
                serde_json::json!({"aivm_model_uuid": "00000000-0000-0000-0000-999999999999", "name": "  "}),
            ])
            .collect();
        let body = serde_json::to_vec(&serde_json::json!({"total": 99999, "aivm_models": models}))
            .unwrap();
        let page = parse_model_search(&body, 3).unwrap();
        assert_eq!(page.page, 3);
        assert_eq!(page.models.len(), MAX_SEARCH_RESULTS);
        let m = &page.models[0];
        assert!(m.description.chars().count() <= MAX_DESCRIPTION_CHARS);
        assert!(!m.name.contains('\u{0007}'));
        assert!(!m.name.contains('\n'));
        // Markup is not interpreted here; it is only ever shown as text.
        assert!(m.name.contains("<script>"));
        assert_eq!(m.licenses, ["A", "B", "C"]);
        assert_eq!(m.speakers.len(), MAX_SPEAKERS);
        assert_eq!(m.speakers[0].styles.len(), MAX_STYLES);
        assert!(
            m.speakers[0]
                .styles
                .iter()
                .all(|s| s.style_id <= MAX_STYLE_ID)
        );
        assert!(page.models.iter().all(|m| is_uuid(&m.uuid)));
        // Over the byte bound, or not JSON, is an error and not a panic.
        assert_eq!(
            parse_model_search(&vec![b' '; MAX_SEARCH_RESPONSE_BYTES + 1], 1),
            Err(ModelsError::TooLarge)
        );
        for bad in [&b"not json"[..], b"[]", b"{\"aivm_models\": 3}", b""] {
            assert_eq!(parse_model_search(bad, 1), Err(ModelsError::Malformed));
        }
        // An empty or unrelated object is an empty page.
        assert!(parse_model_search(b"{}", 1).unwrap().models.is_empty());
    }

    #[test]
    fn the_query_is_bounded_and_encoded() {
        assert!(ModelSearchQuery::new(&"a".repeat(65), ModelSort::Like, 1).is_err());
        assert!(ModelSearchQuery::new("a\nb", ModelSort::Like, 1).is_err());
        assert!(ModelSearchQuery::new("a", ModelSort::Like, 0).is_err());
        assert!(ModelSearchQuery::new("a", ModelSort::Like, MAX_SEARCH_PAGE + 1).is_err());
        let api = FakeAivisCloud::start(FakeAivisBehaviour {
            search_body: Some(FIXTURE.to_vec()),
            ..FakeAivisBehaviour::default()
        });
        let q = ModelSearchQuery::new("  ツン&デレ=1 ", ModelSort::Recent, 2).unwrap();
        let page = search_models(&api.endpoint(), &q, TIMEOUT).unwrap();
        assert_eq!(page.page, 2);
        let requests = api.requests();
        let request = &requests[0];
        assert_eq!(request.route(), SEARCH_ROUTE);
        assert_eq!(
            request.query_param("keyword").as_deref(),
            Some("ツン&デレ=1")
        );
        assert_eq!(request.query_param("sort").as_deref(), Some("recent"));
        assert_eq!(request.query_param("limit").as_deref(), Some("24"));
        assert_eq!(request.query_param("page").as_deref(), Some("2"));
        // The search is public: no credential is sent.
        assert!(request.header("authorization").is_none());
    }

    #[test]
    fn a_failing_or_foreign_service_is_a_fixed_error() {
        let down = FakeAivisCloud::start(FakeAivisBehaviour {
            search_status: 503,
            ..FakeAivisBehaviour::default()
        });
        assert_eq!(
            search_models(&down.endpoint(), &query(""), TIMEOUT),
            Err(ModelsError::Status(503))
        );
        let big = FakeAivisCloud::start(FakeAivisBehaviour {
            search_body: Some(vec![b'{'; MAX_SEARCH_RESPONSE_BYTES + 10]),
            ..FakeAivisBehaviour::default()
        });
        assert_eq!(
            search_models(&big.endpoint(), &query(""), TIMEOUT),
            Err(ModelsError::TooLarge)
        );
        let slow = FakeAivisCloud::start(FakeAivisBehaviour {
            delay: Duration::from_millis(600),
            ..FakeAivisBehaviour::default()
        });
        assert_eq!(
            search_models(&slow.endpoint(), &query(""), Duration::from_millis(100)),
            Err(ModelsError::Timeout)
        );
        assert_eq!(
            search_models("http://127.0.0.1:9", &query(""), TIMEOUT),
            Err(ModelsError::Unreachable)
        );
        for foreign in [
            "http://example.com",
            "https://u:p@example.com",
            "ftp://x",
            "",
        ] {
            assert_eq!(
                search_models(foreign, &query(""), TIMEOUT),
                Err(ModelsError::Origin),
                "{foreign}"
            );
        }
    }

    #[test]
    fn the_key_check_reads_only_the_status_and_sends_the_key_as_a_bearer() {
        let api = FakeAivisCloud::start(FakeAivisBehaviour {
            accept_key: Some(KEY.to_owned()),
            ..FakeAivisBehaviour::default()
        });
        assert_eq!(
            check_api_key(&api.endpoint(), &SecretString::new(KEY), TIMEOUT),
            Ok(KeyCheck::Valid)
        );
        assert_eq!(
            check_api_key(&api.endpoint(), &SecretString::new("wrong"), TIMEOUT),
            Ok(KeyCheck::Rejected)
        );
        let requests = api.requests();
        assert_eq!(requests[0].route(), ME_ROUTE);
        assert_eq!(
            requests[0].header("authorization").as_deref(),
            Some(format!("Bearer {KEY}").as_str())
        );
        let outage = FakeAivisCloud::start(FakeAivisBehaviour {
            me_status: 502,
            ..FakeAivisBehaviour::default()
        });
        assert_eq!(
            check_api_key(&outage.endpoint(), &SecretString::new(KEY), TIMEOUT),
            Ok(KeyCheck::Unexpected)
        );
        assert_eq!(
            check_api_key("http://127.0.0.1:9", &SecretString::new(KEY), TIMEOUT),
            Ok(KeyCheck::NetworkError)
        );
    }

    #[test]
    fn errors_and_debug_never_carry_the_key() {
        let key = SecretString::new(KEY);
        assert!(!format!("{key:?}").contains("SECRETKEY"));
        for error in [
            ModelsError::BadQuery,
            ModelsError::Origin,
            ModelsError::Timeout,
            ModelsError::Unreachable,
            ModelsError::Status(401),
            ModelsError::TooLarge,
            ModelsError::Malformed,
        ] {
            assert!(!error.to_string().contains("SECRETKEY"));
        }
    }
}
