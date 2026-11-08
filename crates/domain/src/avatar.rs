use serde::{Deserialize, Serialize};

/// Canonical, provider-neutral viseme vocabulary (ADR-0003 Decision 4).
///
/// The five mouth shapes use the VRM 1.0 published preset names; `Neutral`
/// means every mouth shape at zero. Adapters map this closed set to their own
/// provider names (for example VTube Studio parameters) privately. Normative
/// wire form: `schemas/viseme-track.schema.json` and
/// `schemas/avatar-playback-command.schema.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Viseme {
    Aa,
    Ih,
    Ou,
    Ee,
    Oh,
    Neutral,
}

impl Viseme {
    pub const ALL: [Viseme; 6] = [
        Viseme::Aa,
        Viseme::Ih,
        Viseme::Ou,
        Viseme::Ee,
        Viseme::Oh,
        Viseme::Neutral,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Viseme::Aa => "aa",
            Viseme::Ih => "ih",
            Viseme::Ou => "ou",
            Viseme::Ee => "ee",
            Viseme::Oh => "oh",
            Viseme::Neutral => "neutral",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_match_as_str_and_round_trip() {
        for viseme in Viseme::ALL {
            let json = serde_json::to_string(&viseme).expect("serialize");
            assert_eq!(json, format!("\"{}\"", viseme.as_str()));
            assert_eq!(
                serde_json::from_str::<Viseme>(&json).expect("parse"),
                viseme
            );
        }
    }

    #[test]
    fn rejects_non_canonical_names() {
        for name in ["A", "AA", "sil", "mm", "Aa", "", "neutral ", "happy"] {
            assert!(
                serde_json::from_str::<Viseme>(&format!("\"{name}\"")).is_err(),
                "{name:?} must be rejected"
            );
        }
    }
}
