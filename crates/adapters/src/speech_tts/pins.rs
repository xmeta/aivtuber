//! SHA-256 pins for the Supertonic 3 int8 TTS model (issue #220, S2b).
//!
//! Same pattern and the same types as the STT pins
//! ([`crate::speech_stt::pins`]): the sidecar refuses to run an unpinned model,
//! because a synthesizer that silently changes is a different voice and its
//! audio is cached by content address. The engine libraries are the sherpa-onnx
//! ones already pinned for STT ([`crate::speech_stt::pins::ENGINE_LIBS`]); there
//! is one runtime pin for both. Files are fetched by an operator step
//! (`scripts/fetch-sherpa-onnx.mjs model supertonic-3-int8 <dir>`) and are never
//! redistributed or committed.

use crate::speech_stt::pins::{ModelPin, PinnedFile};

/// One-line notice the sidecar and the app print at startup (decision Q1 of the
/// S2a decisions on #220). The voice weights are OpenRAIL-M, whose use
/// restrictions include publishing AI-generated content without clearly
/// disclosing that it is machine-generated; docs/supply-chain.adoc passes the
/// restrictions on.
pub const VOICE_LICENCE_NOTICE: &str = "voice model Supertonic 3 (Supertone Inc.) is licensed under OpenRAIL-M: its use restrictions apply, including the duty to disclose that published audio is machine-generated; keep an \"AI-generated voice\" disclosure on stream (docs/supply-chain.adoc)";

/// `model_alias` recorded in TTS provenance.
pub const SUPERTONIC_MODEL_ALIAS: &str = "supertonic-3-int8";

/// Supertonic 3 int8, the sherpa-onnx export in
/// `sherpa-onnx-supertonic-3-tts-int8-2026-05-11.tar.bz2` (archive SHA-256
/// `82fa96f91c4ef8abaae3a14a3f4153facf88bed821d1f7331cec2700f432c427`; a
/// third-party conversion of `Supertone/supertonic-3` revision
/// `3cadd1ee6394adea1bd021217a0e650ede09a323`). Weights OpenRAIL-M, sample code
/// MIT.
pub const SUPERTONIC_3_INT8: ModelPin = ModelPin {
    model_id: SUPERTONIC_MODEL_ALIAS,
    files: &[
        PinnedFile {
            role: "duration_predictor",
            file_name: "duration_predictor.int8.onnx",
            size: 3_700_147,
            sha256: "c3eb91414d5ff8a7a239b7fe9e34e7e2bf8a8140d8375ffb14718b1c639325db",
        },
        PinnedFile {
            role: "text_encoder",
            file_name: "text_encoder.int8.onnx",
            size: 36_416_150,
            sha256: "c7befd5ea8c3119769e8a6c1486c4edc6a3bc8365c67621c881bbb774b9902ff",
        },
        PinnedFile {
            role: "vector_estimator",
            file_name: "vector_estimator.int8.onnx",
            size: 78_400_833,
            sha256: "20cd86fa5c6effedfda0e7cffe5b0569ca401c440a0c3a1d72bf39286c0db3fd",
        },
        PinnedFile {
            role: "vocoder",
            file_name: "vocoder.int8.onnx",
            size: 25_991_073,
            sha256: "e923d60f53f95eb1ce235f1dc33ec56d9c057823c96fa6f8acf98f32b0da6152",
        },
        PinnedFile {
            role: "tts_json",
            file_name: "tts.json",
            size: 8_253,
            sha256: "42078d3aef1cd43ab43021f3c54f47d2d75ceb4e75f627f118890128b06a0d09",
        },
        PinnedFile {
            role: "unicode_indexer",
            file_name: "unicode_indexer.bin",
            size: 262_144,
            sha256: "8402ca48e5189a8950138580b0fff64db6f072f24ac07cd54ba8b2fbb9883b30",
        },
        PinnedFile {
            role: "voice_style",
            file_name: "voice.bin",
            size: 517_168,
            sha256: "67d5209b0ee8ce6c74105ffbe12fe6a7628aea3b4ba2fcb308a4a67938a93ce8",
        },
    ],
};

/// Selectable model, by the name used on the command line.
pub fn model_by_name(name: &str) -> Option<ModelPin> {
    match name {
        "supertonic-3-int8" => Some(SUPERTONIC_3_INT8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech_stt::pins::{bundle_sha256, verify_model_dir};

    #[test]
    fn pins_are_well_formed() {
        for file in SUPERTONIC_3_INT8.files {
            assert_eq!(file.sha256.len(), 64, "{}", file.role);
            assert!(
                file.sha256
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            );
            assert!(file.size > 0);
        }
        assert_eq!(bundle_sha256(&SUPERTONIC_3_INT8).len(), 64);
    }

    /// The operator fetch script carries its own copy of the model pins; the
    /// two must not drift.
    #[test]
    fn the_fetch_script_pins_the_same_model_digests() {
        let script = include_str!("../../../../scripts/fetch-sherpa-onnx.mjs");
        for file in SUPERTONIC_3_INT8.files {
            assert!(
                script.contains(file.sha256) && script.contains(file.file_name),
                "scripts/fetch-sherpa-onnx.mjs does not pin {} ({})",
                file.file_name,
                file.sha256
            );
        }
    }

    #[test]
    fn models_are_selected_by_name_only() {
        assert_eq!(model_by_name("supertonic-3-int8"), Some(SUPERTONIC_3_INT8));
        assert_eq!(model_by_name("../supertonic-3-int8"), None);
        assert_eq!(model_by_name(""), None);
    }

    #[test]
    fn an_empty_model_directory_is_refused() {
        let dir = std::env::temp_dir().join(format!("aivtuber-tts-pins-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(verify_model_dir(&SUPERTONIC_3_INT8, &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_licence_notice_names_the_licence_and_the_disclosure_duty() {
        assert!(VOICE_LICENCE_NOTICE.contains("OpenRAIL-M"));
        assert!(VOICE_LICENCE_NOTICE.contains("machine-generated"));
        assert!(!VOICE_LICENCE_NOTICE.contains('\n'));
    }
}
