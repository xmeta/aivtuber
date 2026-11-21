//! Text-free repeated-phrase measurement and initiative refusal (issue #89 S1).
//!
//! Repeating a phrase word for word is the failure operators report most after
//! cost, and the TTS cache makes it free to do by accident. The
//! [`RepeatGuard`] keeps a bounded ring of *digests* of recently spoken
//! phrases, so it can count exact and near repeats and refuse an idle
//! initiative that would say the same thing again, without ever storing or
//! exposing what was said.
//!
//! # Normalisation
//!
//! [`normalize`] keeps letters and digits only, after case folding and a
//! bounded width fold: full-width ASCII becomes ASCII and half-width katakana
//! (with its voicing marks) becomes full-width katakana. Whitespace,
//! punctuation, symbols and emoji are dropped. This is a deliberate subset of
//! NFKC: no Unicode normalisation crate is in `Cargo.lock` and a dependency
//! change is out of scope, so other compatibility forms (circled digits,
//! ligatures, ...) are not folded. Hiragana and katakana are different
//! characters and are *not* unified. The result is cut at
//! [`NORMALIZED_MAX_CHARS`].
//!
//! # Digests, keys and replay
//!
//! Each entry holds a keyed SHA-256 digest of the normalised text and a sorted
//! set of keyed digests of its character bigrams (at most
//! [`NORMALIZED_MAX_CHARS`] - 1 of them), so a stored value cannot be reversed
//! by a dictionary without the key. The key is random per launch and never
//! leaves the process: status and diagnose carry counters only. Every decision
//! the guard makes is a function of digest *equality* and set sizes, never of
//! a digest's value, so the key cannot change a decision and a replay of the
//! same input trace under any key reaches the same counters and refusals. Time
//! is the logical `at_ms` the caller passes.
//!
//! # Bounds
//!
//! The spoken ring holds at most [`RING_CAPACITY`] entries and drops any older
//! than [`WINDOW_MS`] of logical time; the staged list (committed but not yet
//! heard, so a reply cannot repeat its own earlier phrase) holds at most
//! [`STAGED_CAPACITY`]; counters saturate.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Spoken phrases remembered.
pub const RING_CAPACITY: usize = 64;
/// Logical time an entry stays relevant (15 minutes).
pub const WINDOW_MS: u64 = 900_000;
/// Committed-but-not-yet-spoken phrases remembered for the refusal check.
pub const STAGED_CAPACITY: usize = 8;
/// Normalised characters kept per phrase (bounds the bigram set).
pub const NORMALIZED_MAX_CHARS: usize = 256;
/// A near repeat has bigram Jaccard similarity of at least `4 / 5`.
const NEAR_NUMERATOR: usize = 4;
const NEAR_DENOMINATOR: usize = 5;

/// Where a spoken phrase came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhraseOrigin {
    /// A generated answer to chat or speech.
    ChatReply,
    /// A generated idle initiative.
    Initiative,
    /// A cached or template reaction (not generated).
    Cached,
}

impl PhraseOrigin {
    pub const ALL: [Self; 3] = [Self::ChatReply, Self::Initiative, Self::Cached];

    pub fn name(self) -> &'static str {
        match self {
            Self::ChatReply => "chat_reply",
            Self::Initiative => "initiative",
            Self::Cached => "cached",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::ChatReply => 0,
            Self::Initiative => 1,
            Self::Cached => 2,
        }
    }
}

/// How a phrase relates to the remembered ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    None,
    Exact,
    Near,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    at_ms: u64,
    digest: u64,
    /// Sorted, deduplicated keyed digests of the character bigrams.
    bigrams: Vec<u64>,
}

/// The operator-visible, text-free repeat counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepeatStatus {
    pub phrases_spoken: u64,
    pub exact_repeats: u64,
    pub near_repeats: u64,
    /// Initiative phrases refused before commit because they repeated.
    pub suppressed_repeat: u64,
    /// `(origin, phrases_spoken, exact_repeats, near_repeats)`.
    pub by_origin: Vec<(String, u64, u64, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatGuard {
    key: [u8; 16],
    ring: VecDeque<Entry>,
    staged: VecDeque<Entry>,
    spoken: [u64; 3],
    exact: [u64; 3],
    near: [u64; 3],
    suppressed_repeat: u64,
}

impl RepeatGuard {
    /// A guard with the given per-launch key (tests pass a fixed one).
    pub fn with_key(key: [u8; 16]) -> Self {
        Self {
            key,
            ring: VecDeque::new(),
            staged: VecDeque::new(),
            spoken: [0; 3],
            exact: [0; 3],
            near: [0; 3],
            suppressed_repeat: 0,
        }
    }

    /// A guard keyed from the OS entropy source.
    pub fn random() -> Self {
        let mut key = [0_u8; 16];
        // Without entropy the key degrades to a fixed one: digests are then
        // guessable by dictionary, but no decision depends on the key, and
        // they never leave the process.
        if getrandom::fill(&mut key).is_err() {
            key = [0x5a; 16];
        }
        Self::with_key(key)
    }

    fn keyed(&self, tag: u8, bytes: &[u8]) -> u64 {
        let mut hasher = Sha256::new();
        hasher.update(self.key);
        hasher.update([tag]);
        hasher.update(bytes);
        let out = hasher.finalize();
        let mut head = [0_u8; 8];
        head.copy_from_slice(&out[..8]);
        u64::from_be_bytes(head)
    }

    fn entry(&self, text: &str, at_ms: u64) -> Option<Entry> {
        let normalized = normalize(text);
        if normalized.is_empty() {
            return None;
        }
        let chars: Vec<char> = normalized.chars().collect();
        let mut bigrams: Vec<u64> = chars
            .windows(2)
            .map(|pair| {
                let mut buf = [0_u8; 8];
                let mut used = 0;
                for c in pair {
                    used += c.encode_utf8(&mut buf[used..]).len();
                }
                self.keyed(2, &buf[..used])
            })
            .collect();
        bigrams.sort_unstable();
        bigrams.dedup();
        Some(Entry {
            at_ms,
            digest: self.keyed(1, normalized.as_bytes()),
            bigrams,
        })
    }

    fn evict(&mut self, now_ms: u64) {
        let live = |entry: &Entry| now_ms.saturating_sub(entry.at_ms) < WINDOW_MS;
        self.ring.retain(live);
        self.staged.retain(live);
    }

    fn classify(entries: &[&VecDeque<Entry>], candidate: &Entry) -> Repeat {
        let mut near = false;
        for list in entries {
            for seen in list.iter() {
                if seen.digest == candidate.digest {
                    return Repeat::Exact;
                }
                near = near || is_near(&seen.bigrams, &candidate.bigrams);
            }
        }
        if near { Repeat::Near } else { Repeat::None }
    }

    /// Whether `text` repeats a recently spoken or staged phrase. Pure apart
    /// from dropping entries older than the window.
    pub fn check(&mut self, text: &str, now_ms: u64) -> Repeat {
        self.evict(now_ms);
        match self.entry(text, now_ms) {
            Some(candidate) => Self::classify(&[&self.ring, &self.staged], &candidate),
            None => Repeat::None,
        }
    }

    /// An initiative phrase was refused because it repeated.
    pub fn note_suppressed_repeat(&mut self) {
        self.suppressed_repeat = self.suppressed_repeat.saturating_add(1);
    }

    /// A phrase was committed but is not heard yet: later phrases of the same
    /// reply are checked against it.
    pub fn stage(&mut self, text: &str, now_ms: u64) {
        self.evict(now_ms);
        if let Some(entry) = self.entry(text, now_ms) {
            if self.staged.len() == STAGED_CAPACITY {
                self.staged.pop_front();
            }
            self.staged.push_back(entry);
        }
    }

    /// Forget phrases that were staged but never heard.
    pub fn clear_staged(&mut self) {
        self.staged.clear();
    }

    /// A phrase was spoken: count it against what was said before, then
    /// remember it.
    pub fn record_spoken(&mut self, text: &str, origin: PhraseOrigin, now_ms: u64) -> Repeat {
        self.evict(now_ms);
        let slot = origin.index();
        self.spoken[slot] = self.spoken[slot].saturating_add(1);
        let Some(entry) = self.entry(text, now_ms) else {
            return Repeat::None;
        };
        let repeat = Self::classify(&[&self.ring], &entry);
        match repeat {
            Repeat::Exact => self.exact[slot] = self.exact[slot].saturating_add(1),
            Repeat::Near => self.near[slot] = self.near[slot].saturating_add(1),
            Repeat::None => {}
        }
        if let Some(position) = self
            .staged
            .iter()
            .position(|staged| staged.digest == entry.digest)
        {
            self.staged.remove(position);
        }
        if self.ring.len() == RING_CAPACITY {
            self.ring.pop_front();
        }
        self.ring.push_back(entry);
        repeat
    }

    pub fn status(&self) -> RepeatStatus {
        let sum = |counts: &[u64; 3]| counts.iter().fold(0_u64, |a, b| a.saturating_add(*b));
        RepeatStatus {
            phrases_spoken: sum(&self.spoken),
            exact_repeats: sum(&self.exact),
            near_repeats: sum(&self.near),
            suppressed_repeat: self.suppressed_repeat,
            by_origin: PhraseOrigin::ALL
                .into_iter()
                .map(|origin| {
                    let slot = origin.index();
                    (
                        origin.name().to_owned(),
                        self.spoken[slot],
                        self.exact[slot],
                        self.near[slot],
                    )
                })
                .collect(),
        }
    }

    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    pub fn staged_len(&self) -> usize {
        self.staged.len()
    }
}

/// Bigram Jaccard similarity of at least 4/5, in integers.
fn is_near(a: &[u64], b: &[u64]) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let (mut i, mut j, mut inter) = (0, 0, 0_usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                inter += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = a.len() + b.len() - inter;
    inter * NEAR_DENOMINATOR >= union * NEAR_NUMERATOR
}

const HALFWIDTH_KATAKANA: &str = "ヲァィゥェォャュョッーアイウエオカキクケコサシスセソタチツテトナニヌネノハヒフヘホマミムメモヤユヨラリルレロワン";

fn halfwidth_katakana(c: char) -> Option<char> {
    let code = c as u32;
    if (0xFF66..=0xFF9D).contains(&code) {
        HALFWIDTH_KATAKANA.chars().nth((code - 0xFF66) as usize)
    } else {
        None
    }
}

/// Add a voiced (`+1`) or semi-voiced (`+2`) mark to a full-width katakana.
fn apply_mark(base: char, semi: bool) -> Option<char> {
    let code = base as u32;
    let voiced = matches!(
        base,
        'カ' | 'キ'
            | 'ク'
            | 'ケ'
            | 'コ'
            | 'サ'
            | 'シ'
            | 'ス'
            | 'セ'
            | 'ソ'
            | 'タ'
            | 'チ'
            | 'ツ'
            | 'テ'
            | 'ト'
    );
    let h_row = matches!(base, 'ハ' | 'ヒ' | 'フ' | 'ヘ' | 'ホ');
    if semi {
        return h_row.then(|| char::from_u32(code + 2)).flatten();
    }
    if base == 'ウ' {
        return Some('ヴ');
    }
    (voiced || h_row)
        .then(|| char::from_u32(code + 1))
        .flatten()
}

/// The comparison form of a phrase; see the module docs.
pub fn normalize(text: &str) -> String {
    let mut out = String::new();
    let mut count = 0_usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if count >= NORMALIZED_MAX_CHARS {
            break;
        }
        let c = match c as u32 {
            0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            _ => c,
        };
        let c = match halfwidth_katakana(c) {
            Some(full) => {
                let mark = match chars.peek() {
                    Some('\u{FF9E}') => Some(false),
                    Some('\u{FF9F}') => Some(true),
                    _ => None,
                };
                match mark.and_then(|semi| apply_mark(full, semi)) {
                    Some(marked) => {
                        chars.next();
                        marked
                    }
                    None => full,
                }
            }
            None => c,
        };
        // A stray half-width voicing mark is a modifier letter; drop it.
        if !c.is_alphanumeric() || matches!(c, '\u{FF9E}' | '\u{FF9F}') {
            continue;
        }
        for lower in c.to_lowercase() {
            if count < NORMALIZED_MAX_CHARS {
                out.push(lower);
                count += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A phrase of distinct characters, so no two share a bigram.
    fn unique(n: u64) -> String {
        [0x4e00 + n, 0x5000 + n * 2, 0x5400 + n * 3, 0x5800 + n * 5]
            .iter()
            .filter_map(|code| char::from_u32(*code as u32))
            .collect()
    }

    fn guard() -> RepeatGuard {
        RepeatGuard::with_key([7; 16])
    }

    #[test]
    fn normalisation_folds_width_case_and_punctuation() {
        assert_eq!(normalize("Ｈｅｌｌｏ、 World!"), "helloworld");
        assert_eq!(normalize("ﾊﾟﾝﾀﾞ ｶﾞｲﾄﾞ ｳﾞｨ"), "パンダガイドヴィ");
        assert_eq!(normalize("パンダ、ガイド。"), "パンダガイド");
        assert_eq!(
            normalize("今日は… いい天気ですね😀"),
            "今日はいい天気ですね"
        );
        assert_eq!(normalize("１２３abc"), "123abc");
        // A lone mark with no base is dropped, not kept.
        assert_eq!(normalize("ｱﾞ"), "ア");
        assert!(normalize("!!! ... 😀").is_empty());
    }

    #[test]
    fn normalisation_is_bounded() {
        let long = "あ".repeat(NORMALIZED_MAX_CHARS * 3);
        assert_eq!(normalize(&long).chars().count(), NORMALIZED_MAX_CHARS);
    }

    #[test]
    fn exact_repeat_ignores_width_case_and_punctuation() {
        let mut g = guard();
        assert_eq!(
            g.record_spoken("Hello, world!", PhraseOrigin::Initiative, 0),
            Repeat::None
        );
        assert_eq!(g.check("ＨＥＬＬＯ world", 10), Repeat::Exact);
        assert_eq!(
            g.record_spoken("ﾊﾛｰ", PhraseOrigin::ChatReply, 20),
            Repeat::None
        );
        assert_eq!(
            g.record_spoken("ハロー！", PhraseOrigin::ChatReply, 30),
            Repeat::Exact
        );
        let status = g.status();
        assert_eq!(status.phrases_spoken, 3);
        assert_eq!(status.exact_repeats, 1);
        assert_eq!(status.by_origin[0], ("chat_reply".to_owned(), 2, 1, 0));
        assert_eq!(status.by_origin[1], ("initiative".to_owned(), 1, 0, 0));
    }

    #[test]
    fn near_threshold_is_jaccard_four_fifths() {
        let mut g = guard();
        // 11 distinct bigrams in the base; the variant adds one distinct
        // bigram on each side of an unchanged core.
        let base = "abcdefghijkl";
        g.record_spoken(base, PhraseOrigin::Initiative, 0);
        // Same set plus one new bigram: 11 / 12 >= 0.8.
        assert_eq!(g.check("abcdefghijklm", 1), Repeat::Near);
        // Two extra bigrams: 11 / 13 = 0.846 still near.
        assert_eq!(g.check("abcdefghijklmn", 1), Repeat::Near);
        // Three extra: 11 / 14 = 0.786 is below the threshold.
        assert_eq!(g.check("abcdefghijklmno", 1), Repeat::None);
        // Exactly 0.8: 8 shared of union 10.
        let mut g = guard();
        g.record_spoken("abcdefghi", PhraseOrigin::Initiative, 0); // 8 bigrams
        assert_eq!(g.check("abcdefghijk", 1), Repeat::Near); // 8 / 10
        assert_eq!(g.check("abcdefghijkl", 1), Repeat::None); // 8 / 11
        assert_eq!(g.check("xyzxyzxyz", 1), Repeat::None);
    }

    #[test]
    fn single_character_phrases_only_match_exactly() {
        let mut g = guard();
        g.record_spoken("あ", PhraseOrigin::Cached, 0);
        assert_eq!(g.check("あ", 1), Repeat::Exact);
        assert_eq!(g.check("い", 1), Repeat::None);
        assert_eq!(g.check("!!!", 1), Repeat::None);
    }

    #[test]
    fn the_ring_is_bounded_by_count_and_by_logical_time() {
        let mut g = guard();
        for n in 0..(RING_CAPACITY as u64 + 20) {
            g.record_spoken(&unique(n), PhraseOrigin::ChatReply, n);
        }
        assert_eq!(g.ring_len(), RING_CAPACITY);
        // The oldest were forgotten; the newest are remembered.
        assert_eq!(g.check(&unique(0), 100), Repeat::None);
        assert_eq!(
            g.check(&unique(RING_CAPACITY as u64 + 19), 100),
            Repeat::Exact
        );
        // Older than the window: forgotten.
        assert_eq!(
            g.check(&unique(RING_CAPACITY as u64 + 19), WINDOW_MS + 200),
            Repeat::None
        );
        assert_eq!(g.ring_len(), 0);
    }

    #[test]
    fn staged_phrases_block_a_repeat_inside_one_reply_and_are_bounded() {
        let mut g = guard();
        g.stage("ねえねえ、聞いてる？", 0);
        assert_eq!(g.check("ねえねえ聞いてる", 1), Repeat::Exact);
        // Hearing it moves it from staged to spoken (no double counting).
        g.record_spoken("ねえねえ、聞いてる？", PhraseOrigin::Initiative, 2);
        assert_eq!(g.staged_len(), 0);
        assert_eq!(g.ring_len(), 1);
        for n in 0..(STAGED_CAPACITY + 5) {
            g.stage(&unique(n as u64 + 500), 3);
        }
        assert_eq!(g.staged_len(), STAGED_CAPACITY);
        g.clear_staged();
        assert_eq!(g.staged_len(), 0);
    }

    #[test]
    fn the_key_changes_digests_but_never_decisions() {
        let trace = [
            "こんにちは、今日もいい天気ですね",
            "こんにちは今日もいい天気ですね！",
            "こんにちは、今日もいい天気ですねえ",
            "全然違う話をしよう",
            "ﾊﾛｰ",
            "ハロー",
        ];
        let run = |key: [u8; 16]| {
            let mut g = RepeatGuard::with_key(key);
            let seen: Vec<Repeat> = trace
                .iter()
                .enumerate()
                .map(|(n, text)| g.record_spoken(text, PhraseOrigin::Initiative, n as u64 * 1_000))
                .collect();
            (seen, g.status())
        };
        let a = run([1; 16]);
        assert_eq!(a, run([2; 16]));
        assert_eq!(a, run([1; 16]));
        assert_eq!(a.0[1], Repeat::Exact);
        assert_eq!(a.0[2], Repeat::Near);
        assert_eq!(a.0[5], Repeat::Exact);
    }

    #[test]
    fn nothing_readable_is_kept() {
        let mut g = guard();
        g.record_spoken("secret phrase", PhraseOrigin::ChatReply, 0);
        let debug = format!("{g:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("phrase"));
        let status = serde_json::to_string(&g.status()).expect("json");
        assert!(!status.contains("secret"));
    }
}
