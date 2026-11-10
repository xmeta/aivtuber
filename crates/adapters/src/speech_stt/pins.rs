//! SHA-256 pins for the STT model files and engine libraries (issue #92, S3b).
//!
//! The worker and the sidecar refuse to run an unpinned model or engine
//! library, the same pattern as the Silero model pin in S2: a recognizer that
//! silently changes is a different recognizer, and its text feeds the
//! conversation. The pins are compiled in; the files are fetched by an
//! operator step (`aivtuber-setup local`, `scripts/fetch-sherpa-onnx.mjs`, `docs/supply-chain.adoc`)
//! and are never redistributed or committed.
//!
//! Library pins have a limit worth stating: the engine libraries are linked
//! as shared libraries, so the OS loader maps them before `main` runs and the
//! worker's own check can only catch a wrong or corrupt library before it is
//! *used*. The sidecar therefore checks the libraries beside the worker
//! binary *before spawning it* ([`verify_engine_libs`]), which is where the
//! check can still prevent a load.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedFile {
    /// Role in the bundle digest (`encoder`, `tokens`, ...).
    pub role: &'static str,
    /// Exact file name expected inside the directory.
    pub file_name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPin {
    /// Recorded in every transcript as `engine.model_id`.
    pub model_id: &'static str,
    pub files: &'static [PinnedFile],
}

/// ReazonSpeech k2-v2 int8 (default): Hugging Face
/// `reazon-research/reazonspeech-k2-v2` revision
/// `291488c8151be24d7da4bf7af26e533fad96e407` (Apache-2.0 weights; corpus
/// terms in docs/supply-chain.adoc).
pub const K2V2_INT8: ModelPin = ModelPin {
    model_id: "reazonspeech-k2-v2-int8",
    files: &[
        PinnedFile {
            role: "encoder",
            file_name: "encoder-epoch-99-avg-1.int8.onnx",
            size: 154_670_139,
            sha256: "2c7bd08a8a99f9ddd0d9e458456577b1f6279214e51426f114f9eced44c54e1d",
        },
        PinnedFile {
            role: "decoder",
            file_name: "decoder-epoch-99-avg-1.int8.onnx",
            size: 2_959_337,
            sha256: "8f0bff94d38797b03b762634ed03211a8e303d06cc4603cdd0cf4199d6eb1485",
        },
        PinnedFile {
            role: "joiner",
            file_name: "joiner-epoch-99-avg-1.int8.onnx",
            size: 2_696_970,
            sha256: "49cc7ea1d3d35a40a27442db5e89996da64bf0e683a903dce76e99e57a12e4de",
        },
        PinnedFile {
            role: "tokens",
            file_name: "tokens.txt",
            size: 45_754,
            sha256: "2c3ac659818a48a0c04010e0593bbc4d7c8a24a054340b01131499c05fd52def",
        },
    ],
};

/// NVIDIA parakeet-tdt_ctc-0.6b-ja, the sherpa-onnx int8 CTC export in
/// `sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8.tar.bz2` (archive
/// sha256 `4b0a800e...`). CC-BY-4.0: attribution required when selected.
pub const PARAKEET_JA_INT8: ModelPin = ModelPin {
    model_id: "parakeet-tdt-ctc-0.6b-ja-int8",
    files: &[
        PinnedFile {
            role: "model",
            file_name: "model.int8.onnx",
            size: 655_542_604,
            sha256: "3addd00ef5bd1742078389e540b77394e4a508bdf2f4c9ad1b4a76d93e76598e",
        },
        PinnedFile {
            role: "tokens",
            file_name: "tokens.txt",
            size: 28_557,
            sha256: "732f64c53909f2620c713f4106b487d92e6f54a6915b3cd3d1dbd32f9f4f392a",
        },
    ],
};

/// Engine libraries of sherpa-onnx v2.0.1 that the worker loads, per target.
/// Taken from `sherpa-onnx-v2.0.1-win-x64-shared-MT-Release-lib.tar.bz2`
/// (sha256 `afa4de3c...`) and `sherpa-onnx-v2.0.1-linux-x64-shared-lib.tar.bz2`
/// (sha256 `532c45bb...`). Other targets are unpinned and refused.
#[cfg(all(windows, target_arch = "x86_64"))]
pub const ENGINE_LIBS: &[PinnedFile] = &[
    PinnedFile {
        role: "sherpa-onnx-c-api",
        file_name: "sherpa-onnx-c-api.dll",
        size: 4_294_656,
        sha256: "0965a6e4c0254da2beff489cf7a8aaffaf7dc94f90b43b3e66448b8cab4177ea",
    },
    PinnedFile {
        role: "onnxruntime",
        file_name: "onnxruntime.dll",
        size: 17_799_168,
        sha256: "7f66f939a881baf4f46a2216496798edf4a1429878b646d12674aa62f27d8a25",
    },
    PinnedFile {
        role: "onnxruntime-providers-shared",
        file_name: "onnxruntime_providers_shared.dll",
        size: 104_960,
        sha256: "551d0e1fe4c227d8542314ba718d52f4379e0c7bfe729a37c59833a884e27b4d",
    },
];
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const ENGINE_LIBS: &[PinnedFile] = &[
    PinnedFile {
        role: "sherpa-onnx-c-api",
        file_name: "libsherpa-onnx-c-api.so",
        size: 4_888_200,
        sha256: "96bf5232fb2d6c9600c69430d6d029fd722e0924c01885685815e2fbcf0af196",
    },
    PinnedFile {
        role: "onnxruntime",
        file_name: "libonnxruntime.so",
        size: 27_026_609,
        sha256: "4b3607aebd1784b26b6f9b20e4bd974c7ab8287043e4d095cb7d2cb40b5e566e",
    },
];
#[cfg(not(any(
    all(windows, target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64")
)))]
pub const ENGINE_LIBS: &[PinnedFile] = &[];

/// Selectable model, by the name used on the command line.
pub fn model_by_name(name: &str) -> Option<ModelPin> {
    match name {
        "k2v2-int8" => Some(K2V2_INT8),
        "parakeet-ja" => Some(PARAKEET_JA_INT8),
        _ => None,
    }
}

/// Digest recorded in transcripts as `engine.sha256`: SHA-256 over
/// `"<role> <sha256>\n"` for each pinned file in table order. It identifies the
/// exact model files without a per-file list on every line.
pub fn bundle_sha256(pin: &ModelPin) -> String {
    let mut hasher = Sha256::new();
    for file in pin.files {
        hasher.update(file.role.as_bytes());
        hasher.update(b" ");
        hasher.update(file.sha256.as_bytes());
        hasher.update(b"\n");
    }
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Streaming SHA-256 of a file, refusing a length that differs from the pin
/// before reading it.
fn verify_file(dir: &Path, pinned: &PinnedFile) -> Result<(), String> {
    let path = dir.join(pinned.file_name);
    let mut file = File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let length = file
        .metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if length != pinned.size {
        return Err(format!(
            "{} is {length} bytes; the pinned {} is {} bytes",
            path.display(),
            pinned.role,
            pinned.size
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    let actual = hex(&hasher.finalize());
    if actual == pinned.sha256 {
        Ok(())
    } else {
        Err(format!(
            "{} sha256 {actual} does not match the pinned {} digest {}",
            path.display(),
            pinned.role,
            pinned.sha256
        ))
    }
}

/// Every file of `pin` must exist in `dir` with the pinned size and SHA-256.
pub fn verify_model_dir(pin: &ModelPin, dir: &Path) -> Result<(), String> {
    pin.files.iter().try_for_each(|file| verify_file(dir, file))
}

/// Every pinned engine library must sit in `dir` unmodified. An unpinned
/// target (no table) is refused rather than run unchecked.
pub fn verify_engine_libs(dir: &Path) -> Result<(), String> {
    if ENGINE_LIBS.is_empty() {
        return Err(
            "no engine library pins for this platform (pinned: Windows x64, Linux x64)".to_owned(),
        );
    }
    ENGINE_LIBS
        .iter()
        .try_for_each(|file| verify_file(dir, file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn pins_are_well_formed_and_the_bundle_digest_is_stable() {
        for pin in [K2V2_INT8, PARAKEET_JA_INT8] {
            for file in pin.files.iter().chain(ENGINE_LIBS) {
                assert_eq!(file.sha256.len(), 64, "{}", file.role);
                assert!(
                    file.sha256
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                );
                assert!(file.size > 0);
            }
            let digest = bundle_sha256(&pin);
            assert_eq!(digest.len(), 64);
            assert_eq!(digest, bundle_sha256(&pin));
        }
        assert_ne!(bundle_sha256(&K2V2_INT8), bundle_sha256(&PARAKEET_JA_INT8));
        // The identity recorded on the wire must satisfy the wire contract.
        assert!(
            aivtuber_domain::TranscriptEngine {
                model_id: K2V2_INT8.model_id.to_owned(),
                sha256: bundle_sha256(&K2V2_INT8),
            }
            .model_id
            .len()
                <= aivtuber_domain::MAX_MODEL_ID_LEN
        );
    }

    /// The shared download manifest carries its own copy of the model pins; the
    /// two must not drift.
    #[test]
    fn the_download_manifest_pins_the_same_model_digests() {
        let manifest = include_str!("../../../../packaging/local-profile-pins.json");
        for pin in [K2V2_INT8, PARAKEET_JA_INT8] {
            for file in pin.files {
                assert!(
                    manifest.contains(file.sha256) && manifest.contains(file.file_name),
                    "packaging/local-profile-pins.json does not pin {} ({})",
                    file.file_name,
                    file.sha256
                );
            }
        }
    }

    #[test]
    fn models_are_selected_by_name_only() {
        assert_eq!(model_by_name("k2v2-int8"), Some(K2V2_INT8));
        assert_eq!(model_by_name("parakeet-ja"), Some(PARAKEET_JA_INT8));
        assert_eq!(model_by_name("../k2v2-int8"), None);
        assert_eq!(model_by_name(""), None);
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aivtuber-pins-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_resized_or_altered_file_is_refused() {
        let dir = scratch("verify");
        let content = b"hello pinned world";
        let digest = hex(&Sha256::digest(content));
        let sha: &'static str = Box::leak(digest.into_boxed_str());
        let pinned = PinnedFile {
            role: "tokens",
            file_name: "tokens.txt",
            size: content.len() as u64,
            sha256: sha,
        };
        // Missing.
        assert!(verify_file(&dir, &pinned).is_err());
        // Exact.
        std::fs::File::create(dir.join("tokens.txt"))
            .unwrap()
            .write_all(content)
            .unwrap();
        assert!(verify_file(&dir, &pinned).is_ok());
        // Same length, different bytes.
        std::fs::File::create(dir.join("tokens.txt"))
            .unwrap()
            .write_all(b"hello pinned_world")
            .unwrap();
        let error = verify_file(&dir, &pinned).unwrap_err();
        assert!(error.contains("does not match"), "{error}");
        // Different length is refused before hashing.
        std::fs::File::create(dir.join("tokens.txt"))
            .unwrap()
            .write_all(b"short")
            .unwrap();
        assert!(verify_file(&dir, &pinned).unwrap_err().contains("bytes"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_or_wrong_directory_fails_model_and_library_verification() {
        let dir = scratch("empty");
        assert!(verify_model_dir(&K2V2_INT8, &dir).is_err());
        assert!(verify_engine_libs(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
