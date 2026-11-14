//! Reads and publishes fixed-size pages.
//!
//! A commit writes a complete image to a uniquely named temporary file in the
//! same directory as the database, syncs the file and that directory, then
//! renames it over the database. The previous file is left untouched until
//! that rename. A failure before the rename does not replace the database.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::DbError;
use crate::storage::page::{self, PAGE_SIZE, Page};

const PAGE_SIZE_U64: u64 = PAGE_SIZE as u64;

pub(crate) struct Pager {
    path: PathBuf,
    file: File,
    page_count: u32,
    /// Free data-page ids in the committed image. Highest id is reused first.
    reusable: Vec<u32>,
    cache: Vec<Option<Page>>,
    /// True after `rename` has replaced the database for the current publish.
    publish_visible: bool,
    /// The open handle still refers to the inode from before that rename.
    reopen_required: bool,
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
        if !path.exists()
            || fs::metadata(path)
                .map_err(|error| open_error(path, &error))?
                .len()
                == 0
        {
            let pages = initial_pages()?;
            return Self::create(path, pages);
        }

        let file = File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| open_error(path, &error))?;
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
            reusable: Vec::new(),
            cache: Vec::new(),
            publish_visible: false,
            reopen_required: false,
        };
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

    pub(crate) fn set_reusable(&mut self, mut reusable: Vec<u32>) {
        reusable.sort_unstable();
        reusable.dedup();
        self.reusable = reusable;
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

    pub(crate) fn begin_image(&self) -> Image {
        Image {
            pages: Vec::new(),
            next_id: self.page_count,
            reusable: self.reusable.clone(),
            allocated: Vec::new(),
        }
    }

    pub(crate) fn begin_publish(&mut self) {
        self.publish_visible = false;
    }

    pub(crate) fn publication_visible(&self) -> bool {
        self.publish_visible
    }

    /// Replace the database file with `pages` and remember `reusable`.
    ///
    /// `publication_visible` stays false until `rename` has replaced the
    /// destination. A failure before that leaves the existing file alone. A
    /// failure after it keeps the new page count so memory can stay with the
    /// file that is now visible.
    pub(crate) fn commit(&mut self, pages: Vec<Page>, reusable: Vec<u32>) -> Result<(), DbError> {
        self.publish_visible = false;
        let page_count = u32::try_from(pages.len())
            .map_err(|_| DbError::Internal("published page count does not fit".to_string()))?;
        match persist(&self.path, &pages) {
            Ok(file) => {
                self.publish_visible = true;
                self.file = file;
                self.page_count = page_count;
                self.reusable = reusable;
                self.cache.clear();
                self.reopen_required = false;
                Ok(())
            }
            Err(PersistError::NotVisible(error)) => Err(error),
            Err(PersistError::Visible(error)) => {
                self.publish_visible = true;
                self.page_count = page_count;
                self.reusable = reusable;
                self.cache.clear();
                self.reopen_required = true;
                Err(error)
            }
        }
    }

    fn create(path: &Path, pages: Vec<Page>) -> Result<Self, DbError> {
        let file = persist(path, &pages).map_err(PersistError::into_db_error)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            page_count: u32::try_from(pages.len())
                .map_err(|_| DbError::Internal("published page count does not fit".to_string()))?,
            reusable: Vec::new(),
            cache: Vec::new(),
            publish_visible: false,
            reopen_required: false,
        })
    }

    fn load(&mut self, id: u32) -> Result<(), DbError> {
        if self.reopen_required {
            self.file = File::options()
                .read(true)
                .write(true)
                .open(&self.path)
                .map_err(|error| open_error(&self.path, &error))?;
            self.reopen_required = false;
        }
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

impl Image {
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

fn initial_pages() -> Result<Vec<Page>, DbError> {
    let mut meta = Page::new();
    page::write_meta(
        &mut meta,
        page::Meta {
            page_count: 2,
            catalog_root: 1,
            freelist_root: 0,
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
