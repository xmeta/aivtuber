//! App-local Microsoft Visual C++ runtime for the pinned llama.cpp build.
//!
//! The upstream Windows zip imports `VCRUNTIME140.dll`, `VCRUNTIME140_1.dll`
//! and `MSVCP140.dll` but does not ship them, and a clean Windows has none
//! (`llama-server` then exits with 0xC0000135). The release archive carries
//! those DLLs in `runtime/msvc` (Microsoft's app-local deployment of the
//! redistributable files, docs/supply-chain.adoc section 11); this module
//! copies them beside the llama.cpp executables. No installer, no admin
//! rights, no download.

use std::fs;
use std::path::{Path, PathBuf};

/// The bundled runtime directory, relative to the directory of the programs.
pub fn runtime_dir(bin: &Path) -> PathBuf {
    bin.join("runtime").join("msvc")
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Copied now.
    pub placed: Vec<String>,
    /// Already there and byte-identical.
    pub identical: Vec<String>,
    /// Already there but different: left alone, never overwritten.
    pub kept_different: Vec<String>,
    /// The archive has no `runtime/msvc` at all (a cargo checkout).
    pub no_runtime_dir: bool,
}

/// Copies `names` from `runtime` into `dest` (the directory of `llama-server`).
/// A missing file in an existing runtime directory is an error naming it.
pub fn place(runtime: &Path, dest: &Path, names: &[String]) -> Result<Report, String> {
    let mut report = Report::default();
    if !runtime.is_dir() {
        report.no_runtime_dir = true;
        return Ok(report);
    }
    for name in names {
        if name.contains(['/', '\\']) || name.is_empty() {
            return Err(format!("invalid runtime file name {name:?}"));
        }
        let src = runtime.join(name);
        if !src.is_file() {
            return Err(format!(
                "the release archive is missing {} (expected in {}); download the archive again",
                name,
                runtime.display()
            ));
        }
        let target = dest.join(name);
        if target.exists() {
            let same = fs::read(&src).map_err(|e| format!("{}: {e}", src.display()))?
                == fs::read(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            if same {
                report.identical.push(name.clone());
            } else {
                report.kept_different.push(name.clone());
            }
            continue;
        }
        // Copy beside, then rename: a crash never leaves a half-written DLL
        // under its real name.
        let part = dest.join(format!("{name}.part"));
        fs::copy(&src, &part)
            .and_then(|_| fs::rename(&part, &target))
            .map_err(|e| format!("cannot place {} at {}: {e}", name, target.display()))?;
        report.placed.push(name.clone());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aivtuber-vcrt-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names() -> Vec<String> {
        vec!["a140.dll".to_owned(), "b140.dll".to_owned()]
    }

    fn bundle(root: &Path) -> PathBuf {
        let rt = runtime_dir(root);
        fs::create_dir_all(&rt).unwrap();
        fs::write(rt.join("a140.dll"), b"A").unwrap();
        fs::write(rt.join("b140.dll"), b"B").unwrap();
        rt
    }

    #[test]
    fn copies_the_bundled_files_and_is_idempotent() {
        let root = scratch("copy");
        let rt = bundle(&root);
        let dest = root.join("bin");
        fs::create_dir_all(&dest).unwrap();
        let first = place(&rt, &dest, &names()).unwrap();
        assert_eq!(first.placed, names());
        assert_eq!(fs::read(dest.join("b140.dll")).unwrap(), b"B");
        assert!(!dest.join("a140.dll.part").exists());
        let second = place(&rt, &dest, &names()).unwrap();
        assert!(second.placed.is_empty());
        assert_eq!(second.identical, names());
    }

    #[test]
    fn never_overwrites_a_different_file() {
        let root = scratch("keep");
        let rt = bundle(&root);
        let dest = root.join("bin");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("a140.dll"), b"other").unwrap();
        let r = place(&rt, &dest, &names()).unwrap();
        assert_eq!(r.kept_different, vec!["a140.dll".to_owned()]);
        assert_eq!(r.placed, vec!["b140.dll".to_owned()]);
        assert_eq!(fs::read(dest.join("a140.dll")).unwrap(), b"other");
    }

    #[test]
    fn a_missing_file_in_the_archive_is_named() {
        let root = scratch("missing");
        let rt = bundle(&root);
        fs::remove_file(rt.join("b140.dll")).unwrap();
        let dest = root.join("bin");
        fs::create_dir_all(&dest).unwrap();
        let err = place(&rt, &dest, &names()).unwrap_err();
        assert!(err.contains("b140.dll"), "{err}");
    }

    #[test]
    fn a_checkout_without_the_runtime_dir_only_reports_it() {
        let root = scratch("dev");
        let dest = root.join("bin");
        fs::create_dir_all(&dest).unwrap();
        let r = place(&runtime_dir(&root), &dest, &names()).unwrap();
        assert!(r.no_runtime_dir && r.placed.is_empty());
    }

    #[test]
    fn rejects_path_like_names() {
        let root = scratch("names");
        let rt = bundle(&root);
        assert!(place(&rt, &root, &["../x.dll".to_owned()]).is_err());
    }
}
