//! The capture-device report of the VAD sidecar (#396).
//!
//! Once, at start, the sidecar tells the app which capture devices exist and
//! which one it opened (`null` when none could be opened, for instance an
//! unknown `--input-device`). Like the feed heartbeat it is a tiny bounded
//! status line, not part of the normative `speech-observation` batch: it
//! carries no observation, no time claim and no authority, is routed by its
//! top-level key (`vad`, second key `devices`) and is ignored when malformed.
//!
//! Device names are operator-local data. They are shown in the operation
//! panel and kept in the settings file; they are never part of any model
//! input or of the support bundle. The report is bounded in count and in name
//! length, and names carry no control characters, so the line stays well
//! inside the sidecar line limit whatever the OS reports.

use aivtuber_domain::{MAX_SPEECH_DEVICE_NAME_BYTES, MAX_SPEECH_DEVICES};
use serde::{Deserialize, Serialize};

/// Longest report line the parser looks at. The worst case (32 names of 128
/// bytes, each a quote or backslash that JSON doubles) is about 8.5 KiB.
pub const DEVICE_REPORT_MAX_LINE_BYTES: usize = 12 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceReport {
    /// The device the sidecar opened; `None` when it opened none.
    pub opened: Option<String>,
    /// Capture device names, at most [`MAX_SPEECH_DEVICES`].
    pub available: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Devices {
    devices: DeviceReport,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Line {
    vad: Devices,
}

/// A device name the report may carry: control characters removed and cut at a
/// character boundary to the byte limit. `None` when nothing is left.
pub fn clean_device_name(name: &str) -> Option<String> {
    let mut cleaned = String::new();
    for c in name.chars().filter(|c| !c.is_control()) {
        if cleaned.len() + c.len_utf8() > MAX_SPEECH_DEVICE_NAME_BYTES {
            break;
        }
        cleaned.push(c);
    }
    let cleaned = cleaned.trim().to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// One report line with its trailing newline. Names are cleaned, de-duplicated
/// and capped at [`MAX_SPEECH_DEVICES`]; `opened` is cleaned the same way.
pub fn encode_device_report_line(opened: Option<&str>, available: &[String]) -> String {
    let mut names: Vec<String> = Vec::new();
    for name in available.iter().filter_map(|name| clean_device_name(name)) {
        if names.len() >= MAX_SPEECH_DEVICES {
            break;
        }
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let report = Line {
        vad: Devices {
            devices: DeviceReport {
                opened: opened.and_then(clean_device_name),
                available: names,
            },
        },
    };
    let mut line = serde_json::to_string(&report).expect("plain strings always serialize");
    line.push('\n');
    line
}

/// The report of a well-formed line, else `None`. Lines over the bound, more
/// names than the limit, and names that are empty, over-long or hold control
/// characters are all rejected whole.
pub fn parse_device_report_line(line: &[u8]) -> Option<DeviceReport> {
    if line.len() > DEVICE_REPORT_MAX_LINE_BYTES {
        return None;
    }
    let report = serde_json::from_slice::<Line>(line).ok()?.vad.devices;
    let valid = |name: &String| aivtuber_domain::is_valid_speech_device_name(name);
    (report.available.len() <= MAX_SPEECH_DEVICES
        && report.available.iter().all(valid)
        && report.opened.as_ref().is_none_or(valid))
    .then_some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_round_trips_with_and_without_an_opened_device() {
        let names = vec!["Mic (USB)".to_owned(), "Line (AG06)".to_owned()];
        let line = encode_device_report_line(Some("Mic (USB)"), &names);
        assert_eq!(
            parse_device_report_line(line.as_bytes()),
            Some(DeviceReport {
                opened: Some("Mic (USB)".to_owned()),
                available: names.clone(),
            })
        );
        let line = encode_device_report_line(None, &names);
        let parsed = parse_device_report_line(line.as_bytes()).expect("parses");
        assert_eq!(parsed.opened, None);
        assert_eq!(parsed.available, names);
    }

    #[test]
    fn the_encoder_bounds_count_length_and_characters() {
        let many: Vec<String> = (0..100).map(|i| format!("device {i}")).collect();
        let parsed = parse_device_report_line(encode_device_report_line(None, &many).as_bytes())
            .expect("parses");
        assert_eq!(parsed.available.len(), MAX_SPEECH_DEVICES);
        let long = "x".repeat(500);
        let odd = vec![
            long,
            "a\u{0}b\n".to_owned(),
            "dup".to_owned(),
            "dup".to_owned(),
        ];
        let parsed = parse_device_report_line(encode_device_report_line(None, &odd).as_bytes())
            .expect("parses");
        assert_eq!(parsed.available[0].len(), MAX_SPEECH_DEVICE_NAME_BYTES);
        assert_eq!(parsed.available[1], "ab");
        assert_eq!(parsed.available[2], "dup");
        assert_eq!(parsed.available.len(), 3);
    }

    #[test]
    fn the_worst_case_line_fits_the_sidecar_line_limit() {
        let worst: Vec<String> = (0..MAX_SPEECH_DEVICES)
            .map(|i| format!("{}{i:02}", "\"".repeat(MAX_SPEECH_DEVICE_NAME_BYTES - 2)))
            .collect();
        let line = encode_device_report_line(Some(&worst[0]), &worst);
        assert!(line.len() <= DEVICE_REPORT_MAX_LINE_BYTES, "{}", line.len());
        assert!(line.len() < 16 * 1024);
        let parsed = parse_device_report_line(line.as_bytes()).expect("parses");
        assert_eq!(parsed.available, worst);
    }

    #[test]
    fn malformed_or_out_of_bounds_lines_are_rejected() {
        let too_many = format!(
            "{{\"vad\":{{\"devices\":{{\"opened\":null,\"available\":[{}]}}}}}}",
            (0..=MAX_SPEECH_DEVICES)
                .map(|i| format!("\"d{i}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        let too_long = format!(
            "{{\"vad\":{{\"devices\":{{\"opened\":null,\"available\":[\"{}\"]}}}}}}",
            "x".repeat(MAX_SPEECH_DEVICE_NAME_BYTES + 1)
        );
        let cases: [&[u8]; 9] = [
            b"",
            b"{}",
            b"{\"vad\":{\"devices\":{}}}",
            b"{\"vad\":{\"devices\":{\"opened\":null}}}",
            b"{\"vad\":{\"devices\":{\"opened\":null,\"available\":[\"\"]}}}",
            b"{\"vad\":{\"devices\":{\"opened\":null,\"available\":[\"a\\nb\"]}}}",
            b"{\"vad\":{\"devices\":{\"opened\":null,\"available\":[],\"x\":1}}}",
            too_many.as_bytes(),
            too_long.as_bytes(),
        ];
        for case in cases {
            assert_eq!(
                parse_device_report_line(case),
                None,
                "{:?}",
                String::from_utf8_lossy(case)
            );
        }
        // A heartbeat is not a device report.
        assert_eq!(
            parse_device_report_line(b"{\"vad\":{\"heartbeat\":{\"seq\":1}}}"),
            None
        );
    }
}
