//! Detection of the Microsoft Visual C++ 2015-2022 x64 runtime.
//!
//! The pinned upstream llama.cpp Windows zip imports `VCRUNTIME140.dll`,
//! `VCRUNTIME140_1.dll` and `MSVCP140.dll` and does not ship them; without
//! them `llama-server` exits with 0xC0000135. This project does not
//! redistribute those files (docs/supply-chain.adoc section 11, #349), so
//! setup checks that the machine has them and, if not, stops with the
//! official download page. It never downloads or runs an installer.
//!
//! The check needs no crate and no `unsafe`: the redistributable installs the
//! DLLs into `System32`, and a copy beside `llama-server.exe` also satisfies
//! the loader, so both places are looked at through an injectable probe.

use std::path::{Path, PathBuf};

pub const DOWNLOAD_PAGE: &str =
    "https://learn.microsoft.com/cpp/windows/latest-supported-vc-redist";

/// Where the loader finds the runtime on the real machine: the system
/// directory (always) and the directory of `llama-server` (app-local copies).
pub fn search_dirs(llama_dir: Option<&Path>) -> Vec<PathBuf> {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let mut dirs = vec![Path::new(&root).join("System32")];
    dirs.extend(llama_dir.map(Path::to_path_buf));
    dirs
}

/// The DLLs of `names` that no directory of `dirs` holds, according to `exists`.
pub fn missing(names: &[String], dirs: &[PathBuf], exists: &dyn Fn(&Path) -> bool) -> Vec<String> {
    names
        .iter()
        .filter(|n| !dirs.iter().any(|d| exists(&d.join(n))))
        .cloned()
        .collect()
}

/// A no-op off Windows; on `win32-x64` an error naming the dependency and the
/// official download page when any of `names` is missing.
pub fn require(
    platform: &str,
    names: &[String],
    dirs: &[PathBuf],
    exists: &dyn Fn(&Path) -> bool,
) -> Result<(), String> {
    if platform != "win32-x64" {
        return Ok(());
    }
    let gone = missing(names, dirs, exists);
    if gone.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the Microsoft Visual C++ 2015-2022 Redistributable (x64) is not installed (missing {}). \
         The language-model server (llama.cpp) needs it and this release does not bundle it. \
         Download and install it from Microsoft's page {DOWNLOAD_PAGE} (vc_redist.x64.exe), then run this command again",
        gone.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["vcruntime140.dll".into(), "msvcp140.dll".into()]
    }

    fn dirs() -> Vec<PathBuf> {
        vec![PathBuf::from("sys"), PathBuf::from("llama")]
    }

    #[test]
    fn present_in_the_system_directory_passes() {
        let probe = |p: &Path| p.starts_with("sys");
        assert!(require("win32-x64", &names(), &dirs(), &probe).is_ok());
    }

    #[test]
    fn an_app_local_copy_beside_llama_server_passes() {
        let probe = |p: &Path| p.starts_with("llama");
        assert!(require("win32-x64", &names(), &dirs(), &probe).is_ok());
    }

    #[test]
    fn a_partial_install_names_what_is_missing_and_the_official_page() {
        let probe = |p: &Path| p.ends_with("vcruntime140.dll");
        let err = require("win32-x64", &names(), &dirs(), &probe).unwrap_err();
        assert!(err.contains("(missing msvcp140.dll)"), "{err}");
        assert!(err.contains(DOWNLOAD_PAGE), "{err}");
        assert!(err.contains("Visual C++"), "{err}");
    }

    #[test]
    fn nothing_present_fails() {
        let err = require("win32-x64", &names(), &dirs(), &|_: &Path| false).unwrap_err();
        assert!(
            err.contains("vcruntime140.dll") && err.contains("msvcp140.dll"),
            "{err}"
        );
    }

    #[test]
    fn off_windows_is_a_no_op() {
        assert!(require("linux-x64", &names(), &dirs(), &|_: &Path| false).is_ok());
    }
}
