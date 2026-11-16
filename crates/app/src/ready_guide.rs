//! Short Japanese-first ready guide printed on stderr under `AIVTUBER_LOG=quiet` (#393).
//!
//! Fixed text, no locale detection, no catalogue. The whole quiet start-up adds
//! at most seven lines across the app and `aivtuber-setup start`: four here and
//! three in `crates/setup/src/start.rs`. The fourth app line is the voice-input
//! state (#397), printed only when `aivtuber-setup start` governs voice input;
//! a privacy-relevant fact (is the microphone open) must not be hidden by quiet. The duty text itself (AI-generated
//! voice disclosure, licences) is never translated; the English notices printed
//! earlier stay authoritative.

/// Printed after the panel URL line, only when the panel is running. Does not
/// repeat the URL or its secret.
pub const PANEL_HINT: &str = "準備ができました。上の「local operation panel URL」の行のURLをブラウザで開いてください(秘密を含むので共有しないでください)。 / Ready: open the URL on the \"local operation panel URL\" line above in a browser (it contains a secret; do not share it).";

/// One-line reminder; the English notice above is the authoritative text.
pub const DISCLOSURE_REMINDER: &str = "配信ではAI音声・AI出演者であることを表示してください(正文は上の英語の通知です)。 / Disclose the AI-generated voice and AI performer on stream (the English notice above is authoritative).";

/// Quiet-mode replacement for `thinking_warmup: started`, so the model warm-up
/// is not a silent wait.
pub const WARMUP_NOTE: &str = "モデルを準備しています… / warming up the model…";

/// Voice input (#397), quiet form: Japanese first, English beside it. One line
/// each; none carries a secret, a path or a URL. The `cloud` forms say that
/// speech text leaves the machine because the Thinking model is remote.
pub const VOICE_OFF: &str = "マイクはオフです。音声入力を使うには aivtuber-setup start --voice-input on で起動してください。 / Voice input is off; start with `aivtuber-setup start --voice-input on` to use the microphone.";
pub const VOICE_OFF_CLOUD: &str = "マイクはオフです。オンにすると、聞き取った音声の文字起こしはクラウドの応答モデルに送信されます(aivtuber-setup start --voice-input on)。 / Voice input is off; once on (`aivtuber-setup start --voice-input on`), transcribed speech is sent to the cloud model.";
pub const VOICE_ON: &str = "マイクで聞き取り中です(既定の録音デバイス)。止めるには aivtuber-setup start --voice-input off で起動してください。 / Voice input is on (listening on the default capture device); start with `aivtuber-setup start --voice-input off` to stop.";
pub const VOICE_ON_CLOUD: &str = "マイクで聞き取り中です(既定の録音デバイス)。聞き取った音声の文字起こしはクラウドの応答モデルに送信されます。 / Voice input is on (listening on the default capture device); transcribed speech is sent to the cloud model.";

/// Verbose (diagnostics) form of the same line, in English like the other
/// diagnostic lines.
pub const VOICE_OFF_VERBOSE: &str = "voice input: off (microphone not opened); turn it on with `aivtuber-setup start --voice-input on`";
pub const VOICE_OFF_CLOUD_VERBOSE: &str = "voice input: off (microphone not opened); turn it on with `aivtuber-setup start --voice-input on`; once on, transcribed speech is sent to the cloud model";
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

/// The lines printed once start-up reaches the panel URL / operator-control
/// point. Empty guidance for the panel is omitted when the panel is disabled.
pub fn ready_lines(panel_running: bool) -> Vec<&'static str> {
    let mut lines = Vec::with_capacity(2);
    if panel_running {
        lines.push(PANEL_HINT);
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

    const ALL: [&str; 3] = [PANEL_HINT, DISCLOSURE_REMINDER, WARMUP_NOTE];
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
    fn the_app_side_guide_fits_its_share_of_the_seven_line_budget() {
        // Setup owns three more (see its own test). The voice-input line is
        // the fourth app line: at most one of the eight forms is printed.
        assert!(ALL.len() < 4);
        assert!(ready_lines(true).len() <= 2);
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
