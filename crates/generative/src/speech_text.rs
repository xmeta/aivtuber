//! Speech text projection before TTS (ADR-0004 D-5, issue #220 S2b).
//!
//! Generated text is shown and spoken from the same gated string today, so a
//! URL or an emoji in a reply is *read out* by the synthesizer: measured on
//! every engine in EXP-0003 (a URL became letter noise and took 6 s of audio;
//! emoji were silently dropped or misread). The projection derives the string
//! sent to TTS from the gated display text by *deletion only*:
//!
//! * URLs (`http://`, `https://`, `ftp://`, `www.`) are dropped;
//! * emoji and the joiners and selectors that build them are dropped;
//! * parenthesised kaomoji such as `(´▽`)` or `(^_^)` are dropped;
//! * whitespace left behind is collapsed and trimmed.
//!
//! The display text is never changed, nothing is ever inserted, and the
//! function is pure and deterministic (ADR-0004 derivability: every character
//! of the output is a character of the input) for *this stage*. Everything else
//! passes through unchanged here, and symbols such as `&`, `%`, `#`, `@` still
//! do. Latin brand words (EXP-0003: YouTube, Twitter, VTuber are misread) are
//! handled by the next stage, the reading lexicon in `speech_lexicon`, which
//! runs on this stage's output; that stage inserts kana readings from an
//! operator-owned lexicon, so the "no insertion" statement above holds for the
//! deletion rules only.
//!
//! Limits, stated so they are not mistaken for coverage: kaomoji that contain
//! letters or digits (`(T_T)`, `m(_ _)m`) and kaomoji without parentheses are
//! not recognised; bare domains without a scheme or `www.` are not recognised
//! as URLs.

/// What the projection did, for a metric; the counts never include the text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpeechProjectionReport {
    pub dropped_urls: usize,
    pub dropped_emoji: usize,
    pub dropped_kaomoji: usize,
    /// Lexicon readings spliced in (ADR-0004 D3); zero from the deletion rules alone.
    pub lexicon_hits: usize,
    /// Latin words of two or more letters left for the TTS to read
    /// (`projection_passthrough_latin`, ADR-0004 D-5): the lexicon's to-do list.
    pub passthrough_latin: usize,
}

impl SpeechProjectionReport {
    pub fn changed(&self) -> bool {
        self.dropped_urls + self.dropped_emoji + self.dropped_kaomoji + self.lexicon_hits > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechProjection {
    pub text: String,
    pub report: SpeechProjectionReport,
}

/// Longest kaomoji interior (characters between the parentheses).
const MAX_KAOMOJI_INNER: usize = 15;

/// Project display text to the text sent to TTS. See the module docs.
pub fn project_speech_text(display: &str) -> SpeechProjection {
    let chars: Vec<char> = display.chars().collect();
    let mut out = String::with_capacity(display.len());
    let mut report = SpeechProjectionReport::default();
    let mut index = 0;
    while index < chars.len() {
        if let Some(end) = url_end(&chars, index) {
            report.dropped_urls += 1;
            index = end;
            continue;
        }
        if let Some(end) = kaomoji_end(&chars, index) {
            report.dropped_kaomoji += 1;
            index = end;
            continue;
        }
        let c = chars[index];
        if is_emoji_part(c) {
            // One count per run, not per code point (a skin-toned or joined
            // emoji is several).
            if !index
                .checked_sub(1)
                .is_some_and(|previous| is_emoji_part(chars[previous]))
            {
                report.dropped_emoji += 1;
            }
            index += 1;
            continue;
        }
        out.push(c);
        index += 1;
    }
    let text = if report.changed() {
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        // Nothing was deleted: the text is exactly the input (no whitespace
        // normalisation of a reply that needed none).
        display.to_owned()
    };
    SpeechProjection { text, report }
}

fn starts_with_ci(chars: &[char], at: usize, needle: &str) -> bool {
    let mut position = at;
    for expected in needle.chars() {
        match chars.get(position) {
            Some(c) if c.eq_ignore_ascii_case(&expected) => position += 1,
            _ => return false,
        }
    }
    true
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_graphic() && !matches!(c, '"' | '<' | '>' | '`')
}

/// End (exclusive) of a URL starting at `start`, if one does.
fn url_end(chars: &[char], start: usize) -> Option<usize> {
    // A URL starts at a word boundary: `awww.x` is not one.
    if start > 0 && chars[start - 1].is_ascii_alphanumeric() {
        return None;
    }
    let scheme = ["https://", "http://", "ftp://"]
        .iter()
        .find(|scheme| starts_with_ci(chars, start, scheme));
    let after_prefix = match scheme {
        Some(scheme) => start + scheme.len(),
        None if starts_with_ci(chars, start, "www.")
            && chars
                .get(start + 4)
                .is_some_and(|c| c.is_ascii_alphanumeric()) =>
        {
            start + 4
        }
        None => return None,
    };
    let mut end = after_prefix;
    while chars.get(end).is_some_and(|c| is_url_char(*c)) {
        end += 1;
    }
    // Sentence punctuation right after a URL belongs to the sentence.
    while end > after_prefix
        && matches!(
            chars[end - 1],
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\''
        )
    {
        end -= 1;
    }
    Some(end)
}

/// End (exclusive) of a parenthesised kaomoji opening at `start`, if one does.
fn kaomoji_end(chars: &[char], start: usize) -> Option<usize> {
    if !matches!(chars[start], '(' | '（') {
        return None;
    }
    let mut inner = 0;
    let mut all_plain = true;
    for (offset, c) in chars[start + 1..].iter().enumerate() {
        if matches!(c, ')' | '）') {
            // Two or more symbols, not just punctuation such as `(...)`.
            return (inner >= 2 && !all_plain).then_some(start + 1 + offset + 1);
        }
        inner += 1;
        if inner > MAX_KAOMOJI_INNER
            || c.is_alphanumeric()
            || c.is_whitespace()
            || matches!(c, '(' | '（' | '、' | '。')
        {
            return None;
        }
        if !matches!(c, '.' | ',' | '!' | '?' | '…' | '-' | '・') {
            all_plain = false;
        }
    }
    None
}

/// Emoji code points and the joiners that build sequences. Deliberately
/// narrower than "every pictograph": `★ ☆ ♀ ♂`, the card suits `♡ ♥` and the
/// music notes `♪ ♫` are ordinary Japanese text decoration and pass through.
pub(crate) fn is_emoji_part(c: char) -> bool {
    let code = c as u32;
    matches!(
        code,
        0x1F000..=0x1FAFF
            | 0x2600..=0x27BF
            | 0x231A..=0x231B
            | 0x23E9..=0x23F3
            | 0x23F8..=0x23FA
            | 0x25B6
            | 0x25C0
            | 0x2934..=0x2935
            | 0x2B05..=0x2B07
            | 0x2B1B..=0x2B1C
            | 0x2B50
            | 0x2B55
            | 0x3297
            | 0x3299
            // Selectors, joiner, keycap and tag characters of emoji sequences.
            | 0x200D
            | 0x20E3
            | 0xFE0E..=0xFE0F
            | 0xE0020..=0xE007F
    ) && !matches!(code, 0x2605 | 0x2606 | 0x2640 | 0x2642 | 0x2660..=0x266F)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Item {
        category: String,
        text: String,
        expected: String,
    }

    /// The EXP-0003 handling set (`examples/benchmarks/tts/corpus.py`
    /// HANDLING), with the speech text ADR-0004 D-5 requires for each item.
    /// Numbers, English words and kanji items must pass through unchanged:
    /// only URLs, emoji and kaomoji are in scope.
    fn handling_set() -> Vec<Item> {
        serde_json::from_str(include_str!(
            "../../../examples/tts-projection/handling-set.json"
        ))
        .expect("fixture parses")
    }

    #[test]
    fn the_exp_0003_handling_set_projects_as_adr_0004_d5_requires() {
        let items = handling_set();
        assert_eq!(
            items.len(),
            55,
            "the EXP-0003 handling set (38) plus the brand-word items (17)"
        );
        for item in &items {
            let projection = project_speech_text(&item.text);
            assert_eq!(
                projection.text, item.expected,
                "{} item {:?}",
                item.category, item.text
            );
        }
        // Every category outside D-5 is untouched.
        for item in items
            .iter()
            .filter(|i| !matches!(i.category.as_str(), "url" | "emoji"))
        {
            assert_eq!(item.text, item.expected, "{}", item.category);
            assert!(!project_speech_text(&item.text).report.changed());
        }
    }

    #[test]
    fn urls_are_dropped_and_the_sentence_around_them_survives() {
        let p = project_speech_text("詳しくは https://example.com/live を見てね。");
        assert_eq!(p.text, "詳しくは を見てね。");
        assert_eq!(p.report.dropped_urls, 1);
        assert_eq!(
            project_speech_text("概要欄のリンクはこちら www.example.jp です。").text,
            "概要欄のリンクはこちら です。"
        );
        // A URL that abuts Japanese text ends at the first non-ASCII character.
        assert_eq!(
            project_speech_text("見て→http://a.example/x?y=1&z=2を").text,
            "見て→を"
        );
        // Trailing sentence punctuation is kept, case and scheme variants drop.
        assert_eq!(
            project_speech_text("HTTPS://EXAMPLE.COM/A, ftp://h/f.").text,
            ", ."
        );
        assert_eq!(
            project_speech_text("(see https://x.example/a)").text,
            "(see )"
        );
        // Not URLs: no scheme, no www, or inside a word.
        for kept in [
            "example.com は便利",
            "awww.example.com",
            "www. だけ",
            "http:/x",
        ] {
            assert_eq!(project_speech_text(kept).text, kept);
        }
    }

    #[test]
    fn emoji_runs_are_dropped_and_japanese_decoration_is_kept() {
        let p = project_speech_text("ありがとう😊また来てね✨");
        assert_eq!(p.text, "ありがとうまた来てね");
        assert_eq!(p.report.dropped_emoji, 2);
        assert_eq!(project_speech_text("やったー🎉🎉🎉").text, "やったー");
        assert_eq!(
            project_speech_text("やったー🎉🎉🎉").report.dropped_emoji,
            1
        );
        // ZWJ sequence, skin tone, flag, keycap and text/emoji selectors.
        for emoji in ["👨‍👩‍👧", "👍🏽", "🇯🇵", "❤️", "✅", "⭐", "▶"] {
            assert_eq!(
                project_speech_text(&format!("あ{emoji}い")).text,
                "あい",
                "{emoji}"
            );
        }
        // Ordinary decoration and notes are text, not emoji.
        for kept in ["★お疲れ様☆", "歌うよ♪♫", "♡と♥は別", "♀♂", "◎○●"]
        {
            assert_eq!(project_speech_text(kept).text, kept, "{kept}");
        }
    }

    #[test]
    fn parenthesised_kaomoji_are_dropped_but_ordinary_parentheses_are_not() {
        let p = project_speech_text("えへへ(´▽`)照れるなあ。");
        assert_eq!(p.text, "えへへ照れるなあ。");
        assert_eq!(p.report.dropped_kaomoji, 1);
        for kaomoji in ["(^_^)", "(>_<)", "（＾＾）", "(*^-^*)", "(´;_;`)"] {
            let out = project_speech_text(&format!("やあ{kaomoji}よろしく"));
            assert_eq!(out.text, "やあよろしく", "{kaomoji}");
        }
        for kept in [
            "(笑)",
            "(1)",
            "(...)",
            "(!)",
            "()",
            "(a b)",
            "（ね）",
            "(T_T)",
        ] {
            assert_eq!(project_speech_text(kept).text, kept, "{kept}");
        }
        // An unterminated or over-long parenthesis is not a kaomoji.
        assert_eq!(project_speech_text("(^_^").text, "(^_^");
        let long = format!("({})", "^".repeat(MAX_KAOMOJI_INNER + 1));
        assert_eq!(project_speech_text(&long).text, long);
    }

    #[test]
    fn symbols_and_latin_words_pass_through_for_the_future_lexicon() {
        for kept in [
            "A&B、100%、#タグ、@メンション。",
            "今日はYouTubeでVTuberだよ",
        ] {
            let p = project_speech_text(kept);
            assert_eq!(p.text, kept);
            assert!(!p.report.changed());
        }
    }

    #[test]
    fn a_reply_with_nothing_speakable_projects_to_empty() {
        for only in [
            "😊",
            "https://example.com/x",
            "(^_^)",
            "😊 (^_^) www.example.jp ✨",
            "",
        ] {
            assert!(project_speech_text(only).text.trim().is_empty(), "{only}");
        }
    }

    #[test]
    fn projection_is_deletion_only_pure_and_never_longer() {
        let samples = [
            "こんにちは😊 https://example.com/a (^_^) またね✨",
            "今日はYouTubeでライブ🎉 www.example.jp",
            "",
            "plain",
        ];
        for text in samples {
            let once = project_speech_text(text);
            assert_eq!(once, project_speech_text(text), "deterministic");
            assert!(once.text.len() <= text.len());
            // Every output character comes from the input (derivability).
            assert!(once.text.chars().all(|c| c == ' ' || text.contains(c)));
            // Idempotent.
            assert_eq!(project_speech_text(&once.text).text, once.text);
        }
    }
}
