//! Issue #79 (S1): deterministic, bounded audience-signal aggregation.
//!
//! A burst of public chat is mostly the same few reactions ("草", "GG",
//! "おめでとう") or the same remark repeated by many viewers. Answering each
//! one is wasted work and answering a crowd as if it were one person is wrong.
//! This module turns such a cluster into one [`AudienceSignal`] that the burst
//! path ranks as a single crowd-level candidate.
//!
//! Load-bearing properties:
//!
//! 1. **Safety first, then absorption.** Every candidate passes
//!    [`guard_text`] before any grouping. A text that looks like a threat,
//!    scam, targeted abuse, link, mention, control/injection attempt, or that
//!    may be truncated, is *never* absorbed: it stays an individual candidate.
//!    The guard is a recall-oriented screen that can only *prevent* absorption.
//!    It never suppresses, deletes or acts on a message, so a false positive
//!    costs only a missed aggregation (the safe direction). It is not a
//!    moderation policy (#76/#77 own that).
//! 2. **Two deterministic layers only.** (a) exact / normalized duplicate
//!    coalescing (case, width, kana, whitespace, decoration and repeated-run
//!    folding) and (b) a small reviewed reaction-family table. There is no
//!    semantic/topic grouping (#80), no ANN and no learned model.
//! 3. **A crowd needs more than one voice.** A cluster needs at least
//!    `min_cluster_size` members *and* `min_unique_actors` distinct actors, so
//!    one account repeating itself is not inflated into an audience signal.
//! 4. **Pure and bounded.** No clock, thread, allocation proportional to
//!    anything but the (already hard-capped) batch, and no retained viewer
//!    text: a signal carries only a reviewed family label, counts and at most
//!    [`MAX_SIGNAL_REPRESENTATIVES`] event ids.
//! 5. **Data, not authority.** A signal is derived from untrusted text. It can
//!    neither bypass a lane nor raise the crowd candidate above the 0..=1000
//!    local score range, and carries no capability.
//!
//! [`AudienceSignal`] is Rust-internal and `Serialize`-only: no `Deserialize`
//! and no JSON Schema, so it cannot become a second implicit wire contract.
//! A later slice that sends it across a boundary must define the schema first.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

/// Version of the (Rust-internal) signal shape.
pub const AUDIENCE_SIGNAL_SCHEMA_VERSION: &str = "0.1.0";
/// Version of the grouping method, guard lists and reaction table. Bump it
/// whenever any of them change so replay records say which rules produced them.
pub const AUDIENCE_METHOD_VERSION: &str = "0.1.0";
/// Event ids retained per signal (the carrier first).
pub const MAX_SIGNAL_REPRESENTATIVES: usize = 3;
/// Hard ceiling on `min_cluster_size`.
pub const MAX_AUDIENCE_MIN_CLUSTER: usize = 16;
/// Longest text (in chars) that may be coalesced as a plain duplicate. Longer
/// messages are more likely substantive and are left individual.
pub const MAX_DUPLICATE_TEXT_CHARS: usize = 80;
/// Text at or beyond this many bytes may have been truncated by the candidate
/// bound (256 bytes, UTF-8 boundary safe), so it is never absorbed: a
/// truncated copy could hide a differing tail such as a link.
const POSSIBLY_TRUNCATED_BYTES: usize = crate::burst::MAX_CANDIDATE_TEXT_BYTES - 4;
/// Crowd score bonus per extra member, and its ceiling.
const CROWD_BONUS_PER_MEMBER: u16 = 20;
const CROWD_BONUS_MAX: u16 = 300;

/// Operator-facing configuration. Validated, never clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudienceConfig {
    /// Members a group needs to become a signal (at least 2).
    pub min_cluster_size: usize,
    /// Distinct actors a group needs (anonymous events share one slot).
    pub min_unique_actors: usize,
}

impl Default for AudienceConfig {
    fn default() -> Self {
        Self {
            min_cluster_size: 3,
            min_unique_actors: 2,
        }
    }
}

/// Refusal raised for an out-of-bounds [`AudienceConfig`] field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudienceConfigError {
    pub field: &'static str,
}

impl std::fmt::Display for AudienceConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid audience configuration: {}", self.field)
    }
}

impl std::error::Error for AudienceConfigError {}

impl AudienceConfig {
    pub fn validate(&self) -> Result<(), AudienceConfigError> {
        if !(2..=MAX_AUDIENCE_MIN_CLUSTER).contains(&self.min_cluster_size) {
            return Err(AudienceConfigError {
                field: "min_cluster_size",
            });
        }
        if self.min_unique_actors == 0 || self.min_unique_actors > self.min_cluster_size {
            return Err(AudienceConfigError {
                field: "min_unique_actors",
            });
        }
        Ok(())
    }
}

/// Reviewed reaction families. Adding one is a reviewed code change, never
/// data supplied by a viewer or a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactionFamily {
    Laughter,
    GoodGame,
    Praise,
    Congratulation,
    Greeting,
    Cheer,
    Surprise,
    Sympathy,
    Thanks,
}

/// What kind of crowd behaviour a signal describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AudienceSignalClass {
    /// Several viewers sent a reaction from one reviewed family.
    Reaction { family: ReactionFamily },
    /// Several viewers sent the same short remark.
    RepeatedText,
}

/// How members were grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregationMethod {
    /// Byte-identical (after trimming) texts.
    ExactDuplicate,
    /// Equal after case/width/kana/whitespace/decoration/run folding.
    NormalizedDuplicate,
    /// Every member is composed only of one reviewed family's tokens.
    ReactionFamily,
}

/// Direction of the group between the first and second half of its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudienceTrend {
    Rising,
    Steady,
    Falling,
    /// The window has no extent, so there is no first or second half.
    Unknown,
}

/// The logical window a signal summarises (the batch's own instants).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AudienceWindow {
    pub start_ms: u64,
    pub end_ms: u64,
}

/// A bounded, versioned summary of one repeated-reaction or repeated-remark
/// cluster. Contains no viewer text and no actor identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudienceSignal {
    pub schema_version: String,
    pub method_version: String,
    /// Batch the signal belongs to, and its position within that batch.
    pub batch_id: u64,
    pub index: u32,
    pub window: AudienceWindow,
    pub class: AudienceSignalClass,
    pub method: AggregationMethod,
    /// Members of the cluster, including the carrier.
    pub count: u32,
    /// Distinct actors among the members (anonymous events share one slot).
    /// Exact inside one bounded batch; named an estimate because later
    /// windowed slices may sketch it.
    pub unique_actors_estimate: u32,
    /// Share of the batch, `0..=1000`.
    pub intensity_milli: u16,
    pub trend: AudienceTrend,
    /// First members in arrival order; the first is the carrier that stands
    /// for the crowd in selection.
    pub representative_event_ids: Vec<String>,
    /// A prior for the method, not a calibrated probability.
    pub confidence_milli: u16,
}

/// Why a candidate was kept out of every cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardReason {
    Threat,
    Scam,
    Abuse,
    /// Control-plane vocabulary or a prompt-injection phrase in public chat.
    Injection,
    Link,
    Mention,
    /// Possibly cut by the candidate text bound.
    Truncated,
    /// Digits outside a reaction token (amounts, addresses, codes).
    Digits,
    /// Too long to be a plain duplicate.
    TooLong,
    /// Nothing left after normalization.
    Empty,
}

/// A candidate as the aggregator sees it. Borrowed, bounded by the batch.
#[derive(Debug, Clone, Copy)]
pub struct AudienceInput<'a> {
    pub event_id: &'a str,
    pub actor_id: Option<&'a str>,
    pub arrival_ms: u64,
    pub local_score_milli: u16,
    pub text: &'a str,
}

/// One aggregated cluster: members index into the input slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudienceCluster {
    /// Member indexes in arrival order.
    pub members: Vec<usize>,
    /// The member that stands for the crowd (the first).
    pub carrier: usize,
    /// Local score of the crowd candidate, `0..=1000`.
    pub crowd_score_milli: u16,
    pub signal: AudienceSignal,
}

/// Result of planning one batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudiencePlan {
    pub clusters: Vec<AudienceCluster>,
    /// Candidates kept out of grouping by the guard, by reason.
    pub guarded: BTreeMap<GuardReason, u64>,
    /// Candidates that were eligible but whose group stayed below threshold.
    pub below_threshold: u64,
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

fn is_decoration(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '!' | '.'
                | ','
                | '~'
                | '-'
                | '_'
                | '…'
                | '・'
                | '、'
                | '。'
                | '〜'
                | '♪'
                | '♡'
                | '☆'
                | '★'
                | 'ー'
        )
}

/// Case, width and kana folding. Full-width ASCII becomes ASCII, katakana
/// becomes hiragana, text is lowercased, and emoji modifiers/joiners are
/// dropped. Half-width katakana is deliberately not folded (kept distinct, so
/// it can only miss a group, never merge two different texts).
fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let c = match c {
            '\u{3000}' => ' ',
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(u32::from(c) - 0xFEE0).unwrap_or(c),
            '\u{30A1}'..='\u{30F6}' => char::from_u32(u32::from(c) - 0x60).unwrap_or(c),
            '\u{FE0E}' | '\u{FE0F}' | '\u{200D}' | '\u{1F3FB}'..='\u{1F3FF}' => continue,
            other => other,
        };
        out.extend(c.to_lowercase());
    }
    out
}

/// Folded text without decoration characters.
fn skeleton(folded: &str) -> Vec<char> {
    folded.chars().filter(|c| !is_decoration(*c)).collect()
}

/// Key for the normalized-duplicate layer: skeleton with runs collapsed.
fn duplicate_key(folded: &str) -> String {
    let mut out = String::new();
    let mut previous = None;
    for c in skeleton(folded) {
        if previous != Some(c) {
            out.push(c);
        }
        previous = Some(c);
    }
    out
}

// ---------------------------------------------------------------------------
// Safety guard
// ---------------------------------------------------------------------------

// Every term is folded at first use, so a reviewer writes natural text
// (katakana, capitals) and the comparison happens in folded space.
const THREAT_TERMS: &[&str] = &[
    "kill",
    "killing",
    "kys",
    "suicide",
    "die",
    "dies",
    "murder",
    "shoot",
    "stab",
    "bomb",
    "bombing",
    "swat",
    "swatting",
    "rape",
    "dox",
    "doxx",
    "doxxing",
    "i will find",
    "i know where",
    "go die",
    "see you dead",
    "report",
    "reported",
    "死ね",
    "しね",
    "殺",
    "ころす",
    "爆破",
    "爆弾",
    "火をつけ",
    "燃や",
    "襲",
    "刺す",
    "特定",
    "住所",
    "通報",
];

const SCAM_TERMS: &[&str] = &[
    "free",
    "giveaway",
    "crypto",
    "bitcoin",
    "btc",
    "eth",
    "nft",
    "wallet",
    "airdrop",
    "invest",
    "investment",
    "telegram",
    "whatsapp",
    "dm",
    "pm",
    "promo",
    "discount",
    "winner",
    "prize",
    "claim",
    "verify",
    "password",
    "login",
    "gift",
    "giftcard",
    "cashapp",
    "paypal",
    "venmo",
    "onlyfans",
    "sub4sub",
    "follow",
    "subscribe",
    "click",
    "link",
    "earn",
    "profit",
    "loan",
    "scam",
    "line",
    "無料",
    "副業",
    "儲",
    "稼げ",
    "稼ぐ",
    "出会い",
    "プレゼント",
    "当選",
    "投資",
    "仮想通貨",
    "暗号資産",
    "ライン",
    "クリック",
    "フォロー",
    "登録",
    "詐欺",
    "振込",
    "口座",
    "パスワード",
    "ログイン",
    "限定",
    "配布",
];

const ABUSE_TERMS: &[&str] = &[
    "idiot",
    "stupid",
    "moron",
    "trash",
    "ugly",
    "fat",
    "loser",
    "retard",
    "retarded",
    "shut up",
    "stfu",
    "fuck",
    "fucking",
    "shit",
    "bitch",
    "cunt",
    "slut",
    "whore",
    "nigger",
    "nigga",
    "faggot",
    "fag",
    "hate",
    "scum",
    "pig",
    "worthless",
    "pathetic",
    "消えろ",
    "うざ",
    "きもい",
    "きもち悪",
    "きしょい",
    "ブス",
    "デブ",
    "ハゲ",
    "バカ",
    "ばーか",
    "馬鹿",
    "アホ",
    "クズ",
    "カス",
    "ゴミ",
    "クソ",
    "黙れ",
    "だまれ",
    "嫌い",
    "最低",
    "辞めろ",
    "引退",
];

const INJECTION_TERMS: &[&str] = &[
    "obs.control",
    "tool.grant",
    "memory.admin",
    "performer.",
    "operator",
    "system prompt",
    "ignore previous",
    "ignore all",
    "jailbreak",
    "指示を無視",
    "プロンプト",
];

const LINK_MARKERS: &[&str] = &[
    "://", "www.", "http", ".com", ".net", ".org", ".io", ".gg", ".xyz", ".ly", ".me", ".co",
    ".jp", "t.me", "discord",
];

struct GuardTable {
    ascii: Vec<(GuardReason, String)>,
    other: Vec<(GuardReason, String)>,
}

fn guard_table() -> &'static GuardTable {
    static TABLE: OnceLock<GuardTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = GuardTable {
            ascii: Vec::new(),
            other: Vec::new(),
        };
        let lists: [(GuardReason, &[&str]); 4] = [
            (GuardReason::Threat, THREAT_TERMS),
            (GuardReason::Scam, SCAM_TERMS),
            (GuardReason::Abuse, ABUSE_TERMS),
            (GuardReason::Injection, INJECTION_TERMS),
        ];
        for (reason, terms) in lists {
            for term in terms {
                let folded = fold(term);
                if folded.is_ascii() && !folded.contains('.') {
                    table.ascii.push((reason, folded));
                } else {
                    table.other.push((reason, folded));
                }
            }
        }
        table
    })
}

/// Words of an already folded text, for whole-word matching of ASCII terms.
fn padded_words(folded: &str) -> String {
    let mut out = String::from(" ");
    let mut in_word = false;
    for c in folded.chars() {
        if c.is_ascii_alphanumeric() || c == '\'' {
            out.push(c);
            in_word = true;
        } else if in_word {
            out.push(' ');
            in_word = false;
        }
    }
    if in_word {
        out.push(' ');
    }
    out
}

/// The universal safety screen. Returns why a text must stay individual, or
/// `None` when nothing in it argues against grouping. Runs before any
/// grouping, on every candidate.
pub fn guard_text(text: &str) -> Option<GuardReason> {
    if text.len() >= POSSIBLY_TRUNCATED_BYTES {
        return Some(GuardReason::Truncated);
    }
    let folded = fold(text);
    if folded.chars().all(is_decoration) {
        return Some(GuardReason::Empty);
    }
    if folded.contains('@') {
        return Some(GuardReason::Mention);
    }
    if LINK_MARKERS.iter().any(|marker| folded.contains(marker)) {
        return Some(GuardReason::Link);
    }
    let table = guard_table();
    // The most serious reason wins so the metric names the real concern:
    // threat, then injection, scam, abuse.
    let mut found: Option<GuardReason> = None;
    let padded = padded_words(&folded);
    let mut consider = |reason: GuardReason| {
        let rank = |r: GuardReason| match r {
            GuardReason::Threat => 0,
            GuardReason::Injection => 1,
            GuardReason::Scam => 2,
            _ => 3,
        };
        if found.is_none_or(|current| rank(reason) < rank(current)) {
            found = Some(reason);
        }
    };
    for (reason, term) in &table.ascii {
        if padded.contains(&format!(" {term} ")) {
            consider(*reason);
        }
    }
    for (reason, term) in &table.other {
        if folded.contains(term.as_str()) {
            consider(*reason);
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Reaction-family table
// ---------------------------------------------------------------------------

// Written in natural form and folded at first use. Whole text must be built
// only from tokens of ONE family. A token matches an elongated spelling: each
// run of identical characters in the token may be repeated more times
// ("w" matches "wwww", "nice" matches "niiice"), but a token written with a
// doubled letter ("gg") needs at least that many.
const REACTION_TABLE: &[(ReactionFamily, &[&str])] = &[
    (
        ReactionFamily::Laughter,
        &[
            "草",
            "くさ",
            "笑",
            "w",
            "lol",
            "lmao",
            "lmfao",
            "ha",
            "はは",
            "わろた",
            "kusa",
            "lul",
            "kek",
            "hehe",
            "😂",
            "🤣",
        ],
    ),
    (ReactionFamily::GoodGame, &["gg", "wp", "ggwp", "gj", "gf"]),
    (
        ReactionFamily::Praise,
        &[
            "nice",
            "ナイス",
            "good",
            "great",
            "awesome",
            "cool",
            "いいね",
            "いい",
            "すごい",
            "すげえ",
            "すげ",
            "神",
            "うまい",
            "うま",
            "上手",
            "最高",
            "さいこう",
            "かっこいい",
            "かわいい",
            "cute",
            "👍",
            "👏",
            "8",
        ],
    ),
    (
        ReactionFamily::Congratulation,
        &[
            "おめでとうございます",
            "おめでとう",
            "おめでと",
            "おめ",
            "congrats",
            "congratulations",
            "grats",
            "祝",
            "🎉",
        ],
    ),
    (
        ReactionFamily::Greeting,
        &[
            "こんにちは",
            "こんにちわ",
            "こんばんは",
            "こんばんわ",
            "おはよう",
            "おはよ",
            "おは",
            "こんです",
            "ばんは",
            "hello",
            "hi",
            "hey",
            "やっほー",
            "初見",
            "はじめまして",
            "ただいま",
            "おかえり",
            "おつかれさまです",
            "おつかれさま",
            "おつかれ",
            "おつ",
            "乙",
            "おやすみ",
            "ばいばい",
            "bye",
        ],
    ),
    (
        ReactionFamily::Cheer,
        &[
            "がんばって",
            "がんばれ",
            "がんば",
            "頑張って",
            "頑張れ",
            "ファイト",
            "fight",
            "ganbare",
            "ganbatte",
            "応援",
            "gl",
            "glhf",
        ],
    ),
    (
        ReactionFamily::Surprise,
        &[
            "wow",
            "omg",
            "oh",
            "おお",
            "お",
            "え",
            "えっ",
            "えぇ",
            "うわ",
            "うお",
            "びっくり",
            "まじか",
            "まじ",
        ],
    ),
    (
        ReactionFamily::Sympathy,
        &[
            "f",
            "どんまい",
            "ざんねん",
            "残念",
            "おしい",
            "惜しい",
            "oof",
        ],
    ),
    (
        ReactionFamily::Thanks,
        &[
            "ありがとうございます",
            "ありがとう",
            "ありがと",
            "thanks",
            "thx",
            "ty",
            "あざす",
            "あざっす",
            "あざます",
            "感謝",
        ],
    ),
];

struct ReactionToken {
    family: ReactionFamily,
    /// Runs of identical characters: (char, minimum count).
    runs: Vec<(char, usize)>,
    /// Folded length, for longest-first ordering.
    len: usize,
}

fn reaction_tokens() -> &'static [ReactionToken] {
    static TOKENS: OnceLock<Vec<ReactionToken>> = OnceLock::new();
    TOKENS.get_or_init(|| {
        let mut tokens = Vec::new();
        for (family, list) in REACTION_TABLE {
            for raw in *list {
                let chars = skeleton(&fold(raw));
                let mut runs: Vec<(char, usize)> = Vec::new();
                for c in &chars {
                    match runs.last_mut() {
                        Some((last, n)) if last == c => *n += 1,
                        _ => runs.push((*c, 1)),
                    }
                }
                tokens.push(ReactionToken {
                    family: *family,
                    runs,
                    len: chars.len(),
                });
            }
        }
        // Longest first; ties by family then run content so the order (and so
        // the decomposition) is fully deterministic.
        tokens.sort_by(|a, b| {
            b.len
                .cmp(&a.len)
                .then(a.family.cmp(&b.family))
                .then_with(|| a.runs.cmp(&b.runs))
        });
        tokens
    })
}

fn match_token(token: &ReactionToken, text: &[char], start: usize) -> Option<usize> {
    let mut pos = start;
    for &(c, minimum) in &token.runs {
        let mut k = 0;
        while text.get(pos + k) == Some(&c) {
            k += 1;
        }
        if k < minimum {
            return None;
        }
        pos += k;
    }
    Some(pos)
}

/// The family a whole text belongs to, if it is composed only of tokens of
/// one reviewed family.
pub fn reaction_family(text: &str) -> Option<ReactionFamily> {
    let chars = skeleton(&fold(text));
    if chars.is_empty() {
        return None;
    }
    let tokens = reaction_tokens();
    let mut pos = 0;
    let mut family: Option<ReactionFamily> = None;
    while pos < chars.len() {
        let (next, token_family) = tokens
            .iter()
            .find_map(|token| match_token(token, &chars, pos).map(|end| (end, token.family)))?;
        if family.is_some_and(|current| current != token_family) {
            return None;
        }
        family = Some(token_family);
        pos = next;
    }
    family
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum GroupKey {
    Family(ReactionFamily),
    Text(String),
}

/// Group one batch. Deterministic: the result depends only on the inputs (in
/// arrival order), the window and the configuration.
pub fn plan_audience(
    inputs: &[AudienceInput<'_>],
    config: &AudienceConfig,
    batch_id: u64,
    window: AudienceWindow,
) -> AudiencePlan {
    let mut plan = AudiencePlan::default();
    let mut groups: BTreeMap<GroupKey, Vec<usize>> = BTreeMap::new();
    for (index, input) in inputs.iter().enumerate() {
        // Safety ordering: the guard sees every candidate before any grouping.
        if let Some(reason) = guard_text(input.text) {
            *plan.guarded.entry(reason).or_insert(0) += 1;
            continue;
        }
        if let Some(family) = reaction_family(input.text) {
            groups
                .entry(GroupKey::Family(family))
                .or_default()
                .push(index);
            continue;
        }
        let folded = fold(input.text);
        if folded.chars().any(|c| c.is_ascii_digit()) {
            *plan.guarded.entry(GuardReason::Digits).or_insert(0) += 1;
            continue;
        }
        if input.text.chars().count() > MAX_DUPLICATE_TEXT_CHARS {
            *plan.guarded.entry(GuardReason::TooLong).or_insert(0) += 1;
            continue;
        }
        let key = duplicate_key(&folded);
        if key.is_empty() {
            *plan.guarded.entry(GuardReason::Empty).or_insert(0) += 1;
            continue;
        }
        groups.entry(GroupKey::Text(key)).or_default().push(index);
    }

    let mut clusters = Vec::new();
    for (key, members) in groups {
        let unique = unique_actors(inputs, &members);
        if members.len() < config.min_cluster_size || unique < config.min_unique_actors {
            plan.below_threshold += members.len() as u64;
            continue;
        }
        clusters.push((key, members, unique));
    }
    // Arrival order of the carrier, so signal indexes do not depend on keys.
    clusters.sort_by_key(|(_, members, _)| members[0]);

    for (position, (key, members, unique)) in clusters.into_iter().enumerate() {
        let (class, method) = match &key {
            GroupKey::Family(family) => (
                AudienceSignalClass::Reaction { family: *family },
                AggregationMethod::ReactionFamily,
            ),
            GroupKey::Text(_) => {
                let first = inputs[members[0]].text.trim();
                let exact = members.iter().all(|&m| inputs[m].text.trim() == first);
                (
                    AudienceSignalClass::RepeatedText,
                    if exact {
                        AggregationMethod::ExactDuplicate
                    } else {
                        AggregationMethod::NormalizedDuplicate
                    },
                )
            }
        };
        let carrier = members[0];
        let count = members.len();
        let best = members
            .iter()
            .map(|&m| inputs[m].local_score_milli)
            .max()
            .unwrap_or(0);
        let bonus = (CROWD_BONUS_PER_MEMBER.saturating_mul((count - 1).min(64) as u16))
            .min(CROWD_BONUS_MAX);
        let crowd_score_milli = best.saturating_add(bonus).min(1000);
        plan.clusters.push(AudienceCluster {
            carrier,
            crowd_score_milli,
            signal: AudienceSignal {
                schema_version: AUDIENCE_SIGNAL_SCHEMA_VERSION.to_owned(),
                method_version: AUDIENCE_METHOD_VERSION.to_owned(),
                batch_id,
                index: position as u32,
                window,
                class,
                method,
                count: count as u32,
                unique_actors_estimate: unique as u32,
                intensity_milli: ((count * 1000) / inputs.len().max(1)).min(1000) as u16,
                trend: trend(inputs, &members, window),
                representative_event_ids: members
                    .iter()
                    .take(MAX_SIGNAL_REPRESENTATIVES)
                    .map(|&m| inputs[m].event_id.to_owned())
                    .collect(),
                confidence_milli: match method {
                    AggregationMethod::ExactDuplicate => 1000,
                    AggregationMethod::NormalizedDuplicate => 900,
                    AggregationMethod::ReactionFamily => 850,
                },
            },
            members,
        });
    }
    plan
}

fn unique_actors(inputs: &[AudienceInput<'_>], members: &[usize]) -> usize {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut anonymous = false;
    for &member in members {
        match inputs[member].actor_id {
            Some(actor) => {
                seen.insert(actor);
            }
            None => anonymous = true,
        }
    }
    seen.len() + usize::from(anonymous)
}

fn trend(inputs: &[AudienceInput<'_>], members: &[usize], window: AudienceWindow) -> AudienceTrend {
    if window.end_ms <= window.start_ms {
        return AudienceTrend::Unknown;
    }
    let midpoint = window.start_ms + (window.end_ms - window.start_ms) / 2;
    let second = members
        .iter()
        .filter(|&&m| inputs[m].arrival_ms >= midpoint)
        .count();
    let first = members.len() - second;
    match second.cmp(&first) {
        std::cmp::Ordering::Greater => AudienceTrend::Rising,
        std::cmp::Ordering::Less => AudienceTrend::Falling,
        std::cmp::Ordering::Equal => AudienceTrend::Steady,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(id: &'a str, actor: &'a str, at: u64, text: &'a str) -> AudienceInput<'a> {
        AudienceInput {
            event_id: id,
            actor_id: Some(actor),
            arrival_ms: at,
            local_score_milli: 100,
            text,
        }
    }

    const WINDOW: AudienceWindow = AudienceWindow {
        start_ms: 0,
        end_ms: 1000,
    };

    #[test]
    fn folding_unifies_width_case_and_kana() {
        assert_eq!(fold("ＧＧ"), "gg");
        assert_eq!(fold("ナイス"), "ないす");
        assert_eq!(fold("Ｗｗ"), "ww");
        assert_eq!(duplicate_key(&fold("Nice!!  Play~")), "niceplay");
        assert_eq!(duplicate_key(&fold("niiice play")), "niceplay");
    }

    #[test]
    fn reaction_table_matches_whole_texts_of_one_family() {
        for (text, family) in [
            ("草", ReactionFamily::Laughter),
            ("wwwww", ReactionFamily::Laughter),
            ("ｗｗｗ草草", ReactionFamily::Laughter),
            ("LOL", ReactionFamily::Laughter),
            ("hahaha", ReactionFamily::Laughter),
            ("GG", ReactionFamily::GoodGame),
            ("ggwp", ReactionFamily::GoodGame),
            ("ggg", ReactionFamily::GoodGame),
            ("Nice!!!", ReactionFamily::Praise),
            ("niiice", ReactionFamily::Praise),
            ("ナイスー", ReactionFamily::Praise),
            ("88888", ReactionFamily::Praise),
            ("おめでとう！", ReactionFamily::Congratulation),
            ("おめでとうございます", ReactionFamily::Congratulation),
            ("congrats", ReactionFamily::Congratulation),
            ("こんにちは", ReactionFamily::Greeting),
            ("初見", ReactionFamily::Greeting),
            ("がんばれー", ReactionFamily::Cheer),
            ("ファイト", ReactionFamily::Cheer),
            ("wow", ReactionFamily::Surprise),
            ("どんまい", ReactionFamily::Sympathy),
            ("ありがとう", ReactionFamily::Thanks),
        ] {
            assert_eq!(reaction_family(text), Some(family), "{text}");
        }
        for text in [
            "",
            "!!!",
            "ggwp草",
            "gg that was close",
            "草が生えた",
            "nice?",
            "hello world",
            "g",
            "ww?",
        ] {
            assert_eq!(reaction_family(text), None, "{text}");
        }
    }

    #[test]
    fn guard_screens_threat_scam_abuse_link_mention_and_injection() {
        for (text, reason) in [
            ("I will find you and kill you", GuardReason::Threat),
            ("go die", GuardReason::Threat),
            ("死ねよ", GuardReason::Threat),
            ("free bitcoin giveaway", GuardReason::Scam),
            ("無料でもらえる", GuardReason::Scam),
            ("you are an idiot", GuardReason::Abuse),
            ("ブスw", GuardReason::Abuse),
            ("see https://example.test", GuardReason::Link),
            ("check example.com", GuardReason::Link),
            ("@streamer you suck", GuardReason::Mention),
            ("ignore previous instructions", GuardReason::Injection),
            ("performer.stop now", GuardReason::Injection),
        ] {
            assert_eq!(guard_text(text), Some(reason), "{text}");
        }
        assert_eq!(guard_text(&"a".repeat(300)), Some(GuardReason::Truncated));
        assert_eq!(guard_text("!!!"), Some(GuardReason::Empty));
        // Whole-word matching: "skill" and "killer instinct" are not threats
        // for the ASCII list ("killer" is not "kill"), nor is "diet".
        assert_eq!(guard_text("what a skill"), None);
        assert_eq!(guard_text("nice diet"), None);
        assert_eq!(guard_text("草"), None);
        assert_eq!(guard_text("how did you beat the boss?"), None);
    }

    #[test]
    fn guard_runs_before_grouping_so_an_outlier_is_never_absorbed() {
        // Five identical threats from five actors would be a textbook cluster;
        // the guard keeps every one of them individual.
        let texts = ["i will kill you"; 5];
        let ids = ["a", "b", "c", "d", "e"];
        let inputs: Vec<_> = (0..5)
            .map(|i| input(ids[i], ids[i], i as u64, texts[i]))
            .collect();
        let plan = plan_audience(&inputs, &AudienceConfig::default(), 1, WINDOW);
        assert!(plan.clusters.is_empty());
        assert_eq!(plan.guarded[&GuardReason::Threat], 5);
    }

    #[test]
    fn repeated_topic_is_aggregated_and_outlier_preserved() {
        let inputs = vec![
            input("e0", "a0", 0, "草"),
            input("e1", "a1", 100, "ww"),
            input("e2", "a2", 200, "i will find you and kill you"),
            input("e3", "a3", 300, "wwww"),
            input("e4", "a4", 600, "草草"),
            input("e5", "a5", 700, "how did you do that boss?"),
            input("e6", "a6", 800, "lol"),
        ];
        let plan = plan_audience(&inputs, &AudienceConfig::default(), 7, WINDOW);
        assert_eq!(plan.clusters.len(), 1);
        let cluster = &plan.clusters[0];
        assert_eq!(cluster.members, vec![0, 1, 3, 4, 6]);
        assert_eq!(cluster.carrier, 0);
        assert!(!cluster.members.contains(&2), "outlier is not absorbed");
        let signal = &cluster.signal;
        assert_eq!(
            signal.class,
            AudienceSignalClass::Reaction {
                family: ReactionFamily::Laughter
            }
        );
        assert_eq!(signal.method, AggregationMethod::ReactionFamily);
        assert_eq!((signal.count, signal.unique_actors_estimate), (5, 5));
        assert_eq!(signal.intensity_milli, 714);
        assert_eq!(signal.trend, AudienceTrend::Falling);
        assert_eq!(signal.representative_event_ids, vec!["e0", "e1", "e3"]);
        assert_eq!(signal.batch_id, 7);
        assert_eq!(cluster.crowd_score_milli, 100 + 4 * CROWD_BONUS_PER_MEMBER);
    }

    #[test]
    fn one_voice_is_not_a_crowd() {
        let inputs: Vec<_> = (0..6)
            .map(|i| input(["a", "b", "c", "d", "e", "f"][i], "same", i as u64, "草"))
            .collect();
        let plan = plan_audience(&inputs, &AudienceConfig::default(), 1, WINDOW);
        assert!(plan.clusters.is_empty());
        assert_eq!(plan.below_threshold, 6);
    }

    #[test]
    fn repeated_remark_coalesces_exactly_or_after_normalization() {
        let exact = vec![
            input("e0", "a", 0, "when is the next stream?"),
            input("e1", "b", 1, "when is the next stream?"),
            input("e2", "c", 2, "when is the next stream?"),
        ];
        let plan = plan_audience(&exact, &AudienceConfig::default(), 1, WINDOW);
        assert_eq!(
            plan.clusters[0].signal.method,
            AggregationMethod::ExactDuplicate
        );
        assert_eq!(
            plan.clusters[0].signal.class,
            AudienceSignalClass::RepeatedText
        );

        let normalized = vec![
            input("e0", "a", 0, "When is the next stream?"),
            input("e1", "b", 1, "when is the next stream？"),
            input("e2", "c", 2, "WHEN IS THE NEXT STREAM?"),
        ];
        let plan = plan_audience(&normalized, &AudienceConfig::default(), 1, WINDOW);
        assert_eq!(plan.clusters.len(), 1);
        assert_eq!(
            plan.clusters[0].signal.method,
            AggregationMethod::NormalizedDuplicate
        );
    }

    #[test]
    fn digits_and_long_texts_are_never_coalesced_as_duplicates() {
        let digits: Vec<_> = (0..4)
            .map(|i| {
                input(
                    ["a", "b", "c", "d"][i],
                    ["a", "b", "c", "d"][i],
                    0,
                    "send 100 now",
                )
            })
            .collect();
        let plan = plan_audience(&digits, &AudienceConfig::default(), 1, WINDOW);
        assert!(plan.clusters.is_empty());
        assert_eq!(plan.guarded[&GuardReason::Digits], 4);

        let long = "the quick brown fox jumps over the lazy dog and keeps on running far away";
        let long = format!("{long} {long}");
        let inputs: Vec<_> = (0..4)
            .map(|i| input(["a", "b", "c", "d"][i], ["a", "b", "c", "d"][i], 0, &long))
            .collect();
        let plan = plan_audience(&inputs, &AudienceConfig::default(), 1, WINDOW);
        assert!(plan.clusters.is_empty());
        assert_eq!(plan.guarded[&GuardReason::TooLong], 4);
    }

    #[test]
    fn planning_is_deterministic_and_signal_indexes_follow_arrival() {
        let inputs = vec![
            input("e0", "a0", 0, "gg"),
            input("e1", "a1", 1, "草"),
            input("e2", "a2", 2, "gg"),
            input("e3", "a3", 3, "草"),
            input("e4", "a4", 4, "gg"),
            input("e5", "a5", 5, "草"),
        ];
        let first = plan_audience(&inputs, &AudienceConfig::default(), 1, WINDOW);
        let second = plan_audience(&inputs, &AudienceConfig::default(), 1, WINDOW);
        assert_eq!(first, second);
        assert_eq!(first.clusters.len(), 2);
        assert_eq!(first.clusters[0].carrier, 0);
        assert_eq!(first.clusters[0].signal.index, 0);
        assert_eq!(first.clusters[1].carrier, 1);
        assert_eq!(first.clusters[1].signal.index, 1);
    }

    #[test]
    fn config_is_validated_never_clamped() {
        assert!(AudienceConfig::default().validate().is_ok());
        for bad in [
            AudienceConfig {
                min_cluster_size: 1,
                min_unique_actors: 1,
            },
            AudienceConfig {
                min_cluster_size: MAX_AUDIENCE_MIN_CLUSTER + 1,
                min_unique_actors: 1,
            },
            AudienceConfig {
                min_cluster_size: 3,
                min_unique_actors: 0,
            },
            AudienceConfig {
                min_cluster_size: 3,
                min_unique_actors: 4,
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn crowd_score_is_bounded() {
        let inputs: Vec<_> = (0..16)
            .map(|i| AudienceInput {
                local_score_milli: 900,
                ..input("e", ["a", "b"][i % 2], i as u64, "草")
            })
            .collect();
        let plan = plan_audience(&inputs, &AudienceConfig::default(), 1, WINDOW);
        assert_eq!(plan.clusters[0].crowd_score_milli, 1000);
    }
}
