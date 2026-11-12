//! Plain-text chat for `aivtuber-setup start` (issue #334).
//!
//! The app reads one Event Envelope JSON per stdin line. This module sits
//! between the terminal and that pipe: a line that starts with `{` is passed
//! through unchanged (existing envelope users keep working), any other
//! non-empty line is wrapped as ordinary viewer chat.
//!
//! The wrapper grants no authority. It always emits `public_chat` /
//! `untrusted` / `content` / `chat.message`, with no `authorization` claim and
//! no payload field except `text`; the app's normalizer and ingress policy
//! still decide what that is worth, exactly as for a hand-written envelope.
//! Nothing here can mint a privileged capability (see
//! docs/security-threat-model.adoc).

use aivtuber_domain::{
    EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, SecurityPlane, SourceClass, TrustLevel,
};
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

/// Fixed source name, so the app's per-source rate limit sees one source.
pub const TERMINAL_SOURCE: &str = "local-terminal";
pub const TERMINAL_ACTOR: &str = "local:terminal";
/// Mirrors the app's default `max_payload_bytes` (runtime SecurityRuntimeConfig):
/// the JSON payload `{"text":...}` must fit, measured in UTF-8 bytes.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
/// Raw line cap read from the terminal; keeps memory bounded for any input.
/// Below the app's 64 KiB raw record cap so a passed-through envelope fits.
pub const MAX_LINE_BYTES: usize = 60 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Wrapped {
    /// Write these bytes (one JSON record, no newline) to the app.
    Send(Vec<u8>),
    /// Blank line; nothing to do.
    Skip,
    /// Refused here, with a short message for the user.
    Reject(String),
}

/// Turns terminal lines into app records, numbering the wrapped ones.
pub struct ChatWrapper {
    sequence: u64,
    now: fn() -> SystemTime,
}

impl Default for ChatWrapper {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatWrapper {
    pub fn new() -> Self {
        Self::with_clock(SystemTime::now)
    }

    pub fn with_clock(now: fn() -> SystemTime) -> Self {
        Self { sequence: 0, now }
    }

    /// `line` has no trailing newline. A leading BOM (some Windows pipes) is dropped.
    pub fn wrap(&mut self, line: &str) -> Wrapped {
        let line = line.strip_prefix('\u{feff}').unwrap_or(line);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Wrapped::Skip;
        }
        if trimmed.starts_with('{') {
            return Wrapped::Send(trimmed.as_bytes().to_vec());
        }
        let mut payload = BTreeMap::new();
        payload.insert(
            "text".to_owned(),
            serde_json::Value::String(trimmed.to_owned()),
        );
        let payload_bytes = serde_json::to_vec(&payload).map_or(usize::MAX, |v| v.len());
        if payload_bytes > MAX_PAYLOAD_BYTES {
            return Wrapped::Reject(format!(
                "line too long ({payload_bytes} bytes as JSON, the limit is {MAX_PAYLOAD_BYTES}); not sent"
            ));
        }
        self.sequence += 1;
        let n = self.sequence;
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: format!("{TERMINAL_SOURCE}-{n}"),
            correlation_id: format!("{TERMINAL_SOURCE}-{n}"),
            sequence: n,
            observed_at: utc_rfc3339((self.now)()),
            source: TERMINAL_SOURCE.to_owned(),
            source_class: SourceClass::PublicChat,
            plane: SecurityPlane::Content,
            trust_level: TrustLevel::Untrusted,
            kind: EventKind::ChatMessage,
            actor_id: Some(TERMINAL_ACTOR.to_owned()),
            priority_hint: None,
            authorization: None,
            payload,
        };
        match serde_json::to_vec(&envelope) {
            Ok(bytes) => Wrapped::Send(bytes),
            Err(e) => Wrapped::Reject(format!("cannot encode the line: {e}")),
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a UTC instant (civil-from-days, no time crate).
pub fn utc_rfc3339(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// Reads terminal lines until EOF, writing one record per accepted line to `out`
/// and short refusals to `notice`. Returns on EOF or when `out` is closed
/// (the app exited). Memory is bounded by `MAX_LINE_BYTES` per line. On
/// Windows consoles Rust's stdin already decodes UTF-16 console input to
/// UTF-8, so the code page does not matter; invalid UTF-8 from a pipe is
/// refused rather than guessed at.
pub fn pump<R: BufRead, W: Write, N: Write>(
    mut input: R,
    mut out: W,
    mut notice: N,
    wrapper: &mut ChatWrapper,
) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read = (&mut input)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut buf);
        match read {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if buf.len() > MAX_LINE_BYTES && buf.last() != Some(&b'\n') {
            // Drain the rest of the oversized line so the next one starts clean.
            loop {
                let (consumed, done) = match input.fill_buf() {
                    Ok([]) | Err(_) => (0, true),
                    Ok(chunk) => match chunk.iter().position(|b| *b == b'\n') {
                        Some(i) => (i + 1, true),
                        None => (chunk.len(), false),
                    },
                };
                input.consume(consumed);
                if done {
                    break;
                }
            }
            let _ = writeln!(notice, "line too long; not sent");
            continue;
        }
        let record = match std::str::from_utf8(&buf) {
            Ok(text) => wrapper.wrap(text.trim_end_matches(['\n', '\r'])),
            Err(_) => Wrapped::Reject("input is not valid UTF-8; not sent".to_owned()),
        };
        match record {
            Wrapped::Skip => {}
            Wrapped::Reject(message) => {
                let _ = writeln!(notice, "{message}");
            }
            Wrapped::Send(mut bytes) => {
                bytes.push(b'\n');
                if out.write_all(&bytes).and_then(|()| out.flush()).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::Duration;

    fn fixed() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_794_484_800) // 2026-11-12T12:00:00Z
    }

    fn wrapper() -> ChatWrapper {
        ChatWrapper::with_clock(fixed)
    }

    fn send(w: Wrapped) -> serde_json::Value {
        match w {
            Wrapped::Send(b) => serde_json::from_slice(&b).unwrap(),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn plain_japanese_is_wrapped_as_untrusted_public_chat() {
        let v = send(wrapper().wrap("こんにちは！"));
        assert_eq!(v["source_class"], "public_chat");
        assert_eq!(v["trust_level"], "untrusted");
        assert_eq!(v["plane"], "content");
        assert_eq!(v["kind"], "chat.message");
        assert_eq!(v["source"], TERMINAL_SOURCE);
        assert_eq!(v["actor_id"], TERMINAL_ACTOR);
        assert_eq!(v["observed_at"], "2026-11-12T12:00:00Z");
        assert_eq!(v["payload"], serde_json::json!({"text": "こんにちは！"}));
        assert!(v.get("authorization").is_none());
    }

    #[test]
    fn the_wrapped_envelope_is_valid_and_matches_the_schema() {
        let v = send(wrapper().wrap("hello"));
        let env: EventEnvelope = serde_json::from_value(v.clone()).unwrap();
        env.validate().unwrap();
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../schemas/event-envelope.schema.json"))
                .unwrap();
        let props = schema["properties"].as_object().unwrap();
        for key in v.as_object().unwrap().keys() {
            assert!(props.contains_key(key), "{key} not in schema");
        }
        for key in schema["required"].as_array().unwrap() {
            assert!(v.get(key.as_str().unwrap()).is_some(), "missing {key}");
        }
        for field in ["source_class", "plane", "trust_level", "kind"] {
            let allowed = props[field]["enum"].as_array().unwrap();
            assert!(allowed.contains(&v[field]), "{field}");
        }
    }

    #[test]
    fn envelope_json_passes_through_unchanged() {
        let line = r#"{"schema_version":"0.2.0","event_id":"e"}"#;
        assert_eq!(
            wrapper().wrap(&format!("  {line}  ")),
            Wrapped::Send(line.as_bytes().to_vec())
        );
    }

    #[test]
    fn blank_lines_are_skipped_and_a_leading_bom_is_ignored() {
        let mut w = wrapper();
        assert_eq!(w.wrap(""), Wrapped::Skip);
        assert_eq!(w.wrap("  \t "), Wrapped::Skip);
        assert_eq!(w.wrap("\u{feff}"), Wrapped::Skip);
        assert_eq!(
            w.wrap("\u{feff}{\"a\":1}"),
            Wrapped::Send(b"{\"a\":1}".to_vec())
        );
        // Skipped and passed-through lines do not consume a sequence number.
        assert_eq!(send(w.wrap("hi"))["sequence"], 1);
    }

    #[test]
    fn oversize_text_is_rejected_without_consuming_a_sequence() {
        let mut w = wrapper();
        let big = "あ".repeat(MAX_PAYLOAD_BYTES / 3 + 1);
        assert!(matches!(w.wrap(&big), Wrapped::Reject(_)));
        assert_eq!(send(w.wrap("ok"))["sequence"], 1);
    }

    #[test]
    fn sequence_and_event_ids_rise_monotonically() {
        let mut w = wrapper();
        let a = send(w.wrap("one"));
        let b = send(w.wrap("two"));
        let c = send(w.wrap("three"));
        assert_eq!(
            [&a, &b, &c].map(|v| v["sequence"].as_u64().unwrap()),
            [1, 2, 3]
        );
        assert_ne!(a["event_id"], b["event_id"]);
        assert_ne!(b["event_id"], c["event_id"]);
    }

    #[test]
    fn utc_formatting_handles_leap_days_and_epoch() {
        assert_eq!(utc_rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_208_000);
        assert_eq!(utc_rfc3339(leap), "2024-02-29T12:00:00Z");
    }

    #[test]
    fn pump_wraps_skips_rejects_and_stops_at_eof() {
        let mut input = b"\xef\xbb\xbfhello\r\n\r\n".to_vec();
        input.extend_from_slice(b"{\"x\":1}\n");
        input.extend_from_slice(b"\xff\xfe bad\n");
        input.extend_from_slice(&vec![b'a'; MAX_LINE_BYTES + 10]);
        input.extend_from_slice(b"\nlast");
        let (mut out, mut notice) = (Vec::new(), Vec::new());
        pump(Cursor::new(input), &mut out, &mut notice, &mut wrapper());
        let lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("\"text\":\"hello\""));
        assert_eq!(lines[1], "{\"x\":1}");
        assert!(lines[2].contains("\"text\":\"last\""));
        assert!(lines[2].contains("\"sequence\":2"));
        let notice = String::from_utf8(notice).unwrap();
        assert!(
            notice.contains("UTF-8") && notice.contains("too long"),
            "{notice}"
        );
    }
}
