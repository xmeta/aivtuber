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
}

impl CompositionProfile {
    pub const ALL: [Self; 3] = [Self::Cached, Self::Reflex, Self::Full];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Reflex => "reflex",
            Self::Full => "full",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProfileError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cached" => Ok(Self::Cached),
            "reflex" => Ok(Self::Reflex),
            "full" => Ok(Self::Full),
            other => Err(ProfileError::new(format!(
                "unknown composition profile {other:?}; expected one of cached, reflex, full"
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
        self == Self::Full
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
            return Err(ProfileError::new(
                "profile \"full\" requires generative backend configuration (AIVTUBER_GENERATIVE_ENABLED=true with thinking/TTS endpoints); use the \"reflex\" profile for a documented degraded fallback or complete the configuration",
            ));
        }
        if !self.generative_route() && generative_configured {
            return Err(ProfileError::new(format!(
                "generative backend is configured but profile {:?} cannot reach it; select the \"full\" profile or remove the generative configuration to avoid a misleading composition",
                self.as_str()
            )));
        }
        Ok(())
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
            },
            config_fingerprint,
        }
    }

    /// Render the startup log line. Never includes credentials or endpoints
    /// with embedded keys.
    pub fn log_line(&self) -> String {
        format!(
            "composition profile: profile={} semantic={} jev={} template={} generative={} generative_available={} fallback={} fingerprint={}",
            self.profile,
            self.semantic_retrieval,
            self.jev,
            self.template_route,
            self.generative_route,
            self.generative_available,
            self.fallback_route,
            self.config_fingerprint,
        )
    }
}

/// Stable fingerprint of the active profile composition for replay/telemetry
/// records. Hashes only profile-relevant facts, never secrets.
pub fn config_fingerprint(profile: CompositionProfile, generative_available: bool) -> String {
    let summary = ProfileSummary::for_profile(profile, generative_available, String::new());
    let payload = format!(
        "profile-v1;profile={};semantic={};jev={};template={};generative_available={}",
        summary.profile,
        summary.semantic_retrieval,
        summary.jev,
        summary.template_route,
        summary.generative_available,
    );
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
