//! Directories for unit tests.
//!
//! Integration tests keep a copy in `tests/temp_dir.rs` because this module is
//! compiled only into the library's own test binary.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A directory that no other test in this process can observe.
///
/// The name is the process id plus a counter. `create_dir` refuses a name
/// that already exists, so a leftover from an earlier process is skipped
/// instead of deleted. Drop removes this directory.
pub(crate) struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub(crate) fn new(label: &str) -> Self {
        Self {
            path: unique_dir(label),
        }
    }

    pub(crate) fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn unique_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("rustdb-{label}-{pid}-{n}"));
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("failed to create {}: {error}", path.display()),
        }
    }
}
