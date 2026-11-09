//! Adapter-private translation from pack/asset preset strings to the
//! renderer's `vrm1-default@1` vocabulary (ADR-0003, ADR-0006 S-6).
//!
//! Packs speak `<family>_<level>` (`surprise_medium`, `nod_small`); the renderer
//! knows a handful of plain presets. This table lives on the host side so
//! nothing renderer-specific enters the domain and the renderer page stays a
//! dumb profile. A preset with no honest equivalent is *not sent*: the caller
//! counts it (see `LocalAvatarHealth::unmapped_presets`) and carries on.
//!
//! Level words map onto the four ADR-0006 `intent-vocab/1` ordinal levels, and
//! those onto the compiler-owned numeric table (0.25, 0.5, 0.75, 1.0). The
//! scale multiplies into the command's own intensity/amplitude, so an asset
//! that already says `intensity: 0.72` is attenuated, never amplified.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PresetKind {
    Expression,
    Gesture,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PresetMapping {
    /// Send `target` with the command value multiplied by `scale` (then
    /// clamped to the protocol range by the caller).
    Mapped { target: &'static str, scale: f64 },
    /// Known to the table and deliberately not rendered; `reason` is for
    /// humans and the coverage test.
    Unsupported { reason: &'static str },
    /// Not known to the table at all.
    Unknown,
}

/// ADR-0006 ordinal level -> numeric scale (`intent-vocab/1`).
const SUBTLE: f64 = 0.25;
const MILD: f64 = 0.5;
const MODERATE: f64 = 0.75;
const STRONG: f64 = 1.0;

/// Level words used by checked-in packs (`small`, `soft`, `medium`, `bright`)
/// plus the ADR words and `large`.
const LEVEL_WORDS: &[(&str, f64)] = &[
    ("subtle", SUBTLE),
    ("small", MILD),
    ("soft", MILD),
    ("mild", MILD),
    ("medium", MODERATE),
    ("moderate", MODERATE),
    ("large", STRONG),
    ("bright", STRONG),
    ("strong", STRONG),
];

/// Whole-name entries: irregular names that do not follow `<family>_<level>`,
/// and the renderer's own plain presets (scale 1.0).
const EXPRESSION_EXACT: &[(&str, &str, f64)] = &[
    ("neutral", "neutral", 1.0),
    ("joy", "joy", 1.0),
    ("anger", "anger", 1.0),
    ("sadness", "sadness", 1.0),
    ("calm", "calm", 1.0),
    ("surprise", "surprise", 1.0),
    // The "speaking" half is lip-sync's job; the face baseline is neutral.
    ("speaking.neutral", "neutral", 1.0),
    // Widened eyes are the surprise display; kept mild so it does not read
    // as a full startle.
    ("eyes_wide", "surprise", MILD),
];

/// `<family>` -> renderer preset, for `<family>_<level>` names.
const EXPRESSION_FAMILIES: &[(&str, &str)] = &[
    ("surprise", "surprise"),
    ("smile", "joy"),
    ("joy", "joy"),
    ("anger", "anger"),
    ("sadness", "sadness"),
    ("calm", "calm"),
];

const EXPRESSION_UNSUPPORTED: &[(&str, &str)] = &[(
    "thinking",
    "the renderer has no thinking face; thinking is conveyed by gaze, not an expression (ADR-0006)",
)];

const GESTURE_EXACT: &[(&str, &str, f64)] = &[
    ("nod", "nod", 1.0),
    ("shake", "shake", 1.0),
    // One nod at full amplitude: the renderer cannot repeat, and a double nod
    // is never *weaker* than a single one.
    ("nod_double", "nod", 1.0),
];

const GESTURE_FAMILIES: &[(&str, &str)] =
    &[("nod", "nod"), ("head_shake", "shake"), ("shake", "shake")];

const GESTURE_UNSUPPORTED: &[(&str, &str)] = &[
    (
        "recoil",
        "the renderer has no body recoil gesture (only nod and shake)",
    ),
    (
        "lean_back",
        "the renderer has no torso lean gesture (only nod and shake)",
    ),
    (
        "tilt_head",
        "the renderer has no head tilt gesture (only nod and shake)",
    ),
    (
        "wave",
        "the renderer has no arm gestures (only nod and shake)",
    ),
    (
        "shrug",
        "the renderer has no shoulder gestures (only nod and shake)",
    ),
];

/// Splits `<family>_<level>` at the last underscore when the suffix is a
/// known level word.
fn split_level(name: &str) -> Option<(&str, f64)> {
    let (family, word) = name.rsplit_once('_')?;
    let scale = LEVEL_WORDS.iter().find(|(w, _)| *w == word)?.1;
    Some((family, scale))
}

fn lookup(table: &'static [(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// Resolves a pack preset name. Pure and total.
pub fn map_preset(kind: PresetKind, name: &str) -> PresetMapping {
    let (exact, families, unsupported) = match kind {
        PresetKind::Expression => (
            EXPRESSION_EXACT,
            EXPRESSION_FAMILIES,
            EXPRESSION_UNSUPPORTED,
        ),
        PresetKind::Gesture => (GESTURE_EXACT, GESTURE_FAMILIES, GESTURE_UNSUPPORTED),
    };
    if let Some((_, target, scale)) = exact.iter().find(|(k, _, _)| *k == name) {
        return PresetMapping::Mapped {
            target,
            scale: *scale,
        };
    }
    // A bare family (`recoil`) or `<family>_<level>`.
    let (family, scale) = split_level(name).unwrap_or((name, 1.0));
    if let Some(target) = lookup(families, family) {
        return PresetMapping::Mapped { target, scale };
    }
    // Unsupported families match with any suffix (`thinking_side`): nothing
    // is sent either way, so the suffix does not need to be understood.
    if let Some((_, reason)) = unsupported
        .iter()
        .find(|(f, _)| name == *f || name.strip_prefix(*f).is_some_and(|r| r.starts_with('_')))
    {
        return PresetMapping::Unsupported { reason };
    }
    PresetMapping::Unknown
}

/// Longest preset name kept as a health counter key.
const MAX_TRACKED_NAME_CHARS: usize = 64;

/// Bounded, printable counter key for a preset name that may be hostile or
/// merely long: truncated, non-printable/non-ASCII characters replaced.
pub(crate) fn tracked_name(name: &str) -> String {
    name.chars()
        .take(MAX_TRACKED_NAME_CHARS)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use PresetKind::{Expression, Gesture};

    fn mapped(kind: PresetKind, name: &str) -> (&'static str, f64) {
        match map_preset(kind, name) {
            PresetMapping::Mapped { target, scale } => (target, scale),
            other => panic!("{name}: {other:?}"),
        }
    }

    #[test]
    fn pack_presets_map_through_family_and_level() {
        assert_eq!(
            mapped(Expression, "surprise_medium"),
            ("surprise", MODERATE)
        );
        assert_eq!(mapped(Expression, "surprise_bright"), ("surprise", STRONG));
        assert_eq!(mapped(Expression, "smile_soft"), ("joy", MILD));
        // The speechless `reaction.neutral` fallback (issue #288) must only
        // use presets this renderer draws, or the "visible" reaction is not.
        assert_eq!(mapped(Expression, "calm_soft"), ("calm", MILD));
        assert_eq!(mapped(Expression, "smile_bright"), ("joy", STRONG));
        assert_eq!(mapped(Expression, "joy"), ("joy", 1.0));
        assert_eq!(mapped(Expression, "speaking.neutral"), ("neutral", 1.0));
        assert_eq!(mapped(Expression, "eyes_wide"), ("surprise", MILD));
        assert_eq!(mapped(Gesture, "nod_small"), ("nod", MILD));
        assert_eq!(mapped(Gesture, "nod_double"), ("nod", 1.0));
        assert_eq!(mapped(Gesture, "head_shake_small"), ("shake", MILD));
        assert_eq!(mapped(Gesture, "nod"), ("nod", 1.0));
    }

    #[test]
    fn families_without_a_renderer_equivalent_are_unsupported_not_invented() {
        for name in ["thinking_soft", "thinking_side"] {
            assert!(matches!(
                map_preset(Expression, name),
                PresetMapping::Unsupported { .. }
            ));
        }
        for name in [
            "recoil_small",
            "lean_back_small",
            "tilt_head_small",
            "wave_small",
            "shrug_small",
        ] {
            assert!(matches!(
                map_preset(Gesture, name),
                PresetMapping::Unsupported { .. }
            ));
        }
    }

    #[test]
    fn kinds_do_not_leak_into_each_other() {
        assert_eq!(map_preset(Expression, "nod_small"), PresetMapping::Unknown);
        assert_eq!(map_preset(Gesture, "joy"), PresetMapping::Unknown);
        assert_eq!(
            map_preset(Expression, "VRMExpression.happy!"),
            PresetMapping::Unknown
        );
        assert_eq!(
            map_preset(Expression, "surprise_wobbly"),
            PresetMapping::Unknown
        );
        assert_eq!(map_preset(Expression, ""), PresetMapping::Unknown);
    }

    #[test]
    fn tracked_names_are_bounded_and_printable() {
        let long = "x".repeat(500);
        assert_eq!(tracked_name(&long).len(), MAX_TRACKED_NAME_CHARS);
        assert_eq!(tracked_name("a b\u{1b}é"), "a?b??");
    }
}
