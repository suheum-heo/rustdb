//! Reads pages and checkpoints dirty pages in place.
//!
//! An ordinary commit writes a redo log, syncs that same file, writes those
//! pages into the database, syncs the database, and truncates the log. A
//! failed log sync keeps the log. Creating a database and migrating version 1
//! still replace the file through a temporary name. The path lock is
//! `PATH.lock` and is acquired before any of that work.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::DbError;
use crate::storage::lock::DbLock;
use crate::storage::page::{self, PAGE_SIZE, Page};
use crate::storage::profile;
use crate::storage::wal::{self, Inject, StepError};

const PAGE_SIZE_U64: u64 = PAGE_SIZE as u64;

pub(crate) struct Pager {
    path: PathBuf,
    file: File,
    page_count: u32,
    cache: Vec<Option<Page>>,
    held: Option<DbLock>,
}

/// Uncommitted pages for one publish.
pub(crate) struct Image {
    pages: Vec<Option<Page>>,
    /// Next id to append. Also the page count once the image is complete.
    next_id: u32,
    reusable: Vec<u32>,
    allocated: Vec<u32>,
}

impl Pager {
    pub(crate) fn open(path: &Path) -> Result<Self, DbError> {
        Self::open_locked(path, false)
    }

    /// Open a version 1 file for the explicit migration command.
    pub(crate) fn open_v1(path: &Path) -> Result<Self, DbError> {
        Self::open_locked(path, true)
    }

    fn open_locked(path: &Path, allow_v1: bool) -> Result<Self, DbError> {
        let held = crate::storage::lock::acquire(path)?;
        if !path.exists()
            || fs::metadata(path)
                .map_err(|error| open_error(path, &error))?
                .len()
                == 0
        {
            if allow_v1 {
                return Err(DbError::Storage(format!(
                    "database file '{}' is not format version 1",
                    path.display()
                )));
            }
            let pages = initial_pages_with(fresh_nonce())?;
            let mut pager = Self::create(path, pages)?;
            pager.held = Some(held);
            return Ok(pager);
        }

        let file = File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| open_error(path, &error))?;
        crate::storage::lock::lock_file(&file, path)?;
        let length = file
            .metadata()
            .map_err(|error| open_error(path, &error))?
            .len();
        if length < PAGE_SIZE_U64 || length % PAGE_SIZE_U64 != 0 {
            return Err(DbError::Storage("database file is truncated".to_string()));
        }
        let mut pager = Self {
            path: path.to_path_buf(),
            file,
            page_count: u32::try_from(length / PAGE_SIZE_U64)
                .map_err(|_| DbError::Storage("database file is corrupt".to_string()))?,
            cache: Vec::new(),
            held: Some(held),
        };
        let meta = {
            let mut page = Page::new();
            pager.read_into(0, &mut page)?;
            page::read_meta(&page)?
        };
        if meta.version == page::FORMAT_VERSION_V1 {
            if u64::from(meta.page_count) * PAGE_SIZE_U64 != length {
                return Err(DbError::Storage("database file is truncated".to_string()));
            }
            if allow_v1 {
                pager.page_count = meta.page_count;
                return Ok(pager);
            }
            return Err(page::migration_required(path));
        }
        if allow_v1 {
            return Err(DbError::Storage(format!(
                "database file is already format version {}",
                meta.version
            )));
        }
        // `read_meta` already accepted this version. Recovery does not run for
        // any other version, and it does not run before this point.
        wal::recover(&mut pager.file, &wal::path_for(path))?;
        pager.cache.clear();
        let length = pager
            .file
            .metadata()
            .map_err(|error| open_error(path, &error))?
            .len();
        if length < PAGE_SIZE_U64 || length % PAGE_SIZE_U64 != 0 {
            return Err(DbError::Storage("database file is truncated".to_string()));
        }
        pager.page_count = u32::try_from(length / PAGE_SIZE_U64)
            .map_err(|_| DbError::Storage("database file is corrupt".to_string()))?;
        let meta = {
            let mut page = Page::new();
            pager.read_into(0, &mut page)?;
            page::read_meta(&page)?
        };
        if u64::from(meta.page_count) * PAGE_SIZE_U64 != length {
            return Err(DbError::Storage("database file is truncated".to_string()));
        }
        pager.page_count = meta.page_count;
        Ok(pager)
    }

    pub(crate) fn page_count(&self) -> u32 {
        self.page_count
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn read_into(&mut self, id: u32, dest: &mut Page) -> Result<(), DbError> {
        self.load(id)?;
        let index = usize::try_from(id)
            .map_err(|_| DbError::Internal(format!("page {id} was missing from the cache")))?;
        let Some(page) = self.cache.get(index).and_then(Option::as_ref) else {
            return Err(DbError::Internal(format!(
                "page {id} was missing from the cache"
            )));
        };
        dest.as_mut_slice().copy_from_slice(page.as_slice());
        Ok(())
    }

    /// Sync a redo log of `pages`, write those pages, sync the database, then
    /// truncate the log. `base_commit` is the commit id already durable in page 0.
    pub(crate) fn checkpoint(
        &mut self,
        pages: Vec<(u32, Page)>,
        page_count: u32,
        nonce: u64,
        base_commit: u64,
        inject: Option<Inject>,
    ) -> Result<(), StepError> {
        if pages.len() > page::commit_frame_limit() || pages.iter().any(|(id, _)| *id >= page_count)
        {
            return Err(StepError::BeforeCommit(DbError::Storage(
                "statement changes more pages than one commit can record".to_string(),
            )));
        }
        let wal_path = wal::path_for(&self.path);
        let plan = wal::Plan {
            nonce,
            base_commit,
            frames: &pages,
        };
        let bytes = if inject == Some(Inject::BeforeLogSync) {
            wal::encode_uncommitted(&plan).map_err(StepError::BeforeCommit)?
        } else {
            wal::encode_committed(&plan).map_err(StepError::BeforeCommit)?
        };
        let wal_mark = profile::start();
        let written = write_log(&wal_path, &bytes);
        profile::add_wal_write(wal_mark);
        let wal_file = match written {
            Ok(file) => file,
            Err(error) => return Err(commit_write_error(&wal_path, error)),
        };
        if inject == Some(Inject::BeforeLogSync) {
            return Err(StepError::BeforeCommit(DbError::Storage(
                "injected failure before the redo log was synced".to_string(),
            )));
        }
        if inject == Some(Inject::CommitSyncFailed) {
            return Err(ambiguous_sync());
        }
        let sync_mark = profile::start();
        let synced = wal_file.sync_all();
        profile::add_wal_sync(sync_mark);
        if let Err(error) = synced {
            return Err(ambiguous_sync_io(&wal_path, &error));
        }
        if inject == Some(Inject::AfterLogSync) {
            return Err(StepError::AfterCommit(DbError::Storage(
                "injected failure after the redo log was synced".to_string(),
            )));
        }
        let write_mark = profile::start();
        for (written, (id, page)) in pages.iter().enumerate() {
            if inject
                == Some(Inject::DuringCheckpoint(
                    u32::try_from(written).unwrap_or(u32::MAX),
                ))
            {
                return Err(StepError::AfterCommit(DbError::Storage(
                    "injected failure during checkpoint".to_string(),
                )));
            }
            wal::write_page(&mut self.file, *id, page).map_err(StepError::AfterCommit)?;
        }
        profile::add_db_write(write_mark);
        let db_sync = profile::start();
        let synced = self.file.sync_all();
        profile::add_db_sync(db_sync);
        synced.map_err(|error| {
            StepError::AfterCommit(DbError::Storage(format!(
                "failed to sync database file '{}': {error}",
                self.path.display()
            )))
        })?;
        if matches!(inject, Some(Inject::AfterDbSync | Inject::BeforeLogClear)) {
            return Err(StepError::AfterCommit(DbError::Storage(
                "injected failure before the redo log was cleared".to_string(),
            )));
        }
        let clear_mark = profile::start();
        let cleared = wal::truncate_log(&wal_path);
        profile::add_wal_clear(clear_mark);
        cleared.map_err(StepError::AfterCommit)?;
        profile::note_frames(u32::try_from(pages.len()).unwrap_or(u32::MAX));
        self.page_count = page_count;
        self.cache.clear();
        Ok(())
    }

    fn create(path: &Path, pages: Vec<Page>) -> Result<Self, DbError> {
        let file = persist(path, &pages).map_err(PersistError::into_db_error)?;
        crate::storage::lock::lock_file(&file, path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            page_count: u32::try_from(pages.len())
                .map_err(|_| DbError::Internal("published page count does not fit".to_string()))?,
            cache: Vec::new(),
            held: None,
        })
    }

    fn load(&mut self, id: u32) -> Result<(), DbError> {
        if id >= self.page_count {
            return Err(DbError::Storage(format!(
                "database page {id} is outside the file"
            )));
        }
        let index = usize::try_from(id)
            .map_err(|_| DbError::Storage(format!("database page {id} is outside the file")))?;
        let needed = index.checked_add(1).ok_or_else(|| {
            DbError::Internal(format!("page {id} does not fit in the page cache"))
        })?;
        if self.cache.len() < needed {
            self.cache.resize_with(needed, || None);
        }
        if self.cache[index].is_some() {
            return Ok(());
        }
        let mut page = Page::new();
        self.file
            .seek(SeekFrom::Start(u64::from(id) * PAGE_SIZE_U64))
            .map_err(|error| read_error(&self.path, &error))?;
        self.file.read_exact(page.as_mut_slice()).map_err(|error| {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                DbError::Storage("database file is truncated".to_string())
            } else {
                read_error(&self.path, &error)
            }
        })?;
        page::verify(&page, id)?;
        self.cache[index] = Some(page);
        Ok(())
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        // Release the path lock before the file's inode lock drops with `file`.
        drop(self.held.take());
    }
}

impl Image {
    /// An empty image used to build a complete file, including migration.
    pub(crate) fn fresh() -> Self {
        Self {
            pages: Vec::new(),
            next_id: 1,
            reusable: Vec::new(),
            allocated: Vec::new(),
        }
    }

    /// Reuse a free data page, or append a page id past the current end.
    pub(crate) fn allocate(&mut self) -> Result<u32, DbError> {
        let id = if let Some(id) = self.reusable.pop() {
            id
        } else {
            let id = self.next_id;
            self.next_id = self.next_id.checked_add(1).ok_or_else(|| {
                DbError::Storage("database page id space is exhausted".to_string())
            })?;
            id
        };
        self.allocated.push(id);
        Ok(id)
    }

    pub(crate) fn write(&mut self, id: u32, page: Page) -> Result<(), DbError> {
        let index = usize::try_from(id)
            .map_err(|_| DbError::Internal(format!("page {id} does not fit in the image")))?;
        let needed = index
            .checked_add(1)
            .ok_or_else(|| DbError::Internal(format!("page {id} does not fit in the image")))?;
        if self.pages.len() < needed {
            self.pages.resize_with(needed, || None);
        }
        self.pages[index] = Some(page);
        Ok(())
    }

    pub(crate) fn page_mut(&mut self, id: u32) -> Result<&mut Page, DbError> {
        let index = usize::try_from(id)
            .map_err(|_| DbError::Internal(format!("image is missing page {id}")))?;
        self.pages
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or_else(|| DbError::Internal(format!("image is missing page {id}")))
    }

    pub(crate) fn next_id(&self) -> u32 {
        self.next_id
    }

    /// Page ids below `next_id` that this image did not allocate.
    ///
    /// Page 0 is not included. Call this before allocating freelist directory
    /// pages; those fresh pages are live and must not appear in the result.
    pub(crate) fn free_ids(&self) -> Vec<u32> {
        (1..self.next_id)
            .filter(|id| !self.allocated.contains(id))
            .collect()
    }

    pub(crate) fn into_pages(mut self) -> Result<Vec<Page>, DbError> {
        if self.pages.first().and_then(Option::as_ref).is_none() {
            return Err(DbError::Internal(
                "image is missing the meta page".to_string(),
            ));
        }
        let count = usize::try_from(self.next_id)
            .map_err(|_| DbError::Internal("published page count does not fit".to_string()))?;
        let mut pages = Vec::with_capacity(count);
        for id in 0..self.next_id {
            let index = usize::try_from(id)
                .map_err(|_| DbError::Internal(format!("image is missing page {id}")))?;
            let page = if index >= self.pages.len() {
                None
            } else {
                self.pages[index].take()
            };
            let mut page = match page {
                Some(page) => page,
                None if id == 0 || self.allocated.contains(&id) => {
                    return Err(DbError::Internal(format!("image is missing page {id}")));
                }
                None => {
                    let mut free = Page::new();
                    page::write_free_data(&mut free, id);
                    free
                }
            };
            if page::page_id_of(&page) != id {
                return Err(page::corrupt(id));
            }
            page::seal(&mut page);
            page::verify(&page, id)?;
            pages.push(page);
        }
        Ok(pages)
    }
}

#[cfg(test)]
fn initial_pages() -> Result<Vec<Page>, DbError> {
    initial_pages_with(1)
}

fn initial_pages_with(nonce: u64) -> Result<Vec<Page>, DbError> {
    let mut meta = Page::new();
    page::write_meta(
        &mut meta,
        page::Meta {
            version: page::FORMAT_VERSION,
            page_count: 2,
            catalog_root: 1,
            freelist_root: 0,
            nonce,
            commit_id: 0,
            next_table_id: 1,
            index_catalog_root: 0,
        },
        &[],
    )?;
    let mut catalog = Page::new();
    page::init_slotted(&mut catalog, page::TYPE_CATALOG, 1);
    let mut image = Image {
        pages: Vec::new(),
        next_id: 2,
        reusable: Vec::new(),
        allocated: vec![1],
    };
    image.write(0, meta)?;
    image.write(1, catalog)?;
    image.into_pages()
}

#[derive(Debug)]
enum PersistError {
    /// The destination still names the previous database file.
    NotVisible(DbError),
    /// `rename` replaced the destination. The new bytes are the database.
    Visible(DbError),
}

impl PersistError {
    fn into_db_error(self) -> DbError {
        match self {
            Self::NotVisible(error) | Self::Visible(error) => error,
        }
    }
}

fn persist(path: &Path, pages: &[Page]) -> Result<File, PersistError> {
    let (tmp_path, mut file) = create_temp(path)?;
    let write_result = (|| {
        for page in pages {
            file.write_all(page.as_slice())
                .map_err(|error| write_error(path, &error))?;
        }
        file.sync_all().map_err(|error| write_error(path, &error))?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(PersistError::NotVisible(error));
    }
    // The temporary directory entry has to be durable before the rename, on
    // filesystems that can sync a directory. A failure here is still before
    // the destination changes.
    if let Err(error) = sync_parent(path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(PersistError::NotVisible(error));
    }
    if let Err(error) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(PersistError::NotVisible(write_error(path, &error)));
    }
    // The replacement is now the database. Syncing the parent publishes that
    // directory entry for a later process crash. It is not a power-loss
    // barrier, and a failure here must not pretend the rename did not happen.
    let _ = sync_parent(path);
    match open_database(path) {
        Ok(file) => Ok(file),
        Err(error) => match open_database(path) {
            Ok(file) => Ok(file),
            Err(_) => Err(PersistError::Visible(error)),
        },
    }
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A new temporary path in the same directory as `destination`.
fn temporary_path(destination: &Path) -> PathBuf {
    let directory = match destination.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let file_name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rustdb".to_string());
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    directory.join(format!(
        ".{file_name}.{}.{sequence}.tmp",
        std::process::id()
    ))
}

fn create_temp(destination: &Path) -> Result<(PathBuf, File), PersistError> {
    for _ in 0..32 {
        let path = temporary_path(destination);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PersistError::NotVisible(write_error(destination, &error)));
            }
        }
    }
    Err(PersistError::NotVisible(DbError::Storage(format!(
        "failed to write database file '{}': could not allocate a temporary name",
        destination.display()
    ))))
}

fn sync_parent(path: &Path) -> Result<(), DbError> {
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let dir = File::open(directory).map_err(|error| {
        DbError::Storage(format!(
            "failed to sync database directory '{}': {error}",
            directory.display()
        ))
    })?;
    dir.sync_all().map_err(|error| {
        DbError::Storage(format!(
            "failed to sync database directory '{}': {error}",
            directory.display()
        ))
    })
}

fn open_database(path: &Path) -> Result<File, DbError> {
    File::options()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| open_error(path, &error))
}

pub(crate) fn replace_file(path: &Path, pages: &[Page]) -> Result<(), DbError> {
    persist(path, pages)
        .map(|_| ())
        .map_err(PersistError::into_db_error)
}

fn open_error(path: &Path, error: &std::io::Error) -> DbError {
    DbError::Storage(format!(
        "failed to open database file '{}': {error}",
        path.display()
    ))
}

fn read_error(path: &Path, error: &std::io::Error) -> DbError {
    DbError::Storage(format!(
        "failed to read database file '{}': {error}",
        path.display()
    ))
}

fn write_error(path: &Path, error: &std::io::Error) -> DbError {
    DbError::Storage(format!(
        "failed to write database file '{}': {error}",
        path.display()
    ))
}

pub(crate) fn fresh_nonce() -> u64 {
    let ticks = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(1);
    let mixed = ticks ^ u64::from(std::process::id()).wrapping_shl(17);
    if mixed == 0 { 1 } else { mixed }
}

fn write_log(path: &Path, bytes: &[u8]) -> Result<File, DbError> {
    let mut file = File::create(path).map_err(|error| {
        DbError::Storage(format!(
            "failed to write database file '{}': {error}",
            path.display()
        ))
    })?;
    file.write_all(bytes).map_err(|error| {
        DbError::Storage(format!(
            "failed to write database file '{}': {error}",
            path.display()
        ))
    })?;
    Ok(file)
}

/// A failed write is a confirmed pre-commit failure only when the bytes left
/// behind are not a commit record. A complete or damaged record fences the
/// database and is left for recovery.
fn commit_write_error(path: &Path, error: DbError) -> StepError {
    let bytes = fs::read(path).unwrap_or_default();
    match wal::classify_log(&bytes) {
        wal::LogClass::Incomplete if bytes.is_empty() => StepError::BeforeCommit(error),
        wal::LogClass::Incomplete => match wal::truncate_log(path) {
            Ok(()) => StepError::BeforeCommit(error),
            Err(_) => ambiguous(error),
        },
        wal::LogClass::Committed | wal::LogClass::Damaged => ambiguous(error),
    }
}

fn ambiguous_sync() -> StepError {
    ambiguous(DbError::Storage(
        "injected failure while syncing the commit record; durability is unknown".to_string(),
    ))
}

fn ambiguous_sync_io(path: &Path, error: &std::io::Error) -> StepError {
    ambiguous(DbError::Storage(format!(
        "failed to sync redo log '{}'; durability of the commit record is unknown: {error}",
        path.display()
    )))
}

fn ambiguous(error: DbError) -> StepError {
    StepError::AfterCommit(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir =
                std::env::temp_dir().join(format!("rustdb-pager-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scratch_temps(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                let name = path.file_name()?.to_string_lossy().into_owned();
                if name.starts_with('.') && name.ends_with(".tmp") {
                    Some(path)
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn temporary_files_stay_in_the_database_directory_and_do_not_truncate() {
        let dir = TempDir::new();
        let destination = dir.0.join("demo.db");
        let sibling = dir.0.join("demo.db.tmp");
        fs::write(&sibling, b"keep-me").unwrap();

        let generated = temporary_path(&destination);
        assert_eq!(generated.parent(), Some(dir.0.as_path()));

        let (first, _) = create_temp(&destination).unwrap();
        let (second, _) = create_temp(&destination).unwrap();
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(dir.0.as_path()));
        assert!(first.exists());
        assert!(second.exists());
        assert_eq!(fs::read(&sibling).unwrap(), b"keep-me");
        fs::remove_file(&first).unwrap();
        fs::remove_file(&second).unwrap();

        persist(&destination, &initial_pages().unwrap()).unwrap();
        assert_eq!(fs::read(&sibling).unwrap(), b"keep-me");
        assert!(scratch_temps(&dir.0).is_empty());
        assert_eq!(Pager::open(&destination).unwrap().page_count(), 2);
    }

    #[test]
    fn a_leftover_temp_is_not_opened_as_the_database() {
        let dir = TempDir::new();
        let destination = dir.0.join("demo.db");
        persist(&destination, &initial_pages().unwrap()).unwrap();
        fs::write(dir.0.join(".demo.db.crash.tmp"), vec![7u8; 64]).unwrap();
        let pager = Pager::open(&destination).unwrap();
        assert_eq!(pager.page_count(), 2);
    }

    #[test]
    fn rename_failure_leaves_the_destination_and_removes_the_temp() {
        let dir = TempDir::new();
        let destination = dir.0.join("demo.db");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), b"x").unwrap();
        let error = persist(&destination, &initial_pages().unwrap()).unwrap_err();
        assert!(matches!(error, PersistError::NotVisible(_)));
        assert_eq!(fs::read(destination.join("keep")).unwrap(), b"x");
        assert!(scratch_temps(&dir.0).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_replace_keeps_the_previous_file_bytes() {
        use std::os::unix::fs::PermissionsExt;

        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Ok(metadata) = fs::metadata(&self.0) {
                    let mut permissions = metadata.permissions();
                    permissions.set_mode(0o755);
                    let _ = fs::set_permissions(&self.0, permissions);
                }
            }
        }

        let dir = TempDir::new();
        let destination = dir.0.join("demo.db");
        let pages = initial_pages().unwrap();
        persist(&destination, &pages).unwrap();
        let before = fs::read(&destination).unwrap();
        let _restore = Restore(dir.0.clone());
        let mut permissions = fs::metadata(&dir.0).unwrap().permissions();
        permissions.set_mode(0o555);
        fs::set_permissions(&dir.0, permissions).unwrap();

        let error = persist(&destination, &pages).unwrap_err();
        assert!(matches!(error, PersistError::NotVisible(_)));
        assert_eq!(fs::read(&destination).unwrap(), before);
        assert!(scratch_temps(&dir.0).is_empty());
    }

    #[test]
    fn allocate_does_not_wrap_the_page_id() {
        let mut image = Image {
            pages: Vec::new(),
            next_id: u32::MAX,
            reusable: Vec::new(),
            allocated: Vec::new(),
        };
        let error = image.allocate().unwrap_err();
        assert_eq!(error.to_string(), "database page id space is exhausted");
        assert!(image.allocated.is_empty());
        assert_eq!(image.next_id, u32::MAX);
    }
}
