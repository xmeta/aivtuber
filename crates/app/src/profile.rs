//! Explicit production composition profiles (issue #55).
//!
//! A profile names the whole routing stack — planner, semantic retrieval,
//! Jev, templates, generative fallback — instead of leaving users to infer
//! the active hierarchy from disconnected environment flags. The selected
//! profile determines what gets composed; environment variables still supply
//! credentials and low-level tuning, but the route graph is explicit.
//!
//! Profiles fail fast on impossible or misleading configurations (e.g. `full`
//! without any generative backend configuration). Degraded operation is
//! acceptable only when explicitly designed and reported at startup; silent
//! accidental downgrades are not (issue #55).

/// High-level composition profiles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CompositionProfile {
    /// Exact/preselected cached playback only (`IntentRoutePlanner`).
    #[default]
    Cached,
    /// Semantic retrieval + Jev + cached/template decisions
    /// (`ReflexRoutePlanner`), no generative fallback.
    Reflex,
    /// Semantic retrieval + Jev + template + LLM/TTS generative fallback
    /// (`ReflexRoutePlanner` + generative runtime).
    Full,
    /// Zero-paid-dependency local profile (issue #220 S1): deterministic
    /// intent fast path + deterministic routing fallback (no Jev, no semantic
    /// retrieval) + a Thinking engine that must be a local/loopback endpoint.
    /// Free text chat/speech reaches Thinking instead of ending `Silent`.
    Local,
    /// The `local` route graph with a **remote Thinking provider** (issue
    /// #364): no Jev, no semantic retrieval, local TTS/avatar. Only the
    /// Thinking role may leave the machine, and only over HTTPS to an
    /// allowlisted host ([`ThinkingEgressAllowlist`]) with a Bearer
    /// credential; every other role stays loopback-only, exactly as `local`.
    Hybrid,
}

impl CompositionProfile {
    pub const ALL: [Self; 5] = [
        Self::Cached,
        Self::Reflex,
        Self::Full,
        Self::Local,
        Self::Hybrid,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Reflex => "reflex",
            Self::Full => "full",
            Self::Local => "local",
            Self::Hybrid => "hybrid",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProfileError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cached" => Ok(Self::Cached),
            "reflex" => Ok(Self::Reflex),
            "full" => Ok(Self::Full),
            "local" => Ok(Self::Local),
            "hybrid" => Ok(Self::Hybrid),
            other => Err(ProfileError::new(format!(
                "unknown composition profile {other:?}; expected one of cached, reflex, full, local, hybrid"
            ))),
        }
    }

    pub fn semantic_retrieval(self) -> bool {
        matches!(self, Self::Reflex | Self::Full)
    }

    pub fn jev(self) -> bool {
        matches!(self, Self::Reflex | Self::Full)
    }

    pub fn template_route(self) -> bool {
        matches!(self, Self::Reflex | Self::Full)
    }

    pub fn generative_route(self) -> bool {
        matches!(self, Self::Full | Self::Local | Self::Hybrid)
    }

    /// Whether the profile composes the local stack (deterministic routing,
    /// loopback TTS/avatar/sidecars, operation panel by default): `local`, and
    /// `hybrid`, which differs only in the Thinking endpoint policy.
    pub fn local_stack(self) -> bool {
        matches!(self, Self::Local | Self::Hybrid)
    }

    /// Whether generated replies are committed phrase by phrase unless
    /// `AIVTUBER_PHRASE_MODE` says otherwise (issue #83 S1).
    ///
    /// Only the `local` profile: its whole path is loopback and the case for
    /// it is measured (docs/local-profile-quickstart.adoc, "Phrase mode"):
    /// whole-reply synthesis is the dominant first-audio latency there and
    /// long replies approach the sidecar deadline. `full` keeps buffered
    /// whole-reply commit, which is the more conservative publication
    /// boundary for remote providers, until it has the same evidence;
    /// `AIVTUBER_PHRASE_MODE=phrase` opts any profile in.
    ///
    /// `hybrid` follows `local`: it composes the same local TTS stack, where
    /// whole-reply synthesis is the dominant first-audio latency.
    pub fn phrased_replies_by_default(self) -> bool {
        self.local_stack()
    }

    /// Whether a phrased reply streams the provider's tokens into phrase
    /// commit unless `AIVTUBER_THINKING_STREAM` says otherwise (issue #83
    /// S3c).
    ///
    /// Only the `local` profile, on the measurement against a real
    /// llama-server (docs/experiments/EXP-0005-streaming-ttfa.adoc): streamed
    /// was not slower at p50, p90 or max on either workload and reached the
    /// first phrase much sooner for multi-phrase replies. Every other profile
    /// stays buffered: its Thinking provider is remote or unmeasured, and a
    /// stream has no whole-reply gate before phrase 0. That includes `hybrid`
    /// (issue #364): its remote provider (OpenRouter) has no streaming
    /// time-to-first-audio measurement yet, so it stays buffered until one
    /// exists (`AIVTUBER_THINKING_STREAM=on` opts in). It still needs phrase
    /// mode, so the default applies only when the reply is phrased.
    pub fn thinking_stream_by_default(self) -> bool {
        self == Self::Local
    }

    /// Whether every provider endpoint, Thinking included, must be loopback
    /// (nothing leaves the machine). Only `local`; see
    /// [`Self::validate_thinking_endpoint`] for `hybrid`.
    pub fn loopback_only(self) -> bool {
        self == Self::Local
    }

    /// Validate the profile against the environment-provided configuration
    /// **before** composition, failing fast on misleading setups.
    ///
    /// `generative_configured` is true when a ThinkingEngine/TTS backend is
    /// fully configured (not merely enabled); `jev_api_key_present` reflects
    /// credential availability for the Jev adapter.
    pub fn validate(
        self,
        generative_configured: bool,
        jev_api_key_present: bool,
    ) -> Result<(), ProfileError> {
        if self.jev() && !jev_api_key_present {
            return Err(ProfileError::new(format!(
                "profile {:?} requires Jev credentials (AIVTUBER_JEV_API_KEY); refusing to start with a silently degraded route graph",
                self.as_str()
            )));
        }
        if self.generative_route() && !generative_configured {
            return Err(ProfileError::new(format!(
                "profile {:?} requires generative backend configuration (AIVTUBER_GENERATIVE_ENABLED=true with thinking/TTS endpoints); use the \"reflex\" profile for a documented degraded fallback or complete the configuration",
                self.as_str()
            )));
        }
        if !self.generative_route() && generative_configured {
            return Err(ProfileError::new(format!(
                "generative backend is configured but profile {:?} cannot reach it; select the \"full\" profile or remove the generative configuration to avoid a misleading composition",
                self.as_str()
            )));
        }
        Ok(())
    }

    /// Fail closed when the local stack is pointed at a non-Thinking provider
    /// endpoint that is not provably this machine. `endpoints` pairs a
    /// human-readable role with its already-verified loopback status, so the
    /// error names the offender without echoing the URL. This covers every
    /// role except Thinking, which `hybrid` may send off-machine under
    /// [`Self::validate_thinking_endpoint`]; it is loopback-only for `local`
    /// and `hybrid` alike. Pass Thinking here only for `local`.
    pub fn validate_provider_endpoints(
        self,
        endpoints: &[(&str, bool)],
    ) -> Result<(), ProfileError> {
        if !self.local_stack() {
            return Ok(());
        }
        for (role, loopback) in endpoints {
            if !loopback {
                return Err(ProfileError::new(format!(
                    "profile {:?} is loopback-only but the {role} endpoint is not a loopback address; point it at 127.0.0.1/[::1]/localhost or select the \"full\" profile",
                    self.as_str()
                )));
            }
        }
        Ok(())
    }

    /// Per-role egress policy for the Thinking endpoint (issue #364).
    ///
    /// * `local`: loopback only, exactly as [`Self::validate_provider_endpoints`].
    /// * `hybrid`: loopback, or a remote host that is HTTPS, exactly matches
    ///   `allowlist` and is reached with a Bearer credential. Anything else
    ///   fails closed.
    /// * other profiles: no restriction (unchanged).
    ///
    /// Returns the remote host the chat/reply text will be sent to, for the
    /// startup notice; `None` when nothing leaves the machine. Errors name the
    /// role and never echo the URL.
    pub fn validate_thinking_endpoint(
        self,
        endpoint: &str,
        loopback: bool,
        bearer_auth: bool,
        allowlist: &ThinkingEgressAllowlist,
    ) -> Result<Option<String>, ProfileError> {
        match self {
            Self::Local => {
                self.validate_provider_endpoints(&[("thinking", loopback)])?;
                Ok(None)
            }
            Self::Hybrid if !loopback => {
                let refuse = |reason: &str| {
                    ProfileError::new(format!(
                        "profile \"hybrid\" refuses the thinking endpoint: {reason}"
                    ))
                };
                let (host, port) =
                    aivtuber_adapters::https_endpoint_target(endpoint).map_err(|reason| {
                        refuse(&format!(
                            "a remote endpoint must be HTTPS to a DNS name without credentials ({reason})"
                        ))
                    })?;
                if !allowlist.permits(&host, port) {
                    return Err(refuse(
                        "its host is not on the egress allowlist; use AIVTUBER_THINKING_PROVIDER=openrouter or set AIVTUBER_THINKING_EGRESS_HOST to the exact host",
                    ));
                }
                if !bearer_auth {
                    return Err(refuse(
                        "a remote endpoint requires Bearer authentication (AIVTUBER_OPENAI_API_KEY)",
                    ));
                }
                Ok(Some(host))
            }
            Self::Hybrid | Self::Cached | Self::Reflex | Self::Full => Ok(None),
        }
    }
}

/// Host of the OpenRouter preset (`AIVTUBER_THINKING_PROVIDER=openrouter`).
pub const OPENROUTER_EGRESS_HOST: &str = "openrouter.ai";

/// Exact `(host, port)` pairs the `hybrid` profile may send Thinking traffic
/// to (issue #364). No wildcards, no suffix matching, no IP literals; the
/// default port is 443.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThinkingEgressAllowlist {
    entries: Vec<(String, u16)>,
}

impl ThinkingEgressAllowlist {
    /// `openrouter_preset` allows [`OPENROUTER_EGRESS_HOST`]; `configured` is
    /// the `AIVTUBER_THINKING_EGRESS_HOST` value (`host` or `host:port`).
    pub fn new(openrouter_preset: bool, configured: Option<&str>) -> Result<Self, ProfileError> {
        let mut entries = Vec::new();
        if openrouter_preset {
            entries.push((OPENROUTER_EGRESS_HOST.to_owned(), 443));
        }
        if let Some(value) = configured {
            let invalid = || {
                ProfileError::new(
                    "AIVTUBER_THINKING_EGRESS_HOST must be one bare DNS name with an optional :port (no scheme, path, userinfo, IP address or wildcard)",
                )
            };
            let value = value.trim();
            if value.is_empty()
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
            {
                return Err(invalid());
            }
            let (host, port) = match value.split_once(':') {
                None => (value, 443),
                Some((host, port)) => {
                    let port: u16 = port.parse().map_err(|_| invalid())?;
                    if port == 0 {
                        return Err(invalid());
                    }
                    (host, port)
                }
            };
            // Round-trip through the endpoint parser so the allowlist and the
            // endpoint check share one notion of "host".
            let (parsed, parsed_port) =
                aivtuber_adapters::https_endpoint_target(&format!("https://{host}:{port}/"))
                    .map_err(|_| invalid())?;
            if parsed != host.to_ascii_lowercase() || parsed_port != port {
                return Err(invalid());
            }
            entries.push((parsed, port));
        }
        Ok(Self { entries })
    }

    pub fn permits(&self, host: &str, port: u16) -> bool {
        self.entries
            .iter()
            .any(|(allowed, allowed_port)| allowed == host && *allowed_port == port)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileError {
    message: String,
}

impl ProfileError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProfileError {}

/// Non-secret startup summary of the active routing capabilities (issue #55
/// acceptance: "startup output states the active routing capabilities without
/// leaking secrets").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSummary {
    pub profile: String,
    pub semantic_retrieval: bool,
    pub jev: bool,
    pub template_route: bool,
    pub generative_route: bool,
    /// Whether the process can generate (profile + runtime availability).
    pub generative_available: bool,
    /// Deterministic fallback route for the profile.
    pub fallback_route: String,
    /// Fingerprint of the active configuration for replay/telemetry
    /// correlation (#55 acceptance: replay records the active profile).
    pub config_fingerprint: String,
    /// Remote host that Thinking traffic (chat and reply text) is sent to;
    /// `None` when nothing leaves the machine (issue #364). Host only: no
    /// scheme, path, key or text.
    pub thinking_egress_host: Option<String>,
}

impl ProfileSummary {
    pub fn for_profile(
        profile: CompositionProfile,
        generative_available: bool,
        config_fingerprint: String,
    ) -> Self {
        Self {
            profile: profile.as_str().to_owned(),
            semantic_retrieval: profile.semantic_retrieval(),
            jev: profile.jev(),
            template_route: profile.template_route(),
            generative_route: profile.generative_route(),
            generative_available: profile.generative_route() && generative_available,
            fallback_route: match profile {
                CompositionProfile::Cached => "silent".to_owned(),
                // Reflex/full degrade deterministically through the Jev
                // policy/cache fallback chain (issue #9/#54 semantics).
                CompositionProfile::Reflex | CompositionProfile::Full => {
                    "cached_template".to_owned()
                }
                // No decision model: deterministic intent playback, else
                // Thinking, else the typed non-verbal reaction (#220 S1).
                CompositionProfile::Local | CompositionProfile::Hybrid => {
                    "deterministic_local".to_owned()
                }
            },
            config_fingerprint,
            thinking_egress_host: None,
        }
    }

    pub fn with_thinking_egress_host(mut self, host: Option<String>) -> Self {
        self.thinking_egress_host = host;
        self
    }

    /// Operator-facing egress notice, mirroring the Aivis Cloud one: what
    /// leaves the machine, and to whom. Host only.
    pub fn egress_notice(&self) -> Option<String> {
        self.thinking_egress_host.as_ref().map(|host| {
            format!(
                "thinking egress: NOTICE viewer chat and reply text are sent to {host} (internet service) for every generated reply; the credential goes only to that host and no redirect is followed"
            )
        })
    }

    /// Render the startup log line. Never includes credentials or endpoints
    /// with embedded keys.
    pub fn log_line(&self) -> String {
        format!(
            "composition profile: profile={} semantic={} jev={} template={} generative={} generative_available={} fallback={} fingerprint={}{}",
            self.profile,
            self.semantic_retrieval,
            self.jev,
            self.template_route,
            self.generative_route,
            self.generative_available,
            self.fallback_route,
            self.config_fingerprint,
            self.thinking_egress_host
                .as_ref()
                .map(|host| format!(" thinking_egress={host}"))
                .unwrap_or_default(),
        )
    }
}

/// Stable fingerprint of the active profile composition for replay/telemetry
/// records. Hashes only profile-relevant facts, never secrets.
pub fn config_fingerprint(profile: CompositionProfile, generative_available: bool) -> String {
    config_fingerprint_with_phrases(profile, generative_available, false)
}

/// [`config_fingerprint`] that also names phrase-level commit of generated
/// replies (issue #83), which changes what is committed and in what order.
/// Buffered composition hashes exactly as before, so existing fingerprints
/// stay valid.
pub fn config_fingerprint_with_phrases(
    profile: CompositionProfile,
    generative_available: bool,
    phrased_replies: bool,
) -> String {
    let summary = ProfileSummary::for_profile(profile, generative_available, String::new());
    let mut payload = format!(
        "profile-v1;profile={};semantic={};jev={};template={};generative_available={}",
        summary.profile,
        summary.semantic_retrieval,
        summary.jev,
        summary.template_route,
        summary.generative_available,
    );
    if phrased_replies {
        payload.push_str(";phrased_replies=true");
    }
    // FNV-1a 64-bit: deterministic, dependency-free, fingerprint-only.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in payload.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("profile-v1-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_profile_names() {
        for profile in CompositionProfile::ALL {
            assert_eq!(
                CompositionProfile::parse(profile.as_str()).expect("parse"),
                profile
            );
        }
        assert_eq!(
            CompositionProfile::parse("FULL").expect("case-insensitive"),
            CompositionProfile::Full
        );
        assert!(CompositionProfile::parse("turbo").is_err());
    }

    #[test]
    fn capability_matrix_matches_issue_definitions() {
        let cached = CompositionProfile::Cached;
        assert!(!cached.semantic_retrieval());
        assert!(!cached.jev());
        assert!(!cached.template_route());
        assert!(!cached.generative_route());

        let reflex = CompositionProfile::Reflex;
        assert!(reflex.semantic_retrieval());
        assert!(reflex.jev());
        assert!(reflex.template_route());
        assert!(!reflex.generative_route());

        let full = CompositionProfile::Full;
        assert!(full.semantic_retrieval());
        assert!(full.jev());
        assert!(full.template_route());
        assert!(full.generative_route());

        let local = CompositionProfile::Local;
        assert!(!local.semantic_retrieval());
        assert!(!local.jev(), "local must not require a paid decision model");
        assert!(!local.template_route());
        assert!(local.generative_route());
        assert!(local.loopback_only());
        assert!(!full.loopback_only());
    }

    #[test]
    fn local_profile_needs_generative_config_but_not_jev_credentials() {
        CompositionProfile::Local
            .validate(true, false)
            .expect("local runs without a Jev credential");
        let error = CompositionProfile::Local
            .validate(false, false)
            .expect_err("local without a Thinking engine would silently do nothing");
        assert!(error.to_string().contains("local"));
    }

    #[test]
    fn loopback_only_profile_refuses_non_loopback_provider_endpoints() {
        CompositionProfile::Local
            .validate_provider_endpoints(&[("thinking", true), ("tts", true)])
            .expect("all loopback");
        let error = CompositionProfile::Local
            .validate_provider_endpoints(&[("thinking", true), ("tts", false)])
            .expect_err("remote TTS breaks the local promise");
        assert!(error.to_string().contains("tts"));
        CompositionProfile::Full
            .validate_provider_endpoints(&[("thinking", false)])
            .expect("only the local profile is loopback-only");
    }

    #[test]
    fn full_without_generative_configuration_fails_fast() {
        CompositionProfile::Full
            .validate(true, true)
            .expect("full with complete generative configuration is valid");

        let error = CompositionProfile::Full
            .validate(false, true)
            .expect_err("generative_configured=false must fail the full profile");
        assert!(error.to_string().contains("full"));
    }

    #[test]
    fn reflex_without_jev_credentials_fails_fast() {
        let error = CompositionProfile::Reflex
            .validate(false, false)
            .expect_err("missing jev key");
        assert!(error.to_string().contains("AIVTUBER_JEV_API_KEY"));
    }

    #[test]
    fn generative_configured_with_cached_profile_is_misleading() {
        let error = CompositionProfile::Cached
            .validate(true, true)
            .expect_err("misleading generative config");
        assert!(error.to_string().contains("misleading"));
    }

    #[test]
    fn valid_combinations_pass() {
        CompositionProfile::Cached
            .validate(false, true)
            .expect("cached");
        CompositionProfile::Reflex
            .validate(false, true)
            .expect("reflex");
        CompositionProfile::Full.validate(true, true).expect("full");
    }

    #[test]
    fn summary_reports_capabilities_without_secrets() {
        let summary = ProfileSummary::for_profile(
            CompositionProfile::Full,
            true,
            config_fingerprint(CompositionProfile::Full, true),
        );
        let line = summary.log_line();
        assert!(line.contains("profile=full"));
        assert!(line.contains("generative_available=true"));
        assert!(!line.contains("key"));
        assert!(!line.contains("token"));
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_profile() {
        let full = config_fingerprint(CompositionProfile::Full, true);
        let reflex = config_fingerprint(CompositionProfile::Reflex, false);
        let full_again = config_fingerprint(CompositionProfile::Full, true);
        assert_eq!(
            full, full_again,
            "same composition must produce the same fingerprint"
        );
        assert_ne!(full, reflex);
        assert!(full.starts_with("profile-v1-"));
    }

    #[test]
    fn only_the_local_stack_is_phrased_by_default() {
        for profile in CompositionProfile::ALL {
            assert_eq!(
                profile.phrased_replies_by_default(),
                matches!(
                    profile,
                    CompositionProfile::Local | CompositionProfile::Hybrid
                )
            );
        }
    }

    #[test]
    fn only_the_local_profile_streams_thinking_by_default() {
        for profile in CompositionProfile::ALL {
            assert_eq!(
                profile.thinking_stream_by_default(),
                profile == CompositionProfile::Local
            );
            // Streaming feeds phrase commit, so it never defaults on where
            // phrase mode is off.
            if profile.thinking_stream_by_default() {
                assert!(profile.phrased_replies_by_default());
            }
        }
    }

    #[test]
    fn hybrid_is_the_local_route_graph_with_buffered_streaming_default() {
        let hybrid = CompositionProfile::Hybrid;
        assert_eq!(
            CompositionProfile::parse(" Hybrid ").expect("parse"),
            hybrid
        );
        assert_eq!(hybrid.as_str(), "hybrid");
        assert!(!hybrid.semantic_retrieval());
        assert!(!hybrid.jev());
        assert!(!hybrid.template_route());
        assert!(hybrid.generative_route());
        assert!(hybrid.local_stack());
        assert!(hybrid.phrased_replies_by_default());
        assert!(
            !hybrid.thinking_stream_by_default(),
            "remote Thinking has no streaming measurement yet"
        );
        assert!(
            !hybrid.loopback_only(),
            "only the Thinking role may leave the machine"
        );
        assert!(!CompositionProfile::Full.local_stack());
        // No Jev credential needed; generative configuration still is.
        hybrid
            .validate(true, false)
            .expect("hybrid runs without Jev");
        assert!(hybrid.validate(false, false).is_err());
        assert_eq!(
            ProfileSummary::for_profile(hybrid, true, String::new()).fallback_route,
            "deterministic_local"
        );
    }

    const OPENROUTER: &str = "https://openrouter.ai/api/v1/chat/completions";

    fn openrouter_allowlist() -> ThinkingEgressAllowlist {
        ThinkingEgressAllowlist::new(true, None).expect("preset")
    }

    #[test]
    fn hybrid_allows_only_the_allowlisted_https_thinking_host_with_bearer_auth() {
        let allow = openrouter_allowlist();
        let host = CompositionProfile::Hybrid
            .validate_thinking_endpoint(OPENROUTER, false, true, &allow)
            .expect("openrouter is allowlisted");
        assert_eq!(host.as_deref(), Some("openrouter.ai"));
        // Host comparison is case-insensitive on the parsed URL host.
        CompositionProfile::Hybrid
            .validate_thinking_endpoint("https://OpenRouter.AI/api/v1/x", false, true, &allow)
            .expect("case-insensitive host");
        // Loopback Thinking is fine and reports no egress.
        assert_eq!(
            CompositionProfile::Hybrid
                .validate_thinking_endpoint("http://127.0.0.1:8080/v1/x", true, false, &allow)
                .expect("loopback"),
            None
        );
    }

    #[test]
    fn hybrid_refuses_other_hosts_plaintext_and_endpoint_tricks() {
        let allow = openrouter_allowlist();
        for endpoint in [
            "https://api.openai.com/v1/chat/completions",
            "https://openrouter.ai.evil.example/v1/x",
            "https://evilopenrouter.ai/v1/x",
            "https://sub.openrouter.ai/v1/x",
            "https://openrouter.ai./v1/x",
            "https://evil.example/openrouter.ai",
            "https://evil.example/?h=openrouter.ai",
            "http://openrouter.ai/api/v1/x",
            "https://openrouter.ai@evil.example/v1/x",
            "https://user:pass@openrouter.ai/v1/x",
            "https://openrouter.ai:8443/v1/x",
            "https://openrouter.ai:80/v1/x",
            "https://104.18.0.1/v1/x",
            "https://[2606:4700::1]/v1/x",
            "ftp://openrouter.ai/x",
            "not a url",
        ] {
            let error = CompositionProfile::Hybrid
                .validate_thinking_endpoint(endpoint, false, true, &allow)
                .expect_err(endpoint);
            let message = error.to_string();
            assert!(message.contains("thinking"), "{message}");
            assert!(
                !message.contains("evil") && !message.contains("pass"),
                "the error must not echo the URL: {message}"
            );
        }
        // No preset and no configured host: nothing remote is allowed.
        assert!(
            CompositionProfile::Hybrid
                .validate_thinking_endpoint(
                    OPENROUTER,
                    false,
                    true,
                    &ThinkingEgressAllowlist::default()
                )
                .is_err()
        );
    }

    #[test]
    fn hybrid_requires_bearer_auth_for_a_remote_thinking_endpoint() {
        let error = CompositionProfile::Hybrid
            .validate_thinking_endpoint(OPENROUTER, false, false, &openrouter_allowlist())
            .expect_err("unauthenticated remote Thinking");
        assert!(error.to_string().contains("Bearer"));
    }

    #[test]
    fn a_configured_egress_host_is_exact_and_strict() {
        let allow = ThinkingEgressAllowlist::new(false, Some(" LLM.Example.org ")).expect("host");
        assert!(allow.permits("llm.example.org", 443));
        assert!(
            !allow.permits("openrouter.ai", 443),
            "no preset, no openrouter"
        );
        assert!(!allow.permits("llm.example.org", 8443));
        assert!(!allow.permits("x.llm.example.org", 443));
        CompositionProfile::Hybrid
            .validate_thinking_endpoint("https://llm.example.org/v1/chat", false, true, &allow)
            .expect("configured host");
        assert!(
            CompositionProfile::Hybrid
                .validate_thinking_endpoint("https://llm.example.org:8443/v1", false, true, &allow)
                .is_err(),
            "a port that is not part of the configured value"
        );
        let with_port =
            ThinkingEgressAllowlist::new(false, Some("llm.example.org:8443")).expect("port");
        CompositionProfile::Hybrid
            .validate_thinking_endpoint("https://llm.example.org:8443/v1", false, true, &with_port)
            .expect("explicit port");
        assert!(
            CompositionProfile::Hybrid
                .validate_thinking_endpoint("https://llm.example.org/v1", false, true, &with_port)
                .is_err()
        );
        // The preset and a configured host are a union.
        let both = ThinkingEgressAllowlist::new(true, Some("llm.example.org")).expect("both");
        assert!(both.permits("openrouter.ai", 443) && both.permits("llm.example.org", 443));
        for bad in [
            "",
            "*.example.org",
            "*",
            "https://llm.example.org",
            "llm.example.org/path",
            "user@llm.example.org",
            "127.0.0.1",
            "127.1",
            "0x7f.1",
            "[::1]",
            "llm.example.org:0",
            "llm.example.org:99999",
            "llm.example.org:443:1",
            "llm example.org",
        ] {
            let error = ThinkingEgressAllowlist::new(false, Some(bad)).expect_err(bad);
            assert!(!error.to_string().contains("example.org"));
        }
    }

    #[test]
    fn hybrid_keeps_every_other_role_loopback_only() {
        CompositionProfile::Hybrid
            .validate_provider_endpoints(&[("tts", true), ("perception model", true)])
            .expect("loopback roles");
        for role in ["tts", "perception model", "scene context OBS"] {
            let error = CompositionProfile::Hybrid
                .validate_provider_endpoints(&[(role, false)])
                .expect_err("non-loopback non-Thinking role");
            assert!(error.to_string().contains(role));
        }
    }

    #[test]
    fn local_still_refuses_every_remote_thinking_endpoint() {
        let allow = openrouter_allowlist();
        let error = CompositionProfile::Local
            .validate_thinking_endpoint(OPENROUTER, false, true, &allow)
            .expect_err("local never sends Thinking off the machine");
        assert!(error.to_string().contains("thinking"));
        assert!(error.to_string().contains("\"local\""));
        assert_eq!(
            CompositionProfile::Local
                .validate_thinking_endpoint("http://127.0.0.1:1/v1", true, false, &allow)
                .expect("loopback"),
            None
        );
        // Unchanged: the other profiles carry no Thinking restriction.
        CompositionProfile::Full
            .validate_thinking_endpoint(
                OPENROUTER,
                false,
                true,
                &ThinkingEgressAllowlist::default(),
            )
            .expect("full is unrestricted");
    }

    #[test]
    fn hybrid_summary_names_the_egress_host_and_no_secret() {
        let summary = ProfileSummary::for_profile(
            CompositionProfile::Hybrid,
            true,
            config_fingerprint(CompositionProfile::Hybrid, true),
        )
        .with_thinking_egress_host(Some("openrouter.ai".to_owned()));
        let line = summary.log_line();
        assert!(line.contains("profile=hybrid"));
        assert!(line.contains("jev=false"));
        assert!(line.contains("thinking_egress=openrouter.ai"));
        let notice = summary.egress_notice().expect("notice");
        assert!(notice.contains("openrouter.ai"));
        assert!(notice.contains("chat and reply text"));
        for text in [&line, &notice] {
            assert!(!text.contains("sk-") && !text.contains("Bearer"));
            assert!(!text.contains("https://"));
        }
        assert_eq!(
            ProfileSummary::for_profile(CompositionProfile::Local, true, String::new())
                .egress_notice(),
            None
        );
    }

    #[test]
    fn phrase_mode_changes_the_fingerprint_and_buffered_does_not() {
        for profile in CompositionProfile::ALL {
            assert_eq!(
                config_fingerprint_with_phrases(profile, true, false),
                config_fingerprint(profile, true),
                "buffered composition keeps its existing fingerprint"
            );
            assert_ne!(
                config_fingerprint_with_phrases(profile, true, true),
                config_fingerprint(profile, true)
            );
        }
    }

    #[test]
    fn reflex_full_fallback_is_cached_template() {
        assert_eq!(
            ProfileSummary::for_profile(CompositionProfile::Cached, false, String::new())
                .fallback_route,
            "silent"
        );
        assert_eq!(
            ProfileSummary::for_profile(CompositionProfile::Reflex, false, String::new())
                .fallback_route,
            "cached_template"
        );
    }
}
