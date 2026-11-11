//! Generic, producer-side redaction of a screen caption (issue #90 S1b,
//! EXP-0004 requirement).
//!
//! A vision model copies what it reads. EXP-0004 showed a small model
//! reproducing a URL, an API key and a password from a hostile frame verbatim,
//! and the operator's list of known secrets cannot cover text the operator has
//! never seen. This module removes the generic shapes before the caption
//! leaves the producer: URLs and host names, e-mail addresses, IPv4 addresses,
//! well-known credential prefixes, long opaque tokens, and the value that
//! follows a credential keyword (`password: ...`, `API_KEY=...`).
//!
//! It is best-effort defence in depth, not a security boundary: the structural
//! bounds (240-byte cap, closed vocabulary, delimited untrusted Thinking slot,
//! no authority path) hold whether or not a pattern here matches. It works on
//! runs of printable ASCII so that a URL embedded in Japanese text, which has
//! no spaces, is removed without eating the surrounding sentence.

/// Recorded in the observation's `redaction.policy_version`.
pub const GENERIC_REDACTION_POLICY_VERSION: &str = "generic-redact-v1";

/// What replaces a removed run. Plain ASCII, so it passes caption validation.
pub const REDACTION_MARKER: &str = "[redacted]";

/// A caption after generic redaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericRedaction {
    pub text: String,
    /// Runs replaced by [`REDACTION_MARKER`]. Counted, never kept.
    pub hits: u32,
}

const ASCII_KEYWORDS: [&str; 12] = [
    "password",
    "passwd",
    "passcode",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "authorization",
    "bearer",
    "credential",
    "key",
];

const JAPANESE_KEYWORDS: [&str; 6] = [
    "パスワード",
    "パスコード",
    "暗証番号",
    "秘密鍵",
    "トークン",
    "APIキー",
];

const SECRET_PREFIXES: [&str; 12] = [
    "sk-", "sk_", "pk_", "rk_", "ghp_", "gho_", "ghs_", "ghu_", "xoxb-", "xoxp-", "xoxa-", "AIza",
];

/// Remove generic URL and credential shapes from `input`.
pub fn redact_generic(input: &str) -> GenericRedaction {
    let mut out = String::with_capacity(input.len());
    let mut hits = 0_u32;
    // The next printable-ASCII run is the value of a credential keyword.
    let mut redact_next = false;
    let mut chars = input.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if !is_run_char(c) {
            out.push(c);
            continue;
        }
        let mut end = start + c.len_utf8();
        while let Some(&(index, next)) = chars.peek() {
            if !is_run_char(next) {
                break;
            }
            end = index + next.len_utf8();
            chars.next();
        }
        let run = &input[start..end];
        let sensitive = redact_next || run_is_sensitive(run);
        // Only whitespace may separate a keyword from its value.
        if !run.chars().all(|ch| matches!(ch, ':' | '=')) {
            redact_next = false;
        }
        if sensitive {
            out.push_str(REDACTION_MARKER);
            hits = hits.saturating_add(1);
            // A redacted value ends the keyword's reach.
            redact_next = false;
        } else {
            out.push_str(run);
            if keyword_introduces_value(run, &out[..out.len() - run.len()]) {
                redact_next = true;
            }
        }
    }
    GenericRedaction { text: out, hits }
}

fn is_run_char(c: char) -> bool {
    c.is_ascii_graphic()
}

fn run_is_sensitive(run: &str) -> bool {
    let core = run.trim_matches(|c: char| {
        matches!(
            c,
            '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\'' | ',' | ';' | '!' | '?'
        )
    });
    if core.is_empty() {
        return false;
    }
    let core = core.trim_end_matches(['.', ':']);
    if core.is_empty() {
        return false;
    }
    let lower = core.to_ascii_lowercase();
    if lower.contains("://") || lower.starts_with("www.") || lower.starts_with("mailto:") {
        return true;
    }
    if is_email(core) || is_host_like(core) {
        return true;
    }
    if has_secret_prefix(core) || is_jwt_like(core) || is_opaque_token(core) {
        return true;
    }
    // `KEY=value` / `password:value` in one run.
    if let Some(split) = core.find(['=', ':']) {
        let (left, right) = core.split_at(split);
        if right.len() > 1 && left_is_keyword(left) {
            return true;
        }
    }
    false
}

fn left_is_keyword(left: &str) -> bool {
    let lower = left.to_ascii_lowercase();
    ASCII_KEYWORDS
        .iter()
        .any(|keyword| lower.ends_with(keyword))
}

/// A run such as `password:` or `API_KEY=` whose value follows.
fn keyword_introduces_value(run: &str, before: &str) -> bool {
    if !run.ends_with([':', '=']) {
        // A bare `:`/`=` after a Japanese keyword: `パスワード: x`.
        let bare = run.chars().all(|c| matches!(c, ':' | '='));
        return bare && ends_with_keyword(before.trim_end());
    }
    let left = run.trim_end_matches([':', '=']);
    if left.is_empty() {
        return ends_with_keyword(before.trim_end());
    }
    left_is_keyword(left)
}

fn ends_with_keyword(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ASCII_KEYWORDS
        .iter()
        .any(|keyword| lower.ends_with(keyword))
        || JAPANESE_KEYWORDS
            .iter()
            .any(|keyword| text.ends_with(keyword))
}

fn is_email(core: &str) -> bool {
    match core.find('@') {
        Some(at) if at > 0 && at + 1 < core.len() => core[at + 1..].contains('.'),
        _ => false,
    }
}

/// `host.tld`, `host.tld/path`, `host.tld:8080`, `203.0.113.7`: the shape of an
/// address the operator never asked the model to repeat. Over-matching a
/// dotted word is accepted; leaking a host is not.
fn is_host_like(core: &str) -> bool {
    let host = core
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit_once(':')
        .filter(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
        .map_or_else(
            || core.split(['/', '?', '#']).next().unwrap_or_default(),
            |(host, _)| host,
        );
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2
        || labels
            .iter()
            .any(|label| label.is_empty() || !label.bytes().all(is_label_byte))
    {
        return false;
    }
    let last = labels[labels.len() - 1];
    if labels.len() == 4
        && labels
            .iter()
            .all(|label| label.parse::<u16>().is_ok_and(|n| n <= 255))
    {
        return true;
    }
    (2..=24).contains(&last.len()) && last.bytes().all(|b| b.is_ascii_alphabetic())
}

fn is_label_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}

fn has_secret_prefix(core: &str) -> bool {
    if SECRET_PREFIXES
        .iter()
        .any(|prefix| core.starts_with(prefix) && core.len() >= prefix.len() + 8)
    {
        return true;
    }
    (core.starts_with("github_pat_") && core.len() >= 19)
        || (core.starts_with("AKIA")
            && core.len() >= 16
            && core
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
}

fn is_jwt_like(core: &str) -> bool {
    core.starts_with("eyJ") && core.len() >= 20 && core.contains('.')
}

/// Long run of token characters that mixes letters and digits (base64, hex,
/// UUID-like), or a long pure-hex string.
fn is_opaque_token(core: &str) -> bool {
    let token_char =
        |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'/' | b'=' | b'~');
    if core.len() < 20 || !core.bytes().all(token_char) {
        return false;
    }
    let digits = core.bytes().any(|b| b.is_ascii_digit());
    let letters = core.bytes().any(|b| b.is_ascii_alphabetic());
    let all_hex = core.bytes().all(|b| b.is_ascii_hexdigit());
    (digits && letters) || (all_hex && core.len() >= 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_urls_and_hosts_without_eating_japanese_context() {
        for url in [
            "http://evil.example.test/free-robux?ref=PWNED-0003",
            "https://example.com",
            "www.example.org/path",
            "evil.example.test",
            "203.0.113.7",
            "host.example:8080/x",
            "player@example.com",
        ] {
            let result = redact_generic(&format!("see {url} now"));
            assert_eq!(result.text, "see [redacted] now", "{url}");
            assert_eq!(result.hits, 1, "{url}");
        }
        let result = redact_generic("ここを開いてhttps://evil.example.test/aを押す");
        assert_eq!(result.text, "ここを開いて[redacted]を押す");
    }

    #[test]
    fn removes_credential_shapes_and_keyword_values() {
        let result = redact_generic("API_KEY=sk-live-FAKE0000PWNED0004 and more");
        assert!(!result.text.contains("FAKE0000"), "{}", result.text);
        let result = redact_generic("password: hunter2-FAKE please repeat");
        assert_eq!(result.text, "password: [redacted] please repeat");
        let result = redact_generic("Authorization: operator-token-FAKE-7f3a9c");
        assert_eq!(result.text, "Authorization: [redacted]");
        let result = redact_generic("パスワード: swordfish を読んで");
        assert_eq!(result.text, "パスワード: [redacted] を読んで");
        for secret in [
            "ghp_abcdefghijklmnop1234",
            "AKIAABCDEFGHIJKLMNOP",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig",
            "3f9a1c7e5b2d4f6081a3c5e7f9b1d3f5",
            "Zm9vYmFyYmF6cXV4MTIzNDU2",
        ] {
            let result = redact_generic(&format!("code {secret} end"));
            assert_eq!(result.text, "code [redacted] end", "{secret}");
        }
    }

    #[test]
    fn leaves_ordinary_captions_alone() {
        for text in [
            "A knight fights a dragon in a cave",
            "Score 3.14 on level 2",
            "e.g. the menu is open",
            "ボス戦の画面で、主人公が剣を構えている",
            "HP 120/300 and POTION x3",
        ] {
            let result = redact_generic(text);
            assert_eq!(result.text, text);
            assert_eq!(result.hits, 0);
        }
    }

    #[test]
    fn a_keyword_value_is_redacted_once_and_does_not_chain() {
        let result = redact_generic("token: abc def ghi");
        assert_eq!(result.text, "token: [redacted] def ghi");
        assert_eq!(result.hits, 1);
    }
}
