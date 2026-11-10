//! Archive extraction that cannot write outside its destination.
//!
//! Supports `.zip`, `.tar.gz` and `.tar.bz2` in pure Rust (no `tar`/`unzip`
//! on the target machine). The rules apply to every format:
//!
//! * an entry name must be relative and made of plain components: no `..`, no
//!   root or drive prefix, no `\`, no `:` (which also closes NTFS alternate
//!   data streams), no NUL;
//! * nothing is ever written through a symbolic link: if any existing ancestor
//!   of the target is a link, the entry is refused;
//! * a symbolic-link entry is allowed only on Unix, only with a relative
//!   target that stays inside the destination once `..` is resolved lexically
//!   (the Linux llama.cpp archive ships `libfoo.so -> libfoo.so.0` links);
//! * hard links, devices, FIFOs and any other entry type are refused;
//! * entry count and total expanded bytes are bounded (decompression bombs).

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: u64,
    pub max_entries: usize,
}

impl Limits {
    pub fn with_max_bytes(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            max_entries: 20_000,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ExtractError {
    UnsafePath(String),
    UnsafeLink { entry: String, target: String },
    UnsupportedEntry { entry: String, kind: String },
    TooLarge { limit: u64 },
    TooManyEntries { limit: usize },
    UnknownFormat(String),
    Io(String),
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath(p) => write!(f, "archive entry {p:?} would escape the destination"),
            Self::UnsafeLink { entry, target } => {
                write!(
                    f,
                    "archive link {entry:?} -> {target:?} leaves the destination"
                )
            }
            Self::UnsupportedEntry { entry, kind } => {
                write!(
                    f,
                    "archive entry {entry:?} is a {kind}, which is not extracted"
                )
            }
            Self::TooLarge { limit } => write!(f, "archive expands past {limit} bytes"),
            Self::TooManyEntries { limit } => write!(f, "archive has more than {limit} entries"),
            Self::UnknownFormat(name) => write!(f, "unknown archive type: {name}"),
            Self::Io(m) => write!(f, "extraction error: {m}"),
        }
    }
}

impl std::error::Error for ExtractError {}

fn io_err(e: impl fmt::Display) -> ExtractError {
    ExtractError::Io(e.to_string())
}

/// Validates an archive entry name and returns it as a relative path.
pub fn safe_relative(name: &str) -> Result<PathBuf, ExtractError> {
    let bad = || ExtractError::UnsafePath(name.to_owned());
    if name.is_empty() || name.contains(['\\', '\0']) || name.starts_with('/') {
        return Err(bad());
    }
    let mut out = PathBuf::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(bad()),
            p if p.contains(':') => return Err(bad()),
            p => out.push(p),
        }
    }
    if out.as_os_str().is_empty() || out.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(bad());
    }
    Ok(out)
}

/// Resolves `target` (relative to the directory holding `entry`) lexically and
/// says whether it stays inside the destination.
fn link_stays_inside(entry: &Path, target: &str) -> bool {
    if target.is_empty() || target.starts_with('/') || target.contains(['\\', '\0', ':']) {
        return false;
    }
    let mut depth = entry.components().count() as i64 - 1; // directories above the link
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

struct Sink<'a> {
    root: &'a Path,
    limits: Limits,
    bytes: u64,
    entries: usize,
}

impl<'a> Sink<'a> {
    fn new(root: &'a Path, limits: Limits) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        Ok(Self {
            root,
            limits,
            bytes: 0,
            entries: 0,
        })
    }

    fn count(&mut self) -> Result<(), ExtractError> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(ExtractError::TooManyEntries {
                limit: self.limits.max_entries,
            });
        }
        Ok(())
    }

    /// No existing ancestor of `rel` below the root may be a link.
    fn check_ancestors(&self, rel: &Path, name: &str) -> Result<(), ExtractError> {
        let mut cur = self.root.to_path_buf();
        if let Some(parent) = rel.parent() {
            for part in parent.components() {
                cur.push(part);
                match fs::symlink_metadata(&cur) {
                    Ok(m) if m.file_type().is_symlink() => {
                        return Err(ExtractError::UnsafePath(name.to_owned()));
                    }
                    Ok(m) if !m.is_dir() => return Err(ExtractError::UnsafePath(name.to_owned())),
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                    Err(e) => return Err(io_err(e)),
                }
            }
        }
        Ok(())
    }

    fn dir(&mut self, name: &str) -> Result<(), ExtractError> {
        self.count()?;
        let rel = safe_relative(name)?;
        self.check_ancestors(&rel, name)?;
        fs::create_dir_all(self.root.join(&rel)).map_err(io_err)
    }

    fn file(
        &mut self,
        name: &str,
        reader: &mut dyn Read,
        unix_mode: Option<u32>,
    ) -> Result<(), ExtractError> {
        self.count()?;
        let rel = safe_relative(name)?;
        self.check_ancestors(&rel, name)?;
        let target = self.root.join(&rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_err)?;
        }
        if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink() || m.is_dir()) {
            return Err(ExtractError::UnsafePath(name.to_owned()));
        }
        let mut out = File::create(&target).map_err(io_err)?;
        let remaining = self.limits.max_bytes.saturating_sub(self.bytes);
        let copied = io::copy(&mut reader.take(remaining + 1), &mut out).map_err(io_err)?;
        if copied > remaining {
            drop(out);
            let _ = fs::remove_file(&target);
            return Err(ExtractError::TooLarge {
                limit: self.limits.max_bytes,
            });
        }
        self.bytes += copied;
        set_mode(&target, unix_mode)?;
        Ok(())
    }

    fn symlink(&mut self, name: &str, link_target: &str) -> Result<(), ExtractError> {
        self.count()?;
        let rel = safe_relative(name)?;
        self.check_ancestors(&rel, name)?;
        if !link_stays_inside(&rel, link_target) {
            return Err(ExtractError::UnsafeLink {
                entry: name.to_owned(),
                target: link_target.to_owned(),
            });
        }
        #[cfg(unix)]
        {
            let path = self.root.join(&rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(io_err)?;
            }
            std::os::unix::fs::symlink(link_target, &path).map_err(io_err)
        }
        #[cfg(not(unix))]
        Err(ExtractError::UnsupportedEntry {
            entry: name.to_owned(),
            kind: "symbolic link (not supported on this platform)".to_owned(),
        })
    }
}

/// Keeps the owner/group/other read and execute bits of a regular file (so an
/// extracted `llama-server` stays runnable); write bits are never widened.
#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> Result<(), ExtractError> {
    use std::os::unix::fs::PermissionsExt;
    match mode {
        Some(mode) => {
            fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o755)).map_err(io_err)
        }
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Option<u32>) -> Result<(), ExtractError> {
    Ok(())
}

/// Extracts `archive` (chosen by file extension) into `dest`.
pub fn extract(archive: &Path, dest: &Path, limits: Limits) -> Result<(), ExtractError> {
    let name = archive
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let file = File::open(archive).map_err(io_err)?;
    if name.ends_with(".zip") {
        extract_zip(file, dest, limits)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        extract_tar(flate2::read::GzDecoder::new(file), dest, limits)
    } else if name.ends_with(".tar.bz2") {
        extract_tar(bzip2::read::BzDecoder::new(file), dest, limits)
    } else {
        Err(ExtractError::UnknownFormat(name))
    }
}

pub fn extract_tar<R: Read>(reader: R, dest: &Path, limits: Limits) -> Result<(), ExtractError> {
    let mut sink = Sink::new(dest, limits).map_err(io_err)?;
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(io_err)? {
        let mut entry = entry.map_err(io_err)?;
        let name = String::from_utf8(entry.path_bytes().into_owned())
            .map_err(|_| ExtractError::UnsafePath("<non-UTF-8 name>".to_owned()))?;
        let kind = entry.header().entry_type();
        let mode = entry.header().mode().ok();
        if kind.is_dir() {
            sink.dir(name.trim_end_matches('/'))?;
        } else if kind.is_file() {
            sink.file(&name, &mut entry, mode)?;
        } else if kind.is_symlink() {
            let target = entry
                .link_name_bytes()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            sink.symlink(&name, &target)?;
        } else if matches!(
            kind,
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
        ) {
            continue;
        } else {
            return Err(ExtractError::UnsupportedEntry {
                entry: name,
                kind: format!("{kind:?}"),
            });
        }
    }
    Ok(())
}

pub fn extract_zip(file: File, dest: &Path, limits: Limits) -> Result<(), ExtractError> {
    let mut sink = Sink::new(dest, limits).map_err(io_err)?;
    let mut archive = zip::ZipArchive::new(file).map_err(io_err)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(io_err)?;
        let name = String::from_utf8(entry.name_raw().to_vec())
            .map_err(|_| ExtractError::UnsafePath("<non-UTF-8 name>".to_owned()))?;
        let mode = entry.unix_mode();
        if mode.is_some_and(|m| m & 0o170000 == 0o120000) {
            return Err(ExtractError::UnsupportedEntry {
                entry: name,
                kind: "symbolic link".to_owned(),
            });
        }
        if entry.is_dir() {
            sink.dir(name.trim_end_matches('/'))?;
        } else {
            sink.file(&name, &mut entry, mode)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aivtuber-extract-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn limits() -> Limits {
        Limits::with_max_bytes(1 << 20)
    }

    /// Builds a tar whose entry names and link targets are written raw, which
    /// `tar::Builder::append_data` would refuse to do for `..`.
    fn raw_tar(entries: &[(&str, tar::EntryType, &[u8], &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind, data, link) in entries {
            let mut header = tar::Header::new_gnu();
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            if !link.is_empty() {
                header.as_old_mut().linkname[..link.len()].copy_from_slice(link.as_bytes());
            }
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn run_tar(bytes: Vec<u8>, dest: &Path) -> Result<(), ExtractError> {
        extract_tar(io::Cursor::new(bytes), dest, limits())
    }

    #[test]
    fn a_normal_tar_extracts() {
        let dest = scratch("ok");
        let bytes = raw_tar(&[
            ("pkg/", tar::EntryType::Directory, b"", ""),
            ("pkg/a.txt", tar::EntryType::Regular, b"hello", ""),
        ]);
        run_tar(bytes, &dest).unwrap();
        assert_eq!(fs::read(dest.join("pkg/a.txt")).unwrap(), b"hello");
    }

    #[test]
    fn parent_directory_and_absolute_entries_are_rejected() {
        for name in [
            "../evil.txt",
            "a/../../evil.txt",
            "/abs.txt",
            "C:/win.txt",
            "a\\b.txt",
            "a/b:stream",
        ] {
            let dest = scratch("trav");
            let bytes = raw_tar(&[(name, tar::EntryType::Regular, b"x", "")]);
            let err = run_tar(bytes, &dest).unwrap_err();
            assert!(
                matches!(err, ExtractError::UnsafePath(_)),
                "{name}: {err:?}"
            );
            let outside = dest.parent().unwrap().join("evil.txt");
            assert!(!outside.exists(), "{name} escaped");
        }
    }

    #[test]
    fn links_that_leave_the_destination_are_rejected() {
        for target in ["../../outside", "/etc/passwd", "a/../../../x"] {
            let dest = scratch("link");
            let bytes = raw_tar(&[("lib/link", tar::EntryType::Symlink, b"", target)]);
            let err = run_tar(bytes, &dest).unwrap_err();
            assert!(
                matches!(err, ExtractError::UnsafeLink { .. }),
                "{target}: {err:?}"
            );
        }
    }

    #[test]
    fn a_file_is_never_written_through_a_link_and_hard_links_are_refused() {
        let dest = scratch("through");
        let outside = scratch("through-outside");
        let _ = &outside;
        // Simulate a link that is already on disk inside the destination.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, dest.join("dir")).unwrap();
            let bytes = raw_tar(&[("dir/pwn.txt", tar::EntryType::Regular, b"x", "")]);
            assert!(matches!(
                run_tar(bytes, &dest),
                Err(ExtractError::UnsafePath(_))
            ));
            assert!(!outside.join("pwn.txt").exists());
        }
        let bytes = raw_tar(&[("h", tar::EntryType::Link, b"", "a.txt")]);
        assert!(matches!(
            run_tar(bytes, &dest),
            Err(ExtractError::UnsupportedEntry { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_safe_relative_link_is_kept() {
        let dest = scratch("safe-link");
        let bytes = raw_tar(&[
            ("lib/libx.so.0", tar::EntryType::Regular, b"elf", ""),
            ("lib/libx.so", tar::EntryType::Symlink, b"", "libx.so.0"),
        ]);
        run_tar(bytes, &dest).unwrap();
        assert_eq!(fs::read(dest.join("lib/libx.so")).unwrap(), b"elf");
    }

    #[test]
    fn expansion_and_entry_counts_are_bounded() {
        let dest = scratch("bomb");
        let bytes = raw_tar(&[("big", tar::EntryType::Regular, &vec![0u8; 4096], "")]);
        let err =
            extract_tar(io::Cursor::new(bytes), &dest, Limits::with_max_bytes(1024)).unwrap_err();
        assert_eq!(err, ExtractError::TooLarge { limit: 1024 });
        assert!(!dest.join("big").exists());

        let dest = scratch("many");
        let bytes = raw_tar(&[
            ("a", tar::EntryType::Regular, b"1", ""),
            ("b", tar::EntryType::Regular, b"2", ""),
        ]);
        let tight = Limits {
            max_bytes: 100,
            max_entries: 1,
        };
        assert_eq!(
            extract_tar(io::Cursor::new(bytes), &dest, tight).unwrap_err(),
            ExtractError::TooManyEntries { limit: 1 }
        );
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, data) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn zip_extracts_and_rejects_traversal() {
        let dir = scratch("zip");
        let good = dir.join("good.zip");
        write_zip(
            &good,
            &[("bin/llama-server.exe", b"MZ"), ("ggml.dll", b"dll")],
        );
        let out = dir.join("out");
        extract(&good, &out, limits()).unwrap();
        assert_eq!(fs::read(out.join("bin/llama-server.exe")).unwrap(), b"MZ");

        for name in ["../evil.txt", "a/../../evil.txt", "/abs.txt", "C:/evil.txt"] {
            let bad = dir.join("bad.zip");
            write_zip(&bad, &[(name, b"x")]);
            let out = dir.join("out-bad");
            let err = extract(&bad, &out, limits()).unwrap_err();
            assert!(
                matches!(err, ExtractError::UnsafePath(_)),
                "{name}: {err:?}"
            );
            assert!(!dir.join("evil.txt").exists(), "{name} escaped");
            let _ = fs::remove_dir_all(&out);
        }
    }

    #[test]
    fn the_archive_type_comes_from_the_name() {
        let dir = scratch("kind");
        let p = dir.join("x.rar");
        fs::write(&p, b"x").unwrap();
        assert!(matches!(
            extract(&p, &dir.join("o"), limits()),
            Err(ExtractError::UnknownFormat(_))
        ));
    }

    #[test]
    fn tar_gz_and_tar_bz2_round_trip() {
        let dir = scratch("compressed");
        let tar_bytes = raw_tar(&[("d/x.txt", tar::EntryType::Regular, b"data", "")]);
        let gz = dir.join("a.tar.gz");
        let mut enc =
            flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::fast());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap();
        extract(&gz, &dir.join("gz"), limits()).unwrap();
        assert_eq!(fs::read(dir.join("gz/d/x.txt")).unwrap(), b"data");

        let bz = dir.join("a.tar.bz2");
        let mut enc =
            bzip2::write::BzEncoder::new(File::create(&bz).unwrap(), bzip2::Compression::fast());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap();
        extract(&bz, &dir.join("bz"), limits()).unwrap();
        assert_eq!(fs::read(dir.join("bz/d/x.txt")).unwrap(), b"data");
    }
}
