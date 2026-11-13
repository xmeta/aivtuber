//! What an operator may type to pick a stream to read (#361).
//!
//! The operation panel and `aivtuberctl` both hand these strings to the app,
//! which passes the result to a sidecar as one argument (never through a
//! shell). The parsers are strict and return the normalized value: anything
//! that is not exactly a Twitch login, a YouTube video id or a plain watch,
//! live or short link is refused rather than repaired.

use url::{Host, Url};

/// Longest text either parser looks at; a longer paste is refused outright.
pub const MAX_CHAT_TARGET_CHARS: usize = 256;

/// A Twitch channel login: `[a-z0-9_]{3,25}` after lowercasing and trimming.
/// No `#`, no URL: the login is what the sidecar joins.
pub fn parse_twitch_login(input: &str) -> Result<String, &'static str> {
    let text = input.trim();
    if text.chars().count() > MAX_CHAT_TARGET_CHARS {
        return Err("the Twitch channel name is too long");
    }
    let login = text.to_ascii_lowercase();
    let valid = (3..=25).contains(&login.len())
        && login
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if valid {
        Ok(login)
    } else {
        Err("a Twitch channel name is 3 to 25 letters, digits or underscores")
    }
}

fn is_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A YouTube video id (11 characters of `[A-Za-z0-9_-]`) or one taken from
/// `youtube.com/watch?v=ID`, `youtube.com/live/ID` or `youtu.be/ID` (`http` or
/// `https`, no credentials, no port). Other hosts and paths are refused.
pub fn parse_youtube_video(input: &str) -> Result<String, &'static str> {
    const REFUSED: &str =
        "give an 11-character video id or a youtube.com watch/live or youtu.be link";
    let text = input.trim();
    if text.chars().count() > MAX_CHAT_TARGET_CHARS {
        return Err("the YouTube video id or link is too long");
    }
    if is_video_id(text) {
        return Ok(text.to_owned());
    }
    let url = Url::parse(text).map_err(|_| REFUSED)?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(REFUSED);
    }
    let Some(Host::Domain(host)) = url.host() else {
        return Err(REFUSED);
    };
    let mut segments = url.path_segments().ok_or(REFUSED)?;
    let id = match host {
        "youtube.com" | "www.youtube.com" | "m.youtube.com" => match segments.next() {
            Some("watch") if segments.next().is_none_or(str::is_empty) => url
                .query_pairs()
                .find(|(key, _)| key == "v")
                .map(|(_, value)| value.into_owned())
                .ok_or(REFUSED)?,
            Some("live") => segments.next().ok_or(REFUSED)?.to_owned(),
            _ => return Err(REFUSED),
        },
        "youtu.be" => segments.next().ok_or(REFUSED)?.to_owned(),
        _ => return Err(REFUSED),
    };
    if is_video_id(&id) {
        Ok(id)
    } else {
        Err(REFUSED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twitch_logins_are_lowercased_and_strict() {
        assert_eq!(
            parse_twitch_login("  Some_Channel1 ").unwrap(),
            "some_channel1"
        );
        assert_eq!(parse_twitch_login("abc").unwrap(), "abc");
        assert!(parse_twitch_login(&"a".repeat(25)).is_ok());
        for bad in [
            "",
            "ab",
            "#abc",
            "a b",
            "ab\nc",
            "ab-c",
            "abc/def",
            "日本語です",
            "abc;rm",
            "--channel",
            "https://twitch.tv/abc",
        ] {
            assert!(parse_twitch_login(bad).is_err(), "{bad:?}");
        }
        assert!(parse_twitch_login(&"a".repeat(26)).is_err());
        assert!(parse_twitch_login(&"a".repeat(5_000)).is_err());
    }

    #[test]
    fn youtube_ids_and_the_three_link_shapes_are_accepted() {
        const ID: &str = "dQw4w9WgXcQ";
        for ok in [
            ID,
            " dQw4w9WgXcQ ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?v=dQw4w9WgXcQ&t=10s&list=x",
            "http://m.youtube.com/watch?feature=share&v=dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ?feature=share",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?si=abc",
        ] {
            assert_eq!(parse_youtube_video(ok).unwrap(), ID, "{ok}");
        }
        assert_eq!(parse_youtube_video("-_-_-_-_-_-").unwrap(), "-_-_-_-_-_-");
    }

    #[test]
    fn youtube_input_that_is_not_exactly_a_video_is_refused() {
        for bad in [
            "",
            "short",
            "dQw4w9WgXcQ1",
            "dQw4w9WgXc!",
            "https://example.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com.evil.example/watch?v=dQw4w9WgXcQ",
            "https://evil.example/youtu.be/dQw4w9WgXcQ",
            "https://user:pw@www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com:8443/watch?v=dQw4w9WgXcQ",
            "ftp://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "javascript:alert(1)",
            "https://www.youtube.com/channel/UC1234567890123456789012",
            "https://www.youtube.com/watch",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/watch/extra?v=dQw4w9WgXcQ",
            "https://www.youtube.com/live/",
            "https://youtu.be/",
            "https://[::1]/watch?v=dQw4w9WgXcQ",
        ] {
            assert!(parse_youtube_video(bad).is_err(), "{bad:?}");
        }
        assert!(parse_youtube_video(&format!("https://youtu.be/{}", "a".repeat(400))).is_err());
    }
}
