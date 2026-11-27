//! Test-only scratch directory that removes itself on drop (#542).
//!
//! Shared by unit tests (`mod test_dir`) and integration tests
//! (`#[path = "../src/test_dir.rs"]`), so it must stay dependency-free.
#![allow(dead_code)]

use std::ffi::OsStr;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// Owns a directory under the temp root and deletes it when dropped, so a
/// passing or panicking test never leaves `aivtuber-*` directories behind.
#[derive(Debug)]
pub struct TestDir {
    /// What `Drop` removes.
    owned: PathBuf,
    /// What the test sees (the directory itself, or a path inside it).
    shown: PathBuf,
}

impl TestDir {
    /// Takes ownership of `path`: any stale copy is removed, then it is created.
    pub fn create(path: PathBuf) -> Self {
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create test directory");
        Self {
            shown: path.clone(),
            owned: path,
        }
    }

    /// Like [`TestDir::create`] under the temp root, suffixed with the process id.
    pub fn named(prefix: &str) -> Self {
        Self::create(std::env::temp_dir().join(format!("{prefix}-{}", std::process::id())))
    }

    /// Shows `relative` inside the directory while the whole directory is
    /// still removed on drop, for helpers that hand back a path in a scratch tree.
    pub fn exposing(mut self, relative: &str) -> Self {
        self.shown = self.owned.join(relative);
        self
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.owned);
    }
}

impl Deref for TestDir {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.shown
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.shown
    }
}

impl AsRef<OsStr> for TestDir {
    fn as_ref(&self) -> &OsStr {
        self.shown.as_os_str()
    }
}

thread_local! {
    static HELD: std::cell::RefCell<Vec<TestDir>> = const { std::cell::RefCell::new(Vec::new()) };
}

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Creates a fresh `<prefix>-<pid>-<seq>` directory under the temp root and
/// removes it when the calling thread exits. libtest runs every test on its
/// own thread, so for helpers that return a bare `PathBuf` (and cannot hand a
/// guard back) this scopes the directory to the test. The sequence number
/// keeps concurrent tests from sharing, or deleting, each other's directory.
pub fn scratch_for_test_thread(prefix: &str) -> PathBuf {
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = TestDir::create(
        std::env::temp_dir().join(format!("{prefix}-{}-{seq}", std::process::id())),
    );
    let shown = dir.to_path_buf();
    HELD.with(|held| held.borrow_mut().push(dir));
    shown
}
