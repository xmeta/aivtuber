//! Link arguments for the optional `sherpa-stt` worker (issue #92 S3b) and the
//! `sherpa-tts` sidecar (#220 S2b).
//!
//! `speech-stt-worker` links the sherpa-onnx engine libraries as shared
//! libraries, so the OS loader resolves them before `main`. The worker then
//! verifies SHA-256 pins on the libraries *in its own directory*
//! (`speech_stt::pins::verify_engine_libs`), which only means something if the
//! loader loads exactly those files. This makes it so on Linux:
//!
//! * `-rpath $ORIGIN` searches the directory of the executable;
//! * `--disable-new-dtags` makes it a DT_RPATH, which the loader consults
//!   *before* `LD_LIBRARY_PATH` (a DT_RUNPATH would come after it), so the
//!   environment cannot substitute other libraries.
//!
//! The sherpa-onnx libraries carry their own `$ORIGIN` rpath, so
//! `libonnxruntime.so` is found beside `libsherpa-onnx-c-api.so`. On Windows
//! the loader searches the executable's directory first. A
//! `cargo:rustc-link-arg` from a dependency's build script does not reach this
//! crate's binaries, hence this script. Scoped to the one binary.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SHERPA_STT");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SHERPA_TTS");
    let linux = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "linux");
    if linux {
        // The same loader rule for the TTS sidecar (#220 S2b), which links the
        // same engine libraries and pins them beside its own executable.
        for (feature, bin) in [
            ("CARGO_FEATURE_SHERPA_STT", "speech-stt-worker"),
            ("CARGO_FEATURE_SHERPA_TTS", "local-tts-sidecar"),
        ] {
            if std::env::var_os(feature).is_some() {
                println!("cargo:rustc-link-arg-bin={bin}=-Wl,--disable-new-dtags,-rpath,$ORIGIN");
            }
        }
    }
}
