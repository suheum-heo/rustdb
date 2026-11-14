//! One-statement redo log of sealed page images.
//!
//! The log is a sidecar named by appending `.wal` to the database path. A
//! trailer with a matching checksum is the commit record. Recovery installs
//! those pages and then truncates the log. A log with no trailer may be
//! discarded when it belongs to this database. A damaged trailer is kept.
//! `fsync` of the log is the caller's job, before any database page is
//! modified. A failed `fsync` does not prove the commit record is absent.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::DbError;
use crate::storage::page::{self, PAGE_SIZE, Page};

const LOG_MAGIC: [u8; 8] = *b"RUSTWAL\0";
const COMMIT_MAGIC: [u8; 8] = *b"RUSTCMT\0";
const HEADER_LEN: usize = 64;
const TRAILER_LEN: usize = 32;
const FRAME_LEN: usize = 4 + PAGE_SIZE;
const LOG_VERSION: u32 = 1;

#[derive(Debug)]
pub(crate) enum StepError {
    BeforeCommit(DbError),
    AfterCommit(DbError),
}

impl StepError {
    pub(crate) fn into_db(self) -> DbError {
        match self {
            Self::BeforeCommit(error) | Self::AfterCommit(error) => error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Inject {
    /// Header and frames are left on disk. There is no trailer, and the log
    /// has not been synced. Recovery may discard this log.
    BeforeLogSync,
    /// The commit record was written and `sync_all` is treated as failed.
    /// The log is left intact because durability is unknown.
    CommitSyncFailed,
    /// A synced trailer is left on disk. The database file is not modified.
    AfterLogSync,
    /// The log is synced and this many checkpoint writes have hit the file.
    DuringCheckpoint(u32),
    /// The database file is synced and the log is still complete.
    #[cfg_attr(not(test), allow(dead_code))]
    AfterDbSync,
    /// The database file is synced. The log truncate has not happened.
    #[cfg_attr(not(test), allow(dead_code))]
    BeforeLogClear,
}

pub(crate) fn path_for(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".wal");
    PathBuf::from(name)
}

pub(crate) struct Plan<'a> {
    pub nonce: u64,
    pub base_commit: u64,
    pub frames: &'a [(u32, Page)],
}

/// Bytes of a log with a commit trailer. `new commit id` is `base + 1`.
pub(crate) fn encode_committed(plan: &Plan<'_>) -> Result<Vec<u8>, DbError> {
    let mut bytes = encode_body(plan)?;
    let frames = &bytes[HEADER_LEN..];
    let mut trailer = [0u8; TRAILER_LEN];
    trailer[..8].copy_from_slice(&COMMIT_MAGIC);
    trailer[8..12].copy_from_slice(
        &u32::try_from(plan.frames.len())
            .map_err(|_| DbError::Internal("redo log frame count does not fit".to_string()))?
            .to_le_bytes(),
    );
    trailer[12..16].copy_from_slice(&page::checksum(frames).to_le_bytes());
    let new_commit = plan
        .base_commit
        .checked_add(1)
        .ok_or_else(|| DbError::Storage("database commit id space is exhausted".to_string()))?;
    trailer[16..24].copy_from_slice(&new_commit.to_le_bytes());
    bytes.extend(trailer);
    Ok(bytes)
}

pub(crate) fn encode_uncommitted(plan: &Plan<'_>) -> Result<Vec<u8>, DbError> {
    encode_body(plan)
}

fn encode_body(plan: &Plan<'_>) -> Result<Vec<u8>, DbError> {
    if plan.nonce == 0 {
        return Err(DbError::Internal(
            "redo log requires a database nonce".to_string(),
        ));
    }
    let mut bytes = vec![0u8; HEADER_LEN];
    bytes[..8].copy_from_slice(&LOG_MAGIC);
    bytes[8..12].copy_from_slice(&LOG_VERSION.to_le_bytes());
    bytes[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    bytes[16..24].copy_from_slice(&plan.nonce.to_le_bytes());
    bytes[24..32].copy_from_slice(&plan.base_commit.to_le_bytes());
    for (id, page) in plan.frames {
        bytes.extend(id.to_le_bytes());
        if page.as_slice().len() != PAGE_SIZE {
            return Err(DbError::Internal(
                "redo log frame is not one page".to_string(),
            ));
        }
        bytes.extend(page.as_slice());
    }
    Ok(bytes)
}

struct ParsedLog {
    nonce: u64,
    base_commit: u64,
    frames: Vec<(u32, Page)>,
    new_commit: u64,
}

enum LogBody {
    /// No commit trailer. Recovery may delete the log when the header belongs
    /// to this database and the commit id is the current one or one behind it.
    Incomplete,
    Committed(ParsedLog),
}

/// Read a log file.
///
/// `Ok(Incomplete)` is the only result recovery may truncate. A trailer whose
/// magic matches but whose length or checksum does not is returned as `Err`
/// and must be left on disk: a commit was attempted and the record is damaged.
fn parse_committed(bytes: &[u8]) -> Result<LogBody, DbError> {
    if bytes.len() < HEADER_LEN || bytes[..8] != LOG_MAGIC {
        return Err(DbError::Storage(
            "redo log does not belong to this database".to_string(),
        ));
    }
    let version = read_u32_at(bytes, 8)?;
    let page_size = read_u32_at(bytes, 12)?;
    if version != LOG_VERSION || page_size != PAGE_SIZE as u32 {
        return Err(DbError::Storage(
            "redo log does not belong to this database".to_string(),
        ));
    }
    let nonce = read_u64_at(bytes, 16)?;
    let base_commit = read_u64_at(bytes, 24)?;
    let body = &bytes[HEADER_LEN..];
    if body.len() < TRAILER_LEN
        || body[body.len() - TRAILER_LEN..body.len() - TRAILER_LEN + 8] != COMMIT_MAGIC
    {
        return Ok(LogBody::Incomplete);
    }
    let trailer = &body[body.len() - TRAILER_LEN..];
    let frame_count = read_u32_at(trailer, 8)? as usize;
    let stored_checksum = read_u32_at(trailer, 12)?;
    let new_commit = read_u64_at(trailer, 16)?;
    let frame_bytes_len = frame_count
        .checked_mul(FRAME_LEN)
        .ok_or_else(|| DbError::Storage("redo log commit record is damaged".to_string()))?;
    if body.len() != frame_bytes_len + TRAILER_LEN {
        return Err(DbError::Storage(
            "redo log commit record is damaged".to_string(),
        ));
    }
    let frames_bytes = &body[..frame_bytes_len];
    if page::checksum(frames_bytes) != stored_checksum {
        return Err(DbError::Storage(
            "redo log commit record is damaged".to_string(),
        ));
    }
    let expected = base_commit.checked_add(1).ok_or_else(|| {
        DbError::Storage("redo log commit id conflicts with the database".to_string())
    })?;
    if new_commit != expected {
        return Err(DbError::Storage(
            "redo log commit id conflicts with the database".to_string(),
        ));
    }
    let mut frames = Vec::with_capacity(frame_count);
    for chunk in frames_bytes.chunks(FRAME_LEN) {
        let id = read_u32_at(chunk, 0)?;
        let mut page = Page::new();
        page.as_mut_slice().copy_from_slice(&chunk[4..]);
        page::verify(&page, id)?;
        frames.push((id, page));
    }
    Ok(LogBody::Committed(ParsedLog {
        nonce,
        base_commit,
        frames,
        new_commit,
    }))
}

/// How a log file should be treated after a failed write.
pub(crate) enum LogClass {
    /// No commit trailer. The caller may delete the log.
    Incomplete,
    /// A valid commit record. The caller must keep it.
    Committed,
    /// A trailer or header that cannot be trusted. The caller must keep it.
    Damaged,
}

pub(crate) fn classify_log(bytes: &[u8]) -> LogClass {
    if bytes.is_empty() {
        return LogClass::Incomplete;
    }
    match parse_committed(bytes) {
        Ok(LogBody::Incomplete) => LogClass::Incomplete,
        Ok(LogBody::Committed(_)) => LogClass::Committed,
        Err(error) if error.to_string().contains("damaged") => LogClass::Damaged,
        Err(_) if bytes.len() >= HEADER_LEN && bytes[..8] == LOG_MAGIC => LogClass::Damaged,
        Err(_) => LogClass::Incomplete,
    }
}

fn read_u32_at(bytes: &[u8], offset: usize) -> Result<u32, DbError> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| DbError::Storage("redo log does not belong to this database".to_string()))?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| DbError::Storage("redo log does not belong to this database".to_string()))?;
    let bytes: [u8; 4] = slice
        .try_into()
        .map_err(|_| DbError::Storage("redo log does not belong to this database".to_string()))?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64_at(bytes: &[u8], offset: usize) -> Result<u64, DbError> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| DbError::Storage("redo log does not belong to this database".to_string()))?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| DbError::Storage("redo log does not belong to this database".to_string()))?;
    let bytes: [u8; 8] = slice
        .try_into()
        .map_err(|_| DbError::Storage("redo log does not belong to this database".to_string()))?;
    Ok(u64::from_le_bytes(bytes))
}

/// Install a committed log, or drop an incomplete one.
///
/// A log may be truncated only when it has no commit trailer, its nonce is
/// this database's nonce, and its base commit id is the id in page 0 or one
/// less than that id. A valid trailer is replayed when the nonce matches and
/// either the base id or the new id equals the id in page 0. Replaying an
/// already-applied trailer writes the same pages again. A damaged trailer, a
/// foreign nonce, or any other commit id is left on disk and the open fails.
pub(crate) fn recover(database: &mut File, wal_path: &Path) -> Result<(), DbError> {
    if !wal_path.exists() {
        return Ok(());
    }
    let bytes = fs::read(wal_path).map_err(|error| {
        DbError::Storage(format!(
            "failed to read redo log '{}': {error}",
            wal_path.display()
        ))
    })?;
    if bytes.is_empty() {
        return Ok(());
    }
    let meta_page = read_page0(database)?;
    let meta = page::read_meta(&meta_page)?;
    if meta.version != page::FORMAT_VERSION && meta.version != page::FORMAT_VERSION_V3 {
        return Err(DbError::Storage(
            "refusing to recover a redo log for a database that is not format version 2"
                .to_string(),
        ));
    }
    match parse_committed(&bytes)? {
        LogBody::Incomplete => discard_incomplete(&bytes, &meta, wal_path),
        LogBody::Committed(log) => apply_committed(database, wal_path, &meta, log),
    }
}

fn discard_incomplete(bytes: &[u8], meta: &page::Meta, wal_path: &Path) -> Result<(), DbError> {
    if header_nonce(bytes) != Some(meta.nonce) {
        return Err(DbError::Storage(
            "redo log does not belong to this database".to_string(),
        ));
    }
    let Some(base) = header_commit(bytes) else {
        return Err(DbError::Storage(
            "redo log does not belong to this database".to_string(),
        ));
    };
    let next = base.checked_add(1).ok_or_else(|| {
        DbError::Storage("redo log commit id conflicts with the database".to_string())
    })?;
    if base != meta.commit_id && next != meta.commit_id {
        return Err(DbError::Storage(
            "redo log commit id conflicts with the database".to_string(),
        ));
    }
    truncate_log(wal_path)
}

fn apply_committed(
    database: &mut File,
    wal_path: &Path,
    meta: &page::Meta,
    log: ParsedLog,
) -> Result<(), DbError> {
    if log.nonce != meta.nonce {
        return Err(DbError::Storage(
            "redo log does not belong to this database".to_string(),
        ));
    }
    if log.base_commit != meta.commit_id && log.new_commit != meta.commit_id {
        return Err(DbError::Storage(
            "redo log commit id conflicts with the database".to_string(),
        ));
    }
    let Some((_, meta_page)) = log.frames.iter().find(|(id, _)| *id == 0) else {
        return Err(DbError::Storage(
            "redo log commit id conflicts with the database".to_string(),
        ));
    };
    let frame_meta = page::read_meta(meta_page)?;
    if frame_meta.nonce != log.nonce || frame_meta.commit_id != log.new_commit {
        return Err(DbError::Storage(
            "redo log commit id conflicts with the database".to_string(),
        ));
    }
    if frame_meta.version != meta.version {
        return Err(DbError::Storage(
            "redo log uses a database format this file has not migrated to".to_string(),
        ));
    }
    let mut seen = HashSet::with_capacity(log.frames.len());
    for (id, page) in &log.frames {
        if !seen.insert(*id) {
            return Err(DbError::Storage(
                "redo log commit id conflicts with the database".to_string(),
            ));
        }
        if meta.version == page::FORMAT_VERSION {
            let page_type = page::page_type_of(page);
            if page_type == page::TYPE_INDEX_CATALOG
                || page_type == page::TYPE_INDEX_INTERNAL
                || page_type == page::TYPE_INDEX_LEAF
            {
                return Err(DbError::Storage(
                    "redo log uses a database format this file has not migrated to".to_string(),
                ));
            }
        }
    }
    for (id, page) in &log.frames {
        write_page(database, *id, page)?;
    }
    database.sync_all().map_err(|error| {
        DbError::Storage(format!(
            "failed to sync database file during recovery: {error}"
        ))
    })?;
    truncate_log(wal_path)
}

fn header_nonce(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < HEADER_LEN || bytes[..8] != LOG_MAGIC {
        return None;
    }
    Some(u64::from_le_bytes(bytes[16..24].try_into().ok()?))
}

fn header_commit(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < HEADER_LEN || bytes[..8] != LOG_MAGIC {
        return None;
    }
    Some(u64::from_le_bytes(bytes[24..32].try_into().ok()?))
}

fn read_page0(file: &mut File) -> Result<Page, DbError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| DbError::Storage(format!("failed to read database file: {error}")))?;
    let mut page = Page::new();
    file.read_exact(page.as_mut_slice())
        .map_err(|error| DbError::Storage(format!("failed to read database file: {error}")))?;
    Ok(page)
}

pub(crate) fn write_page(file: &mut File, id: u32, page: &Page) -> Result<(), DbError> {
    file.seek(SeekFrom::Start(u64::from(id) * PAGE_SIZE as u64))
        .map_err(|error| DbError::Storage(format!("failed to write database file: {error}")))?;
    file.write_all(page.as_slice())
        .map_err(|error| DbError::Storage(format!("failed to write database file: {error}")))?;
    Ok(())
}

pub(crate) fn truncate_log(wal_path: &Path) -> Result<(), DbError> {
    let file = File::options()
        .write(true)
        .open(wal_path)
        .map_err(|error| {
            DbError::Storage(format!(
                "failed to clear redo log '{}': {error}",
                wal_path.display()
            ))
        })?;
    file.set_len(0).map_err(|error| {
        DbError::Storage(format!(
            "failed to clear redo log '{}': {error}",
            wal_path.display()
        ))
    })?;
    file.sync_all().map_err(|error| {
        DbError::Storage(format!(
            "failed to clear redo log '{}': {error}",
            wal_path.display()
        ))
    })?;
    Ok(())
}
