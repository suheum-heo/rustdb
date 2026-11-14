//! Advisory exclusive lock for one database path.
//!
//! The lock file is `PATH.lock` in the same directory as the database. It is
//! created on first use and left in place after unlock. `flock` on that file
//! excludes other processes. A process-local set records the live owner in
//! this process. The set entry is inserted only after `flock` succeeds, and
//! it is removed only after that same guard closes its descriptor, so a failed
//! acquire cannot leave an owner behind and a drop cannot clear a newer one.
//!
//! `flock` is inherited across `fork` and stays held until every descriptor
//! of that file is closed. A child that has not reached `exec` can keep the
//! lock after this guard is dropped. The descriptor is close-on-exec, so the
//! child releases it when it starts a new program.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fs4::fs_std::FileExt;

use crate::error::DbError;

static OPEN_LOCKS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub(crate) struct DbLock {
    file: Option<File>,
    key: PathBuf,
}

pub(crate) fn path_for(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

pub(crate) fn acquire(database: &Path) -> Result<DbLock, DbError> {
    let lock_path = path_for(database);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| {
            DbError::Storage(format!(
                "failed to open database lock '{}': {error}",
                lock_path.display()
            ))
        })?;
    let key = fs::canonicalize(&lock_path)
        .or_else(|_| std::path::absolute(&lock_path))
        .map_err(|error| {
            DbError::Storage(format!(
                "failed to open database lock '{}': {error}",
                lock_path.display()
            ))
        })?;
    let mut open = OPEN_LOCKS
        .lock()
        .map_err(|_| DbError::Storage("database lock state is poisoned".to_string()))?;
    if open.iter().any(|held| held == &key) {
        return Err(locked(&lock_path));
    }
    match file.try_lock_exclusive() {
        Ok(true) => {
            open.push(key.clone());
            Ok(DbLock {
                file: Some(file),
                key,
            })
        }
        Ok(false) => Err(locked(&lock_path)),
        Err(error) => Err(DbError::Storage(format!(
            "failed to lock database file '{}': {error}",
            lock_path.display()
        ))),
    }
}

pub(crate) fn lock_file(file: &File, path: &Path) -> Result<(), DbError> {
    match file.try_lock_exclusive() {
        Ok(true) => Ok(()),
        Ok(false) => Err(locked(path)),
        Err(error) => Err(DbError::Storage(format!(
            "failed to lock database file '{}': {error}",
            path.display()
        ))),
    }
}

fn locked(path: &Path) -> DbError {
    DbError::Storage(format!("database file '{}' is locked", path.display()))
}

fn release_key(key: &Path) {
    if let Ok(mut open) = OPEN_LOCKS.lock()
        && let Some(index) = open.iter().position(|held| held == key)
    {
        open.swap_remove(index);
    }
}

impl Drop for DbLock {
    fn drop(&mut self) {
        // Close the descriptor before forgetting the owner. A concurrent
        // acquire still sees the key and does not record itself, so this
        // drop cannot erase a newer guard.
        drop(self.file.take());
        release_key(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_database() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rustdb-lock-life-{}-{}-{n}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join("db")
    }

    #[test]
    fn dropping_the_only_guard_lets_the_same_process_acquire_again() {
        let path = temp_database();
        for _ in 0..100 {
            let held = acquire(&path).unwrap();
            let error = match acquire(&path) {
                Err(error) => error,
                Ok(_) => panic!("second acquire should fail"),
            };
            assert!(error.to_string().contains("is locked"), "{error}");
            drop(held);
            let held = acquire(&path).unwrap();
            drop(held);
        }
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn contended_acquires_keep_one_live_owner() {
        let path = temp_database();
        let start = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            let start = Arc::clone(&start);
            handles.push(thread::spawn(move || {
                start.wait();
                for _ in 0..40 {
                    match acquire(&path) {
                        Ok(held) => {
                            let error = match acquire(&path) {
                                Err(error) => error,
                                Ok(_) => panic!("live guard was not visible"),
                            };
                            assert!(error.to_string().contains("is locked"), "{error}");
                            drop(held);
                        }
                        Err(error) => {
                            assert!(error.to_string().contains("is locked"), "{error}");
                        }
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let held = acquire(&path).unwrap();
        drop(held);
        assert!(path_for(&path).is_file());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
