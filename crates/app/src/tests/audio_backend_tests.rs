//! Issue #354: Audio backend selection tests.

use crate::{AudioBackendKind, build_audio_output};
use std::fs;

#[test]
fn audio_backend_kind_parses_valid_options() {
    assert_eq!(
        AudioBackendKind::parse("process").unwrap(),
        AudioBackendKind::Process
    );
    assert_eq!(
        AudioBackendKind::parse("rodio-gapless").unwrap(),
        AudioBackendKind::RodioGapless
    );
    assert_eq!(
        AudioBackendKind::parse("rodio").unwrap(),
        AudioBackendKind::RodioGapless
    );
}

#[test]
fn audio_backend_kind_rejects_unknown_options() {
    let err = AudioBackendKind::parse("unknown").unwrap_err();
    assert!(
        err.contains("invalid AIVTUBER_AUDIO_BACKEND"),
        "error must mention the invalid variable: {err}"
    );
    assert!(
        err.contains("valid options are 'process', 'rodio-gapless'"),
        "error must list valid options: {err}"
    );
}

#[test]
fn build_process_audio_backend() {
    let root =
        std::env::temp_dir().join(format!("audio-backend-process-test-{}", std::process::id()));
    fs::create_dir_all(&root).expect("root");
    let selection = build_audio_output(AudioBackendKind::Process, &root, None).expect("process");
    assert_eq!(selection.active, AudioBackendKind::Process);
    assert!(selection.fallback_reason.is_none());
    assert!(!selection.output.gapless_queue());
}

#[cfg(feature = "native-audio")]
#[test]
fn rodio_request_uses_native_or_degrades_with_a_reason() {
    let root =
        std::env::temp_dir().join(format!("audio-backend-rodio-test-{}", std::process::id()));
    fs::create_dir_all(&root).expect("root");
    let selection =
        build_audio_output(AudioBackendKind::RodioGapless, &root, None).expect("never fails");
    match selection.active {
        AudioBackendKind::RodioGapless => {
            assert!(selection.fallback_reason.is_none());
            assert!(selection.output.gapless_queue());
        }
        AudioBackendKind::Process => {
            let reason = selection.fallback_reason.expect("reason");
            assert!(!reason.contains('\n'));
            assert!(!selection.output.gapless_queue());
        }
    }
}

#[cfg(not(feature = "native-audio"))]
#[test]
fn rodio_request_without_feature_degrades_to_process() {
    let root =
        std::env::temp_dir().join(format!("audio-backend-nofeat-test-{}", std::process::id()));
    fs::create_dir_all(&root).expect("root");
    let selection = build_audio_output(AudioBackendKind::RodioGapless, &root, None).expect("ok");
    assert_eq!(selection.active, AudioBackendKind::Process);
    assert!(
        selection
            .fallback_reason
            .expect("reason")
            .contains("native-audio")
    );
    assert!(!selection.output.gapless_queue());
}
