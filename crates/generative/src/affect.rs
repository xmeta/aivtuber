//! Bounded, deterministic text-to-affect classifier (issue #84 S1).
//!
//! Advisory data only: the classifier *recommends* one coarse affect class for
//! a phrase's gated display text; the compiler decides whether and how to use
//! it (pack availability, intensity cap, anti-repetition). Nothing here
//! reaches control, security or authority paths, and no model is involved.
//!
//! Invariants: the lexicon is compiled in and fixed in size; the input is cut
//! to [`AFFECT_MAX_INPUT_CHARS`]; every pattern contributes at most
//! [`MAX_HITS_PER_PATTERN`] hits; no clock, RNG or I/O, so the same text always
//! gives the same reading (replay-identical).

use aivtuber_asset_store::ExpressionTrack;

/// Characters of a phrase the classifier reads. Phrases are short by
/// construction; the cut only bounds adversarial input.
pub const AFFECT_MAX_INPUT_CHARS: usize = 400;
/// Hard ceiling on the intensity the classifier can ask for. The expression
/// is a face accent on a speaking avatar, not a full emotional display.
pub const AFFECT_INTENSITY_CAP: f64 = 0.6;
const MILD_INTENSITY: f64 = 0.35;
const MAX_HITS_PER_PATTERN: usize = 2;
const MAX_AVAILABLE_PRESETS: usize = 64;
/// Minimum evidence for a non-neutral reading; one weak word is not enough.
const MIN_SCORE: u32 = 2;
const STRONG_SCORE: u32 = 4;
/// Consecutive phrases that keep a held (non-neutral) face before it relaxes.
const RELAX_AFTER_HELD: u8 = 2;
/// Characters after a Japanese keyword scanned for a negator.
const JA_NEGATION_WINDOW: usize = 6;
/// Tokens before an English keyword scanned for a negator.
const EN_NEGATION_TOKENS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AffectClass {
    Neutral,
    Joy,
    Surprise,
    Calm,
    Sadness,
    Anger,
}

impl AffectClass {
    pub const ALL: [AffectClass; 6] = [
        Self::Neutral,
        Self::Joy,
        Self::Surprise,
        Self::Calm,
        Self::Sadness,
        Self::Anger,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Joy => "joy",
            Self::Surprise => "surprise",
            Self::Calm => "calm",
            Self::Sadness => "sadness",
            Self::Anger => "anger",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AffectStrength {
    Mild,
    Strong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AffectReading {
    pub class: AffectClass,
    pub strength: AffectStrength,
}

const NEUTRAL_READING: AffectReading = AffectReading {
    class: AffectClass::Neutral,
    strength: AffectStrength::Mild,
};

type Lexicon = &'static [(&'static str, u32)];

const JOY: Lexicon = &[
    ("嬉し", 2),
    ("うれし", 2),
    ("楽し", 2),
    ("たのし", 2),
    ("やった", 2),
    ("最高", 2),
    ("ありがとう", 1),
    ("大好き", 2),
    ("好き", 1),
    ("わくわく", 2),
    ("ワクワク", 2),
    ("おめでとう", 2),
    ("素敵", 1),
    ("すてき", 1),
    ("良かった", 1),
    ("よかった", 1),
    ("幸せ", 2),
    ("しあわせ", 2),
    ("ハッピー", 2),
    ("わーい", 2),
    ("いえーい", 2),
    ("感謝", 1),
    ("えへへ", 1),
    ("うふふ", 1),
    ("あはは", 1),
    ("happy", 2),
    ("glad", 1),
    ("love", 1),
    ("loved", 1),
    ("great", 1),
    ("awesome", 2),
    ("wonderful", 2),
    ("fun", 1),
    ("yay", 2),
    ("thanks", 1),
    ("thank", 1),
    ("congrats", 2),
    ("congratulations", 2),
    ("excited", 2),
    ("delighted", 2),
    ("enjoy", 1),
    ("enjoyed", 1),
    ("fantastic", 2),
    ("nice", 1),
    ("haha", 1),
    ("lol", 1),
];

const SURPRISE: Lexicon = &[
    ("えっ！", 2),
    ("えっ？", 2),
    ("ええっ", 2),
    ("まさか", 2),
    ("びっくり", 2),
    ("ビックリ", 2),
    ("驚", 2),
    ("すごい", 1),
    ("凄い", 1),
    ("うそ", 2),
    ("嘘でしょ", 2),
    ("マジで", 1),
    ("なんと", 2),
    ("信じられない", 2),
    ("意外", 2),
    ("突然", 1),
    ("wow", 2),
    ("whoa", 2),
    ("surprised", 2),
    ("surprising", 2),
    ("unexpected", 2),
    ("unbelievable", 2),
    ("no way", 2),
    ("oh my", 2),
    ("omg", 2),
    ("suddenly", 1),
    ("shocked", 2),
    ("can't believe", 2),
    ("incredible", 1),
    ("amazing", 1),
];

const CALM: Lexicon = &[
    ("落ち着", 2),
    ("安心", 2),
    ("ゆっくり", 2),
    ("のんびり", 2),
    ("穏やか", 2),
    ("静か", 1),
    ("リラックス", 2),
    ("大丈夫", 1),
    ("ほっと", 2),
    ("まったり", 2),
    ("calm", 2),
    ("relax", 2),
    ("relaxed", 2),
    ("relaxing", 2),
    ("peaceful", 2),
    ("gentle", 1),
    ("quiet", 1),
    ("take it easy", 2),
    ("no worries", 2),
    ("slowly", 1),
    ("breathe", 1),
    ("cozy", 2),
];

const SADNESS: Lexicon = &[
    ("悲し", 2),
    ("かなし", 2),
    ("寂し", 2),
    ("さびし", 2),
    ("さみし", 2),
    ("残念", 2),
    ("つらい", 2),
    ("辛い", 1),
    ("泣きそう", 2),
    ("泣", 1),
    ("しょんぼり", 2),
    ("落ち込", 2),
    ("申し訳", 1),
    ("ごめん", 1),
    ("すみません", 1),
    ("切ない", 2),
    ("涙", 1),
    ("がっかり", 2),
    ("sad", 2),
    ("sorry", 1),
    ("unfortunately", 1),
    ("miss you", 2),
    ("lonely", 2),
    ("disappointed", 2),
    ("disappointing", 2),
    ("cry", 1),
    ("crying", 2),
    ("tears", 1),
    ("heartbroken", 2),
    ("depressed", 2),
    ("unhappy", 2),
    ("sadly", 2),
];

const ANGER: Lexicon = &[
    ("怒", 2),
    ("むかつ", 2),
    ("ムカつ", 2),
    ("腹が立", 2),
    ("許せない", 2),
    ("ふざけないで", 2),
    ("ふざけるな", 2),
    ("いい加減に", 2),
    ("うざ", 2),
    ("最悪", 1),
    ("イライラ", 2),
    ("いらいら", 2),
    ("頭にくる", 2),
    ("ひどい", 1),
    ("やめて", 1),
    ("勘弁", 1),
    ("うるさい", 2),
    ("angry", 2),
    ("furious", 2),
    ("annoying", 2),
    ("annoyed", 2),
    ("hate", 2),
    ("mad", 1),
    ("stop it", 1),
    ("unacceptable", 2),
    ("outrageous", 2),
    ("ridiculous", 1),
    ("irritating", 2),
    ("pissed", 2),
    ("damn", 1),
    ("how dare", 2),
    ("fed up", 2),
];

/// Evaluation order doubles as the tie-break (first maximum wins): the
/// negative classes outrank the positive ones, since a wrong cheerful face on
/// a bad moment reads worse than the opposite.
const CLASS_LEXICONS: [(AffectClass, Lexicon); 5] = [
    (AffectClass::Anger, ANGER),
    (AffectClass::Sadness, SADNESS),
    (AffectClass::Surprise, SURPRISE),
    (AffectClass::Joy, JOY),
    (AffectClass::Calm, CALM),
];

/// Markers that turn praise into irony. They suppress the positive classes;
/// the result is neutral rather than a guess at the opposite emotion.
const SARCASM: &[&str] = &[
    "oh great",
    "oh, great",
    "just great",
    "yeah right",
    "yeah, right",
    "sure, because",
    "as if",
    "thanks a lot",
    "oh wonderful",
    "oh, wonderful",
    "how nice",
    "great, another",
    "はいはい",
    "どうせ",
    "さすがですね",
    "ご立派",
    "お得意の",
];

const JA_NEGATORS: &[&str] = &["ない", "なかった", "ません", "ではなく", "じゃなく"];
const EN_NEGATORS: &[&str] = &[
    "not", "no", "never", "cannot", "hardly", "barely", "without", "dont", "isnt", "wasnt",
    "arent", "wont", "didnt", "doesnt",
];

fn is_en_pattern(pattern: &str) -> bool {
    pattern.is_ascii()
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '\''
}

fn en_boundary_ok(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
}

fn en_negated(text: &str, start: usize) -> bool {
    for token in text[..start]
        .split_whitespace()
        .rev()
        .take(EN_NEGATION_TOKENS)
    {
        let bare: String = token
            .chars()
            .filter(|c| c.is_ascii_alphabetic() || *c == '\'')
            .collect();
        let squashed = bare.replace('\'', "");
        if EN_NEGATORS.contains(&squashed.as_str()) || bare.ends_with("n't") {
            return true;
        }
        if token.ends_with(['.', ',', ';', '!', '?']) {
            break;
        }
    }
    false
}

fn ja_negated(text: &str, end: usize) -> bool {
    let window: String = text[end..].chars().take(JA_NEGATION_WINDOW).collect();
    // A clause boundary ends the scope of the negator.
    let scope = window.split(['。', '、', '！', '？', '!', '?']).next();
    scope.is_some_and(|scope| JA_NEGATORS.iter().any(|negator| scope.contains(negator)))
}

fn score_class(text: &str, lexicon: Lexicon, positive_suppressed: bool) -> u32 {
    if positive_suppressed {
        return 0;
    }
    let mut score = 0;
    for (pattern, weight) in lexicon {
        let english = is_en_pattern(pattern);
        let mut hits = 0;
        for (start, matched) in text.match_indices(pattern) {
            let end = start + matched.len();
            if english && !en_boundary_ok(text, start, end) {
                continue;
            }
            let negated = if english {
                en_negated(text, start)
            } else {
                ja_negated(text, end)
            };
            if negated {
                continue;
            }
            score += weight;
            hits += 1;
            if hits == MAX_HITS_PER_PATTERN {
                break;
            }
        }
    }
    score
}

/// Classify one phrase of gated display text. Pure and bounded.
pub fn classify_affect(text: &str) -> AffectReading {
    let lowered: String = text
        .chars()
        .take(AFFECT_MAX_INPUT_CHARS)
        .flat_map(char::to_lowercase)
        .collect();
    let sarcastic = SARCASM.iter().any(|marker| lowered.contains(marker));
    let exclaimed = lowered.contains(['!', '！']);
    let surprise_punct = ["!?", "?!", "！？", "？！"]
        .iter()
        .any(|mark| lowered.contains(mark));

    let mut best = (AffectClass::Neutral, 0_u32);
    for (class, lexicon) in CLASS_LEXICONS {
        let suppressed = sarcastic && matches!(class, AffectClass::Joy | AffectClass::Calm);
        let mut score = score_class(&lowered, lexicon, suppressed);
        if score > 0 {
            let boosted = match class {
                AffectClass::Joy | AffectClass::Anger | AffectClass::Surprise => exclaimed,
                _ => false,
            };
            score += u32::from(boosted);
            if class == AffectClass::Surprise && surprise_punct {
                score += 1;
            }
        }
        if score > best.1 {
            best = (class, score);
        }
    }
    if best.1 < MIN_SCORE {
        return NEUTRAL_READING;
    }
    AffectReading {
        class: best.0,
        strength: if best.1 >= STRONG_SCORE {
            AffectStrength::Strong
        } else {
            AffectStrength::Mild
        },
    }
}

/// Preset candidates per class, in preference order for each strength.
fn candidates(class: AffectClass, strength: AffectStrength) -> &'static [&'static str] {
    use AffectStrength::{Mild, Strong};
    match (class, strength) {
        (AffectClass::Joy, Strong) => &["smile_bright", "smile_soft"],
        (AffectClass::Joy, Mild) => &["smile_soft", "smile_bright"],
        (AffectClass::Surprise, Strong) => &["surprise_bright", "surprise_medium"],
        (AffectClass::Surprise, Mild) => &["surprise_medium", "surprise_bright"],
        (AffectClass::Calm, _) => &["calm_soft"],
        (AffectClass::Sadness, _) => &["sadness_soft", "sadness_medium"],
        (AffectClass::Anger, _) => &["anger_soft", "anger_medium"],
        (AffectClass::Neutral, _) => &[],
    }
}

/// Which expression presets the loaded pack actually ships. A class whose
/// presets are absent degrades to the neutral baseline instead of naming a
/// preset the pack cannot play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectConfig {
    available: Vec<String>,
}

impl AffectConfig {
    pub fn new(available: impl IntoIterator<Item = String>) -> Self {
        let mut available: Vec<String> = available.into_iter().collect();
        available.sort();
        available.dedup();
        available.truncate(MAX_AVAILABLE_PRESETS);
        Self { available }
    }

    /// The expressions the checked-in starter reaction pack ships.
    pub fn starter_pack() -> Self {
        Self::new(
            [
                "calm_soft",
                "smile_soft",
                "smile_bright",
                "surprise_medium",
                "surprise_bright",
            ]
            .map(str::to_owned),
        )
    }

    pub fn available(&self) -> &[String] {
        &self.available
    }

    fn pick(&self, reading: AffectReading) -> Option<ExpressionTrack> {
        let preset = candidates(reading.class, reading.strength)
            .iter()
            .find(|name| self.available.iter().any(|have| have == *name))?;
        let intensity = match reading.strength {
            AffectStrength::Mild => MILD_INTENSITY,
            AffectStrength::Strong => AFFECT_INTENSITY_CAP,
        }
        .min(AFFECT_INTENSITY_CAP);
        Some(ExpressionTrack {
            preset: (*preset).to_owned(),
            intensity,
        })
    }
}

/// Explicit, bounded per-reply carry for anti-repetition. One value per reply;
/// phrase N reads only phrases `0..N` (no look-ahead).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AffectState {
    last: Option<AffectClass>,
    held: u8,
}

impl AffectState {
    /// Decide the expression start for one phrase, or `None` to keep the face
    /// already showing. `neutral` is the compiler's baseline expression.
    ///
    /// * Phrase 0 always starts an expression.
    /// * A later phrase starts one only when its class differs from the one
    ///   showing; the same class again, or a neutral reading, holds the face.
    /// * A held non-neutral face relaxes to the baseline after
    ///   [`RELAX_AFTER_HELD`] consecutive holds, so a mood never outlives the
    ///   text that earned it.
    pub fn plan(
        &mut self,
        config: &AffectConfig,
        seq: u32,
        text: &str,
        neutral: &ExpressionTrack,
    ) -> Option<ExpressionTrack> {
        if seq == 0 {
            *self = Self::default();
        }
        let reading = classify_affect(text);
        let picked = config.pick(reading);
        // A class the pack cannot play is neutral, for the state machine too.
        let class = if picked.is_some() {
            reading.class
        } else {
            AffectClass::Neutral
        };

        let Some(last) = self.last else {
            self.last = Some(class);
            return Some(picked.unwrap_or_else(|| neutral.clone()));
        };
        if class != AffectClass::Neutral && class != last {
            self.last = Some(class);
            self.held = 0;
            return picked;
        }
        // Same class again, or a neutral reading: hold the face.
        self.held = self.held.saturating_add(1);
        if last != AffectClass::Neutral && self.held >= RELAX_AFTER_HELD {
            self.last = Some(AffectClass::Neutral);
            self.held = 0;
            return Some(neutral.clone());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn neutral() -> ExpressionTrack {
        ExpressionTrack {
            preset: "speaking.neutral".to_owned(),
            intensity: 0.4,
        }
    }

    fn class(text: &str) -> AffectClass {
        classify_affect(text).class
    }

    #[test]
    fn basic_ja_and_en_readings() {
        assert_eq!(class("今日はとても嬉しいです！"), AffectClass::Joy);
        assert_eq!(class("本当に残念で、悲しいです。"), AffectClass::Sadness);
        assert_eq!(class("This is awesome, I love it!"), AffectClass::Joy);
        assert_eq!(class("ふざけないで、もう許せない"), AffectClass::Anger);
        assert_eq!(class("今日は在庫を確認します。"), AffectClass::Neutral);
    }

    #[test]
    fn negation_and_sarcasm_do_not_fire_the_positive_class() {
        assert_eq!(class("ぜんぜん楽しくない"), AffectClass::Neutral);
        assert_eq!(class("I am not happy about this."), AffectClass::Neutral);
        assert_eq!(class("Oh great, another bug."), AffectClass::Neutral);
        assert_eq!(class("もうこれは楽しいです、ありがとう"), AffectClass::Joy);
    }

    #[test]
    fn english_matches_whole_words_only() {
        assert_eq!(class("The madness of the funnel"), AffectClass::Neutral);
    }

    #[test]
    fn input_is_bounded_and_deterministic() {
        let long = "嬉しい".repeat(100_000);
        let first = classify_affect(&long);
        assert_eq!(first, classify_affect(&long));
        // Hits per pattern are capped, so spam cannot exceed the strong band
        // by volume alone and the scan stays within the input cut.
        assert_eq!(first.class, AffectClass::Joy);
        // Text past the cut is never read.
        let late = format!("{}怒怒怒怒", "あ".repeat(AFFECT_MAX_INPUT_CHARS));
        assert_eq!(class(&late), AffectClass::Neutral);
    }

    #[test]
    fn intensity_never_exceeds_the_cap() {
        let config = AffectConfig::starter_pack();
        for strength in [AffectStrength::Mild, AffectStrength::Strong] {
            let pick = config
                .pick(AffectReading {
                    class: AffectClass::Joy,
                    strength,
                })
                .expect("joy is in the starter pack");
            assert!(pick.intensity <= AFFECT_INTENSITY_CAP);
        }
    }

    #[test]
    fn a_class_the_pack_lacks_falls_back_to_neutral() {
        let mut state = AffectState::default();
        let config = AffectConfig::starter_pack();
        // The starter pack ships no anger expression.
        let first = state.plan(&config, 0, "ふざけないで、許せない！", &neutral());
        assert_eq!(first, Some(neutral()));
        let empty = AffectConfig::new(Vec::new());
        let mut state = AffectState::default();
        let joyful = state.plan(&empty, 0, "とても嬉しい！最高！", &neutral());
        assert_eq!(joyful, Some(neutral()));
    }

    #[test]
    fn anti_repetition_holds_the_face_and_then_relaxes() {
        let config = AffectConfig::starter_pack();
        let mut state = AffectState::default();
        let joy = "とても嬉しい！最高！";
        let first = state.plan(&config, 0, joy, &neutral()).expect("phrase 0");
        assert!(first.preset.starts_with("smile"));
        // Same class again: no new expression start.
        assert_eq!(state.plan(&config, 1, joy, &neutral()), None);
        // Second consecutive hold relaxes to the baseline.
        assert_eq!(state.plan(&config, 2, joy, &neutral()), Some(neutral()));
        // A different class replaces the face immediately.
        let surprise = state
            .plan(&config, 3, "えっ！？まさか、びっくりしました", &neutral())
            .expect("class change");
        assert!(surprise.preset.starts_with("surprise"));
    }

    #[test]
    fn neutral_text_after_a_mood_holds_then_relaxes() {
        let config = AffectConfig::starter_pack();
        let mut state = AffectState::default();
        state.plan(&config, 0, "とても嬉しい！最高！", &neutral());
        assert_eq!(state.plan(&config, 1, "次の話題です。", &neutral()), None);
        assert_eq!(
            state.plan(&config, 2, "次の話題です。", &neutral()),
            Some(neutral())
        );
        // A neutral baseline that is already showing is never re-emitted.
        assert_eq!(state.plan(&config, 3, "次の話題です。", &neutral()), None);
    }

    #[test]
    fn phrase_zero_resets_state_and_is_deterministic() {
        let config = AffectConfig::starter_pack();
        let run = || {
            let mut state = AffectState::default();
            ["嬉しい！最高！", "嬉しい！最高！", "今日は晴れです。"]
                .iter()
                .enumerate()
                .map(|(i, text)| state.plan(&config, i as u32, text, &neutral()))
                .collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }
}
