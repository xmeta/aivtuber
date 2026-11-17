//! Short Japanese-first ready guide printed on stderr under `AIVTUBER_LOG=quiet` (#393).
//!
//! Fixed text, no locale detection, no catalogue. The whole quiet start-up adds
//! at most nine lines across the app and `aivtuber-setup start`: six here and
//! three in `crates/setup/src/start.rs`. The fourth app line is the voice-input
//! state (#397), printed only when `aivtuber-setup start` governs voice input;
//! a privacy-relevant fact (is the microphone open) must not be hidden by quiet.
//! The fifth is the character sheet line (#404), printed only when
//! `AIVTUBER_PERSONA_FILE` is set: whether the sheet is in effect, and that it
//! is sent to the cloud model when Thinking is remote, must not be hidden either.
//! The duty text itself (AI-generated
//! voice disclosure, licences) is never translated; the English notices printed
//! earlier stay authoritative.

use crate::persona::{PersonaLoad, PersonaRefusal};
use aivtuber_domain::PersonaError;

/// Printed after the panel URL line, only when the panel is running. Does not
/// repeat the URL or its secret.
pub const PANEL_HINT: &str = "準備ができました。上の「local operation panel URL」の行のURLをブラウザで開いてください(秘密を含むので共有しないでください)。 / Ready: open the URL on the \"local operation panel URL\" line above in a browser (it contains a secret; do not share it).";

/// Printed after [`PANEL_HINT`] (#388): how to see the URL again. Names a
/// command, never the URL.
pub const PANEL_URL_REPRINT: &str = "パネルの URL をもう一度表示: aivtuberctl panel-url / reprint the panel URL: aivtuberctl panel-url";

/// One-line reminder; the English notice above is the authoritative text.
pub const DISCLOSURE_REMINDER: &str = "配信ではAI音声・AI出演者であることを表示してください(正文は上の英語の通知です)。 / Disclose the AI-generated voice and AI performer on stream (the English notice above is authoritative).";

/// Quiet-mode replacement for `thinking_warmup: started`, so the model warm-up
/// is not a silent wait.
pub const WARMUP_NOTE: &str = "モデルを準備しています… / warming up the model…";

/// Voice input (#397), quiet form: Japanese first, English beside it. One line
/// each; none carries a secret, a path or a URL. The `cloud` forms say that
/// speech text leaves the machine because the Thinking model is remote. The
/// same lines are printed when the operator switches voice input in the panel
/// or with `aivtuberctl` (#400), which is why they name both ways.
pub const VOICE_OFF: &str = "マイクはオフです。使うには、操作パネルの「音声入力」か aivtuber-setup start --voice-input on をお使いください。 / Voice input is off; turn it on in the operation panel's voice input card or with `aivtuber-setup start --voice-input on`.";
pub const VOICE_OFF_CLOUD: &str = "マイクはオフです。オンにすると、聞き取った音声の文字起こしはクラウドの応答モデルに送信されます(操作パネルの「音声入力」か aivtuber-setup start --voice-input on)。 / Voice input is off; once on (in the operation panel or with `aivtuber-setup start --voice-input on`), transcribed speech is sent to the cloud model.";
pub const VOICE_ON: &str = "マイクで聞き取り中です(既定の録音デバイス)。止めるには、操作パネルの「音声入力」か aivtuber-setup start --voice-input off をお使いください。 / Voice input is on (listening on the default capture device); turn it off in the operation panel or with `aivtuber-setup start --voice-input off`.";
pub const VOICE_ON_CLOUD: &str = "マイクで聞き取り中です(既定の録音デバイス)。聞き取った音声の文字起こしはクラウドの応答モデルに送信されます。 / Voice input is on (listening on the default capture device); transcribed speech is sent to the cloud model.";

/// Verbose (diagnostics) form of the same line, in English like the other
/// diagnostic lines.
pub const VOICE_OFF_VERBOSE: &str = "voice input: off (microphone not opened); turn it on in the operation panel or with `aivtuber-setup start --voice-input on`";
pub const VOICE_OFF_CLOUD_VERBOSE: &str = "voice input: off (microphone not opened); turn it on in the operation panel or with `aivtuber-setup start --voice-input on`; once on, transcribed speech is sent to the cloud model";
pub const VOICE_ON_VERBOSE: &str = "voice input: on (listening on the default capture device)";
pub const VOICE_ON_CLOUD_VERBOSE: &str = "voice input: on (listening on the default capture device); transcribed speech is sent to the cloud model";

/// The voice-input line for the current state and verbosity. `cloud` is true
/// when Thinking leaves the machine (a non-loopback endpoint).
pub fn voice_input_line(on: bool, cloud: bool, diagnostics: bool) -> &'static str {
    match (on, cloud, diagnostics) {
        (true, false, true) => VOICE_ON_VERBOSE,
        (true, true, true) => VOICE_ON_CLOUD_VERBOSE,
        (false, false, true) => VOICE_OFF_VERBOSE,
        (false, true, true) => VOICE_OFF_CLOUD_VERBOSE,
        (true, false, false) => VOICE_ON,
        (true, true, false) => VOICE_ON_CLOUD,
        (false, false, false) => VOICE_OFF,
        (false, true, false) => VOICE_OFF_CLOUD,
    }
}

/// [`voice_input_line`], with the "default capture device" wording replaced
/// when the operator chose a device (#396), so the line never says "default"
/// about a chosen one. The device name itself is not printed here (the panel
/// shows it).
pub fn voice_input_text(on: bool, cloud: bool, diagnostics: bool, device_chosen: bool) -> String {
    let line = voice_input_line(on, cloud, diagnostics);
    if on && device_chosen {
        line.replace("既定の録音デバイス", "選択した録音デバイス")
            .replace("default capture device", "chosen capture device")
    } else {
        line.to_owned()
    }
}

/// The character sheet line (#404): always printed once, whichever way the
/// file read went, so the operator knows whether the character is in effect and
/// whether it leaves the machine. Quiet form is Japanese first with the English
/// half beside it; verbose is English like the other diagnostic lines. None
/// carries the text, a path or a URL: only the length, a SHA-256 prefix and a
/// fixed reason phrase.
pub fn persona_line(load: &PersonaLoad, cloud: bool, diagnostics: bool) -> String {
    match load {
        PersonaLoad::Loaded(sheet) => {
            let (bytes, hash) = (sheet.len(), sheet.sha256_prefix());
            match (cloud, diagnostics) {
                (false, true) => format!("persona: loaded bytes={bytes} sha256={hash}"),
                (true, true) => format!(
                    "persona: loaded bytes={bytes} sha256={hash}; the character sheet is sent to the cloud model with each reply"
                ),
                (false, false) => format!(
                    "キャラクター設定を読み込みました({bytes} バイト)。 / Character sheet loaded ({bytes} bytes, sha256 {hash})."
                ),
                (true, false) => format!(
                    "キャラクター設定を読み込みました({bytes} バイト)。この設定は返信のたびにクラウドの応答モデルへ送信されます。 / Character sheet loaded ({bytes} bytes, sha256 {hash}); it is sent to the cloud model with each reply."
                ),
            }
        }
        PersonaLoad::Absent => {
            if diagnostics {
                "persona: none (the file is missing or empty); replies use the default voice"
                    .to_owned()
            } else {
                "キャラクター設定はありません(ファイルが無いか空です)。 / No character sheet (the file is missing or empty).".to_owned()
            }
        }
        PersonaLoad::Refused(refusal) => {
            let (en, ja) = match refusal {
                PersonaRefusal::Unreadable => ("could not be read", "読み込めません"),
                PersonaRefusal::Invalid(PersonaError::NotUtf8) => {
                    ("not valid UTF-8", "UTF-8 ではありません")
                }
                PersonaRefusal::Invalid(PersonaError::ControlCharacter) => (
                    "contains a control character (only newline and tab are allowed)",
                    "制御文字が含まれています(改行とタブのみ可)",
                ),
                PersonaRefusal::Invalid(PersonaError::TooLarge) => {
                    ("larger than 3000 bytes", "3000 バイトを超えています")
                }
                PersonaRefusal::Invalid(PersonaError::ReservedTag) => (
                    "contains a <character tag, which the prompt reserves",
                    "<character タグが含まれています(予約済みです)",
                ),
            };
            if diagnostics {
                format!("persona: not loaded ({en}); replies use the default voice")
            } else {
                format!(
                    "キャラクター設定を読み込めませんでした({ja})。既定の話し方になります。 / Character sheet not loaded ({en}); replies use the default voice."
                )
            }
        }
    }
}

/// The lines printed once start-up reaches the panel URL / operator-control
/// point. Empty guidance for the panel is omitted when the panel is disabled.
pub fn ready_lines(panel_running: bool) -> Vec<&'static str> {
    let mut lines = Vec::with_capacity(2);
    if panel_running {
        lines.push(PANEL_HINT);
        lines.push(PANEL_URL_REPRINT);
    }
    lines.push(DISCLOSURE_REMINDER);
    lines
}

/// Whether the `thinking: endpoint=... wire=... data_collection=...` line is
/// printed. Quiet hides it only for a loopback endpoint; when Thinking leaves
/// the machine the line is privacy-relevant and always printed.
pub fn thinking_line_visible(diagnostics: bool, endpoint_is_loopback: bool) -> bool {
    diagnostics || !endpoint_is_loopback
}

/// Periodic or on-change `*_metrics:` lines (speech floor, conversation,
/// barge-in, memory writer, ...): hidden under quiet unless one of the line's
/// drop / error / overflow / expired counters is non-zero, like `panel: refused`.
/// A line with no such counters passes an empty slice and is hidden.
pub fn metrics_line_visible(diagnostics: bool, trouble_counters: &[u64]) -> bool {
    diagnostics || trouble_counters.iter().any(|&count| count != 0)
}

/// `speech_conversation: final ...`: quiet hides only the normal accepted
/// outcome (admission Queued); a drop or a route error stays printed.
pub fn final_line_visible(diagnostics: bool, accepted_normally: bool) -> bool {
    diagnostics || !accepted_normally
}

/// The warm-up line under the current verbosity: the original line when
/// verbose; in quiet the bilingual note for `started` and nothing for `ok`
/// (a failure or skip is always printed unchanged).
pub fn warmup_line(line: &str, diagnostics: bool) -> Option<&str> {
    if diagnostics {
        Some(line)
    } else if line.starts_with("thinking_warmup: started") {
        Some(WARMUP_NOTE)
    } else if line.starts_with("thinking_warmup: ok") {
        None
    } else {
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [&str; 4] = [
        PANEL_HINT,
        PANEL_URL_REPRINT,
        DISCLOSURE_REMINDER,
        WARMUP_NOTE,
    ];
    const VOICE: [&str; 8] = [
        VOICE_OFF,
        VOICE_OFF_CLOUD,
        VOICE_ON,
        VOICE_ON_CLOUD,
        VOICE_OFF_VERBOSE,
        VOICE_OFF_CLOUD_VERBOSE,
        VOICE_ON_VERBOSE,
        VOICE_ON_CLOUD_VERBOSE,
    ];

    #[test]
    fn the_app_side_guide_fits_its_share_of_the_eight_line_budget() {
        // Setup owns three more (see its own test). The voice-input line is
        // the fourth app line: at most one of the eight forms is printed. The
        // character sheet line (#404) is the fifth, so the total quiet
        // start-up is eight lines, one more than before the character sheet.
        assert!(ALL.len() < 5);
        // The panel hint, the panel URL reprint (#388) and the disclosure.
        assert!(ready_lines(true).len() <= 3);
    }

    #[test]
    fn persona_lines_are_single_line_text_free_and_say_what_they_must() {
        use crate::persona::PersonaSheet;
        use aivtuber_domain::parse_persona;
        let sheet = PersonaSheet::new(
            parse_persona("名前: SECRET-NAME".as_bytes())
                .unwrap()
                .unwrap(),
        );
        let loaded = PersonaLoad::Loaded(sheet.clone());
        let mut lines = Vec::new();
        for cloud in [false, true] {
            for diagnostics in [false, true] {
                lines.push((
                    cloud,
                    diagnostics,
                    persona_line(&loaded, cloud, diagnostics),
                ));
            }
        }
        for (cloud, diagnostics, line) in &lines {
            assert!(
                !line.contains('\n') && !line.contains("SECRET-NAME"),
                "{line}"
            );
            assert!(line.contains(&sheet.len().to_string()), "{line}");
            assert!(line.contains(sheet.sha256_prefix()), "{line}");
            assert_eq!(line.contains("cloud model"), *cloud, "{line}");
            assert_eq!(line.contains(" / "), !diagnostics, "{line}");
        }
        assert_eq!(
            persona_line(&loaded, false, true),
            format!(
                "persona: loaded bytes={} sha256={}",
                sheet.len(),
                sheet.sha256_prefix()
            )
        );
        assert!(
            persona_line(&loaded, true, false).starts_with("キャラクター設定を読み込みました(")
        );
        assert!(persona_line(&loaded, true, false).contains("クラウドの応答モデルへ送信されます"));
        // The other outcomes name a fixed reason, never content.
        let mut others = vec![PersonaLoad::Absent];
        for refusal in [
            PersonaRefusal::Unreadable,
            PersonaRefusal::Invalid(PersonaError::NotUtf8),
            PersonaRefusal::Invalid(PersonaError::ControlCharacter),
            PersonaRefusal::Invalid(PersonaError::TooLarge),
            PersonaRefusal::Invalid(PersonaError::ReservedTag),
        ] {
            others.push(PersonaLoad::Refused(refusal));
        }
        for load in others {
            for diagnostics in [false, true] {
                let line = persona_line(&load, true, diagnostics);
                assert!(!line.contains('\n') && !line.is_empty(), "{line}");
                assert!(!line.contains("http") && !line.contains("token="), "{line}");
                assert_eq!(line.contains(" / "), !diagnostics, "{line}");
            }
        }
        assert!(
            persona_line(
                &PersonaLoad::Refused(PersonaRefusal::Invalid(PersonaError::TooLarge)),
                false,
                true
            )
            .contains("larger than 3000 bytes")
        );
    }

    #[test]
    fn voice_lines_are_single_line_secret_free_and_say_what_they_must() {
        for line in VOICE {
            assert!(!line.trim().is_empty() && !line.contains('\n'), "{line}");
            assert!(!line.contains("token") && !line.contains("http"), "{line}");
        }
        for (on, cloud, diagnostics) in [
            (false, false, false),
            (false, true, false),
            (true, false, false),
            (true, true, false),
            (false, false, true),
            (false, true, true),
            (true, false, true),
            (true, true, true),
        ] {
            let line = voice_input_line(on, cloud, diagnostics);
            assert!(VOICE.contains(&line));
            // Cloud wording appears exactly when Thinking is remote.
            let says_cloud = line.contains("cloud model");
            assert_eq!(says_cloud, cloud, "{line}");
            // Only the on-forms claim to listen; the off-forms name the way on.
            assert_eq!(line.contains("listening"), on, "{line}");
            if !on {
                assert!(line.contains("--voice-input on"), "{line}");
            }
            // Quiet forms are Japanese first with the English half beside it.
            assert_eq!(line.contains(" / "), !diagnostics, "{line}");
        }
        assert!(VOICE_OFF_CLOUD.starts_with("マイクはオフです。"));
        assert!(VOICE_ON_CLOUD.contains("クラウドの応答モデルに送信されます"));
    }

    #[test]
    fn no_line_is_empty_multiline_or_carries_a_secret() {
        for line in ALL {
            assert!(!line.trim().is_empty());
            assert!(!line.contains('\n'));
            assert!(!line.contains("token="), "{line}");
            assert!(!line.contains("http"), "{line}");
        }
    }

    #[test]
    fn a_disabled_panel_claims_no_panel_url() {
        let lines = ready_lines(false);
        assert_eq!(lines, vec![DISCLOSURE_REMINDER]);
        assert!(ready_lines(true).contains(&PANEL_HINT));
        assert!(ready_lines(true).contains(&PANEL_URL_REPRINT));
        assert!(PANEL_URL_REPRINT.contains("aivtuberctl panel-url"));
    }

    #[test]
    fn the_thinking_line_is_hidden_only_for_loopback_under_quiet() {
        assert!(!thinking_line_visible(false, true));
        assert!(thinking_line_visible(false, false));
        assert!(thinking_line_visible(true, true));
        assert!(thinking_line_visible(true, false));
    }

    #[test]
    fn floor_and_conversation_metrics_print_under_quiet_only_with_trouble() {
        // speech_floor_metrics: [dropped_expired, dropped_overflow]
        assert!(!metrics_line_visible(false, &[0, 0]));
        assert!(metrics_line_visible(false, &[0, 2]));
        // speech_conversation_metrics: [oversize, blank, runtime, route_errors]
        assert!(!metrics_line_visible(false, &[0, 0, 0, 0]));
        assert!(metrics_line_visible(false, &[0, 0, 1, 0]));
        // No trouble counters at all (memory_falsifier_metrics): hidden.
        assert!(!metrics_line_visible(false, &[]));
        // Verbose always prints.
        assert!(metrics_line_visible(true, &[0, 0]));
        assert!(metrics_line_visible(true, &[]));
    }

    #[test]
    fn a_normal_accepted_final_is_hidden_under_quiet_only() {
        assert!(!final_line_visible(false, true));
        assert!(final_line_visible(false, false));
        assert!(final_line_visible(true, true));
    }

    #[test]
    fn warmup_lines_follow_the_verbosity() {
        let started = "thinking_warmup: started timeout_ms=120000";
        let ok = "thinking_warmup: ok ms=437 prompt_tokens=238";
        let failed = "thinking_warmup: failed reason=unavailable";
        assert_eq!(warmup_line(started, true), Some(started));
        assert_eq!(warmup_line(ok, true), Some(ok));
        assert_eq!(warmup_line(started, false), Some(WARMUP_NOTE));
        assert_eq!(warmup_line(ok, false), None);
        assert_eq!(warmup_line(failed, false), Some(failed));
    }
}
