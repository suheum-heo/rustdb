//! Per-test directories for the integration binaries.
//!
//! The library's test helper is not visible here. Names use the process id
//! and a counter, and `create_dir` skips a name that already exists.
//! Each integration crate uses a different entry point, so unused items in
//! one crate are expected.

#![allow(dead_code)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct KeepDir {
    path: PathBuf,
}

impl KeepDir {
    pub fn new(label: &str) -> Self {
        Self {
            path: unique_dir(label),
        }
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl AsRef<Path> for KeepDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for KeepDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn unique_dir(label: &str) -> PathBuf {
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
