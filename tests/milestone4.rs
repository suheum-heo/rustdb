//! Milestone 4: exclusive locking across processes.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};

/// One test at a time. `flock` survives `fork` until the child execs, so a
/// sibling `Command::spawn` can keep another test's lock after that test
/// drops its database.
fn one_test_at_a_time() -> MutexGuard<'static, ()> {
    static GATE: Mutex<()> = Mutex::new(());
    GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_for_prompt(child: &mut Child) {
    let mut stdout = child.stdout.take().expect("child stdout");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64];
    loop {
        match stdout.read(&mut chunk) {
            Ok(0) => {
                let stderr = child_stderr(child);
                panic!(
                    "child exited before the prompt; stderr: {stderr}; stdout: {}",
                    String::from_utf8_lossy(&buf)
                );
            }
            Ok(count) => {
                buf.extend_from_slice(&chunk[..count]);
                if buf.windows(7).any(|window| window == b"rustdb>") {
                    return;
                }
            }
            Err(error) => panic!("failed to read the child prompt: {error}"),
        }
    }
}

fn child_stderr(child: &mut Child) -> String {
    let _ = child.kill();
    let _ = child.wait();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    stderr
}

#[test]
fn a_second_process_cannot_open_a_locked_database() {
    let _gate = one_test_at_a_time();
    let dir = std::env::temp_dir().join(format!(
        "rustdb-lock-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db");
    drop(rustdb::Database::open(&path).unwrap());

    let mut child = Command::new(env!("CARGO_BIN_EXE_rustdb"))
        .arg("--database")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_prompt(&mut child);
    let error = match rustdb::Database::open(&path) {
        Err(error) => error,
        Ok(_) => panic!("second process should hold the database"),
    };
    assert!(error.to_string().contains("is locked"), "{error}");
    let _ = child.kill();
    let _ = child.wait();

    rustdb::Database::open(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_open_in_this_process_is_rejected_and_the_lock_file_stays() {
    let _gate = one_test_at_a_time();
    let dir = std::env::temp_dir().join(format!(
        "rustdb-lock-same-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db");
    let database = rustdb::Database::open(&path).unwrap();
    for _ in 0..30 {
        let error = match rustdb::Database::open(&path) {
            Err(error) => error,
            Ok(_) => panic!("second open should fail"),
        };
        assert!(error.to_string().contains("is locked"), "{error}");
    }
    drop(database);
    let mut lock_path = path.clone().into_os_string();
    lock_path.push(".lock");
    assert!(std::path::Path::new(&lock_path).is_file());
    for _ in 0..30 {
        let again = rustdb::Database::open(&path).unwrap();
        drop(again);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn migration_waits_behind_an_open_database() {
    let _gate = one_test_at_a_time();
    let dir = std::env::temp_dir().join(format!(
        "rustdb-lock-migrate-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db");
    let database = rustdb::Database::open(&path).unwrap();
    let error = rustdb::Database::migrate(&path).unwrap_err();
    assert!(error.to_string().contains("is locked"), "{error}");
    drop(database);
    let error = rustdb::Database::migrate(&path).unwrap_err();
    assert!(
        error.to_string().contains("already format version 2"),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn renaming_the_database_file_keeps_the_other_process_lock() {
    let _gate = one_test_at_a_time();
    let dir = std::env::temp_dir().join(format!(
        "rustdb-lock-rename-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db");
    drop(rustdb::Database::open(&path).unwrap());

    let mut child = Command::new(env!("CARGO_BIN_EXE_rustdb"))
        .arg("--database")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_prompt(&mut child);

    let renamed = dir.join("renamed");
    std::fs::rename(&path, &renamed).unwrap();
    let error = match rustdb::Database::open(&renamed) {
        Err(error) => error,
        Ok(_) => panic!("renamed database should still be locked"),
    };
    assert!(error.to_string().contains("is locked"), "{error}");

    let _ = child.kill();
    let _ = child.wait();
    rustdb::Database::open(&renamed).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_child_that_has_execed_does_not_keep_the_lock() {
    let _gate = one_test_at_a_time();
    let dir = std::env::temp_dir().join(format!(
        "rustdb-lock-exec-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db");
    let database = rustdb::Database::open(&path).unwrap();
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id().to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let output = Command::new("ps")
            .args(["-p", &pid, "-o", "comm="])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&output.stdout).contains("sleep") {
            break;
        }
        assert!(
            deadline > std::time::Instant::now(),
            "child did not exec sleep"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(database);
    rustdb::Database::open(&path).unwrap();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
