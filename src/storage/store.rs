//! Catalog, heap images, and incremental commits.
//!
//! A statement edits only the pages it changes. Those pages are written to a
//! redo log and synced before the database file is modified. The log is
//! cleared only after the database file itself has been synced.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::error::DbError;
use crate::schema::{Column, Schema};
use crate::storage::btree::{self, Nodes};
use crate::storage::edit::{Editor, LiveIndex, LiveTable, Snapshot};
use crate::storage::page::{self, InsertError, PAGE_SIZE, Page};
use crate::storage::pager::{Image, Pager};
use crate::storage::profile;
use crate::storage::record::{
    self, CatalogEntry, Link, NULL_LINK, StoredRecord, ensure_record_fits,
};
use crate::storage::wal::{self, Inject, StepError};
use crate::value::{DataType, Value};

const FREE_DIRECTORY_CAPACITY: usize = (PAGE_SIZE - 28) / 4;

/// One integer index the editor should keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexImage {
    pub name: String,
    pub table: String,
    pub column: String,
}

/// One table, in the order its rows should be stored.
#[derive(Clone, Debug)]
pub(crate) struct TableImage {
    pub name: String,
    pub columns: Vec<Column>,
    /// Zero asks a full rewrite to assign `index + 1`. A loaded table keeps
    /// the id stored in its catalog cell.
    pub table_id: u32,
    pub next_row_id: u64,
    pub rows: Vec<(u64, Vec<Value>)>,
}

pub(crate) struct Store {
    pager: Pager,
    state: Snapshot,
    fence: Option<DbError>,
    inject: Option<Inject>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Store")
            .field("path", &self.pager.path())
            .finish()
    }
}

impl Store {
    pub(crate) fn open(path: &Path) -> Result<(Self, Vec<TableImage>), DbError> {
        let mut pager = Pager::open(path)?;
        let (images, state) = load(&mut pager, true)?;
        Ok((
            Self {
                pager,
                state,
                fence: None,
                inject: None,
            },
            images,
        ))
    }

    /// Replace a version 1 file with version 2, or a version 2 file with version 3.
    ///
    /// The source version selects the target. A version 3 file is left unchanged
    /// and this returns an error. Migration copies the bytes through a temporary
    /// file and renames it only after the new image is checked.
    pub(crate) fn migrate(path: &Path) -> Result<(), DbError> {
        if !path.exists() {
            return Err(DbError::Storage(format!(
                "database file '{}' is not format version 1",
                path.display()
            )));
        }
        refuse_migration_wal(path)?;
        let version = file_version(path)?;
        match version {
            page::FORMAT_VERSION_V1 => migrate_v1(path),
            page::FORMAT_VERSION => migrate_v2(path),
            page::FORMAT_VERSION_V3 => Err(DbError::Storage(
                "database file is already format version 3".to_string(),
            )),
            other => Err(DbError::Storage(format!(
                "unsupported database format version {other}"
            ))),
        }
    }

    pub(crate) fn fenced(&self) -> Option<DbError> {
        self.fence.clone()
    }

    pub(crate) fn format_version(&self) -> u32 {
        self.state.version
    }

    pub(crate) fn path(&self) -> &Path {
        self.pager.path()
    }

    pub(crate) fn indexes(&self) -> Vec<IndexImage> {
        self.state
            .indexes
            .iter()
            .filter_map(|index| {
                let table = self
                    .state
                    .tables
                    .iter()
                    .find(|(_, table)| table.table_id == index.table_id)?
                    .0
                    .clone();
                Some(IndexImage {
                    name: index.name.clone(),
                    table,
                    column: index.column_name.clone(),
                })
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn set_inject(&mut self, inject: Inject) {
        self.inject = Some(inject);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn publish(&mut self, tables: &[TableImage]) -> Result<(), StepError> {
        self.publish_indexes(tables, &[])
    }

    pub(crate) fn publish_indexes(
        &mut self,
        tables: &[TableImage],
        indexes: &[IndexImage],
    ) -> Result<(), StepError> {
        if let Some(error) = &self.fence {
            return Err(StepError::AfterCommit(error.clone()));
        }
        let mut ordered: Vec<&TableImage> = tables.iter().collect();
        ordered.sort_by(|left, right| left.name.cmp(&right.name));
        validate(&ordered).map_err(StepError::BeforeCommit)?;
        let owned: Vec<TableImage> = ordered.into_iter().cloned().collect();
        let base_commit = self.state.commit_id;
        let nonce = self.state.nonce;
        let apply_mark = profile::start();
        let prepared = Editor::new(self.state.clone())
            .apply(&mut self.pager, &owned, indexes)
            .map_err(StepError::BeforeCommit)?;
        profile::add_apply(apply_mark);
        let Some(prepared) = prepared else {
            return Ok(());
        };
        match self.pager.checkpoint(
            prepared.pages,
            prepared.page_count,
            nonce,
            base_commit,
            self.inject.take(),
        ) {
            Ok(()) => {
                self.state = prepared.snapshot;
                Ok(())
            }
            Err(StepError::BeforeCommit(error)) => Err(StepError::BeforeCommit(error)),
            Err(StepError::AfterCommit(error)) => {
                let fenced = DbError::Storage(format!(
                    "database checkpoint did not finish; the outcome is unknown and this is not a confirmed rollback. Reopen the file before running another statement. A retry after recovery can duplicate a row ({error})"
                ));
                self.fence = Some(fenced.clone());
                Err(StepError::AfterCommit(fenced))
            }
        }
    }
}

fn refuse_migration_wal(path: &Path) -> Result<(), DbError> {
    let wal_path = wal::path_for(path);
    if wal_path.exists()
        && fs::metadata(&wal_path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(true)
    {
        return Err(DbError::Storage(
            "refusing to migrate a database that has a redo log".to_string(),
        ));
    }
    Ok(())
}

fn file_version(path: &Path) -> Result<u32, DbError> {
    let mut file = fs::File::open(path).map_err(|error| {
        DbError::Storage(format!(
            "failed to read database file '{}': {error}",
            path.display()
        ))
    })?;
    let mut page = Page::new();
    std::io::Read::read_exact(&mut file, page.as_mut_slice()).map_err(|error| {
        DbError::Storage(format!(
            "failed to read database file '{}': {error}",
            path.display()
        ))
    })?;
    Ok(page::read_meta(&page)?.version)
}

fn migrate_v1(path: &Path) -> Result<(), DbError> {
    let mut pager = Pager::open_v1(path)?;
    let (images, _) = load(&mut pager, false)?;
    let mut ordered: Vec<&TableImage> = images.iter().collect();
    ordered.sort_by(|left, right| left.name.cmp(&right.name));
    validate(&ordered)?;
    let pages = build_v2_image(&ordered, crate::storage::pager::fresh_nonce())?;
    crate::storage::pager::replace_file(path, &pages)?;
    drop(pager);
    Ok(())
}

fn migrate_v2(path: &Path) -> Result<(), DbError> {
    let mut pager = Pager::open(path)?;
    let (_images, snapshot) = load(&mut pager, true)?;
    if snapshot.version != page::FORMAT_VERSION {
        return Err(DbError::Storage(
            "database file is not format version 2".to_string(),
        ));
    }
    let mut pages = Vec::with_capacity(usize::try_from(snapshot.page_count).unwrap_or(0));
    for id in 0..snapshot.page_count {
        let mut page = Page::new();
        pager.read_into(id, &mut page)?;
        pages.push(page);
    }
    let capacity = page::META_FREE_CAPACITY_V3;
    let mut inline = snapshot.inline_free.clone();
    let mut page_count = snapshot.page_count;
    let mut freelist_root = snapshot.freelist_root;
    if inline.len() > capacity {
        let overflow = inline.split_off(capacity);
        let directory_id = page_count;
        page_count = page_count
            .checked_add(1)
            .ok_or_else(|| DbError::Storage("database page id space is exhausted".to_string()))?;
        let mut directory = Page::new();
        page::write_free_directory(&mut directory, directory_id, freelist_root, &overflow)?;
        page::seal(&mut directory);
        page::verify(&directory, directory_id)?;
        pages.push(directory);
        freelist_root = directory_id;
    }
    let mut meta = Page::new();
    page::write_meta(
        &mut meta,
        page::Meta {
            version: page::FORMAT_VERSION_V3,
            page_count,
            catalog_root: snapshot.catalog_root,
            freelist_root,
            nonce: snapshot.nonce,
            commit_id: snapshot.commit_id,
            next_table_id: snapshot.next_table_id,
            index_catalog_root: 0,
        },
        &inline,
    )?;
    page::seal(&mut meta);
    page::verify(&meta, 0)?;
    let checked = page::read_meta(&meta)?;
    if checked.version != page::FORMAT_VERSION_V3
        || checked.nonce != snapshot.nonce
        || checked.commit_id != snapshot.commit_id
        || checked.index_catalog_root != 0
        || checked.page_count != page_count
    {
        return Err(page::corrupt_file());
    }
    pages[0] = meta;
    for (id, page) in pages.iter().enumerate() {
        page::verify(page, u32::try_from(id).unwrap_or(u32::MAX))?;
    }
    crate::storage::pager::replace_file(path, &pages)?;
    drop(pager);
    Ok(())
}

fn validate(tables: &[&TableImage]) -> Result<(), DbError> {
    let mut names = HashSet::with_capacity(tables.len());
    for table in tables {
        if table.name.is_empty() || !names.insert(table.name.as_str()) {
            return Err(DbError::Internal(
                "cannot publish a catalog with a missing or duplicate table name".to_string(),
            ));
        }
        if table.next_row_id == 0 {
            return Err(DbError::Internal(
                "table row-id counter must start at 1".to_string(),
            ));
        }
        let mut ids = HashSet::with_capacity(table.rows.len());
        for (row_id, values) in &table.rows {
            if *row_id == 0 || *row_id >= table.next_row_id || !ids.insert(*row_id) {
                return Err(DbError::Internal(format!(
                    "row id {row_id} is outside the counter for table '{}'",
                    table.name
                )));
            }
            if values.len() != table.columns.len() {
                return Err(DbError::Internal(format!(
                    "row in '{}' does not match its schema",
                    table.name
                )));
            }
            for (value, column) in values.iter().zip(&table.columns) {
                if let Some(actual) = value.data_type()
                    && actual != column.data_type()
                {
                    return Err(DbError::Internal(format!(
                        "row in '{}' has a value that does not match its column",
                        table.name
                    )));
                }
            }
            ensure_record_fits(values)?;
        }
    }
    Ok(())
}

fn write_heaps(image: &mut Image, tables: &[&TableImage]) -> Result<Vec<Link>, DbError> {
    let mut heads = Vec::with_capacity(tables.len());
    for table in tables {
        if table.rows.is_empty() {
            heads.push(NULL_LINK);
            continue;
        }
        let mut placed = Vec::with_capacity(table.rows.len());
        let mut page_id = image.allocate()?;
        let mut page = heap_page(page_id);
        for (row_id, values) in &table.rows {
            let bytes = record::encode_record(&StoredRecord {
                row_id: *row_id,
                prev: NULL_LINK,
                next: NULL_LINK,
                values: values.clone(),
            })?;
            match page::insert_cell(&mut page, &bytes) {
                Ok(slot) => placed.push((page_id, slot)),
                Err(InsertError::Full) => {
                    image.write(page_id, page)?;
                    page_id = image.allocate()?;
                    page = heap_page(page_id);
                    let slot = insert_on_empty_page(&mut page, &bytes)?;
                    placed.push((page_id, slot));
                }
                Err(InsertError::TooBig) => return Err(record_too_big()),
            }
        }
        image.write(page_id, page)?;
        for index in 0..placed.len() {
            let prev = if index == 0 {
                NULL_LINK
            } else {
                link(placed[index - 1])
            };
            let next = if index + 1 == placed.len() {
                NULL_LINK
            } else {
                link(placed[index + 1])
            };
            let (page_id, slot) = placed[index];
            let cell = page::cell_mut(image.page_mut(page_id)?, slot)?;
            record::patch_links(cell, prev, next)?;
        }
        heads.push(link(placed[0]));
    }
    Ok(heads)
}

fn write_catalog(
    image: &mut Image,
    tables: &[&TableImage],
    heads: &[Link],
) -> Result<u32, DbError> {
    let mut entries = Vec::with_capacity(tables.len());
    for (index, table) in tables.iter().enumerate() {
        let table_id = if table.table_id != 0 {
            table.table_id
        } else {
            u32::try_from(index + 1).map_err(|_| page::corrupt_file())?
        };
        entries.push(record::encode_catalog(
            table_id,
            table.next_row_id,
            heads[index],
            &table.name,
            &table.columns,
        )?);
    }
    if entries.is_empty() {
        let page_id = image.allocate()?;
        image.write(page_id, catalog_page(page_id))?;
        return Ok(page_id);
    }

    let mut page_id = image.allocate()?;
    let mut page = catalog_page(page_id);
    let root = page_id;
    for entry in &entries {
        match page::insert_cell(&mut page, entry) {
            Ok(_) => {}
            Err(InsertError::Full) => {
                let previous = page_id;
                image.write(previous, page)?;
                page_id = image.allocate()?;
                page = catalog_page(page_id);
                page::set_next_page(image.page_mut(previous)?, page_id);
                insert_on_empty_page(&mut page, entry).map_err(|_| definition_too_big())?;
            }
            Err(InsertError::TooBig) => return Err(definition_too_big()),
        }
    }
    image.write(page_id, page)?;
    Ok(root)
}

fn write_freelist(image: &mut Image) -> Result<(u32, Vec<u32>, Vec<u32>), DbError> {
    let mut free_ids = image.free_ids();
    let directory_count = directory_pages_needed(free_ids.len());
    if directory_count > free_ids.len() {
        return Err(DbError::Internal(
            "cannot reserve freelist directory pages".to_string(),
        ));
    }
    let directory_ids: Vec<u32> = free_ids.drain(free_ids.len() - directory_count..).collect();
    let inline_len = free_ids.len().min(page::META_FREE_CAPACITY);
    let inline = free_ids[..inline_len].to_vec();
    let overflow = free_ids[inline_len..].to_vec();
    if directory_ids.is_empty() {
        return Ok((0, inline, free_ids));
    }
    let chunks: Vec<&[u32]> = overflow.chunks(FREE_DIRECTORY_CAPACITY).collect();
    if chunks.len() > directory_ids.len() {
        return Err(DbError::Internal(
            "freelist directory count does not match the free-page list".to_string(),
        ));
    }
    for (index, directory_id) in directory_ids.iter().copied().enumerate() {
        let next = directory_ids.get(index + 1).copied().unwrap_or(0);
        let chunk = chunks.get(index).copied().unwrap_or(&[]);
        let mut page = Page::new();
        page::write_free_directory(&mut page, directory_id, next, chunk)?;
        image.write(directory_id, page)?;
    }
    Ok((directory_ids[0], inline, free_ids))
}

fn directory_pages_needed(free_count: usize) -> usize {
    if free_count <= page::META_FREE_CAPACITY {
        0
    } else {
        let extra = free_count - page::META_FREE_CAPACITY;
        extra.div_ceil(FREE_DIRECTORY_CAPACITY + 1)
    }
}

fn build_v2_image(tables: &[&TableImage], nonce: u64) -> Result<Vec<Page>, DbError> {
    let mut image = Image::fresh();
    let heads = write_heaps(&mut image, tables)?;
    let catalog_root = write_catalog(&mut image, tables, &heads)?;
    let (freelist_root, inline_free, _) = write_freelist(&mut image)?;
    let next_table_id = tables
        .iter()
        .enumerate()
        .map(|(index, table)| {
            if table.table_id != 0 {
                Ok(table.table_id)
            } else {
                u32::try_from(index + 1).map_err(|_| page::corrupt_file())
            }
        })
        .collect::<Result<Vec<_>, DbError>>()?
        .into_iter()
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| DbError::Storage("table id space is exhausted".to_string()))?;
    let mut meta = Page::new();
    page::write_meta(
        &mut meta,
        page::Meta {
            version: page::FORMAT_VERSION,
            page_count: image.next_id(),
            catalog_root,
            freelist_root,
            nonce,
            commit_id: 0,
            next_table_id,
            index_catalog_root: 0,
        },
        &inline_free,
    )?;
    image.write(0, meta)?;
    image.into_pages()
}

fn load(pager: &mut Pager, strict: bool) -> Result<(Vec<TableImage>, Snapshot), DbError> {
    let mut meta_page = Page::new();
    pager.read_into(0, &mut meta_page)?;
    let meta = page::read_meta(&meta_page)?;
    if meta.page_count != pager.page_count()
        || meta.catalog_root == 0
        || meta.catalog_root >= meta.page_count
    {
        return Err(page::corrupt_file());
    }

    let mut free_ids = page::inline_free_ids(&meta_page)?;
    let (directory_pages, more) = read_freelist(pager, meta.freelist_root, meta.page_count)?;
    free_ids.extend(more);
    validate_free_ids(&free_ids, meta.page_count)?;
    let (catalog_pages, entries) = read_catalog(pager, meta.catalog_root, meta.page_count)?;
    let mut images = Vec::with_capacity(entries.len());
    let mut tables = HashMap::with_capacity(entries.len());
    let mut heap_slots: HashMap<u32, HashSet<u16>> = HashMap::new();
    let mut heap_owner: HashMap<u32, u32> = HashMap::new();
    let mut names = HashSet::with_capacity(entries.len());
    let mut table_ids = HashSet::with_capacity(entries.len());
    let mut highest_table_id = 0u32;
    for placed in entries {
        let PlacedCatalog {
            page: catalog_page,
            slot: catalog_slot,
            entry,
        } = placed;
        if entry.name.is_empty()
            || entry.table_id == 0
            || !names.insert(entry.name.clone())
            || !table_ids.insert(entry.table_id)
        {
            return Err(page::corrupt_file());
        }
        highest_table_id = highest_table_id.max(entry.table_id);
        let columns = Schema::try_new(&entry.name, entry.columns.clone())
            .map_err(|_| page::corrupt_file())?
            .columns()
            .to_vec();
        let walked = walk_rows(
            pager,
            entry.head,
            &columns,
            meta.page_count,
            entry.table_id,
            &mut heap_slots,
            &mut heap_owner,
        )?;
        let rows = walked.rows;
        let links = walked.links;
        if entry.next_row_id == 0
            || rows
                .iter()
                .any(|(row_id, _)| *row_id == 0 || *row_id >= entry.next_row_id)
        {
            return Err(page::corrupt_file());
        }
        let mut seen = HashSet::with_capacity(rows.len());
        if rows.iter().any(|(row_id, _)| !seen.insert(*row_id)) {
            return Err(page::corrupt_file());
        }
        let row_ids: Vec<u64> = rows.iter().map(|(row_id, _)| *row_id).collect();
        tables.insert(
            entry.name.clone(),
            LiveTable {
                table_id: entry.table_id,
                catalog_page,
                catalog_slot,
                head: entry.head,
                tail: links.last().copied().unwrap_or(NULL_LINK),
                next_row_id: entry.next_row_id,
                row_ids,
                links,
            },
        );
        images.push(TableImage {
            name: entry.name,
            columns,
            table_id: entry.table_id,
            next_row_id: entry.next_row_id,
            rows,
        });
    }
    for (page_id, slots) in &heap_slots {
        let mut page = Page::new();
        pager.read_into(*page_id, &mut page)?;
        let live = page::live_slots(&page)?;
        if live.len() != slots.len() || live.iter().any(|slot| !slots.contains(slot)) {
            return Err(page::corrupt(*page_id));
        }
    }

    let loaded_indexes = if meta.version >= page::FORMAT_VERSION_V3 {
        load_indexes(
            pager,
            meta.index_catalog_root,
            meta.page_count,
            &images,
            &free_ids,
        )?
    } else if meta.index_catalog_root != 0 {
        return Err(page::corrupt_file());
    } else {
        LoadedIndexes::default()
    };
    let index_pages = loaded_indexes.pages;
    let indexes = loaded_indexes.indexes;
    let mut live = HashSet::new();
    live.insert(0);
    live.extend(catalog_pages.iter().copied());
    live.extend(directory_pages.iter().copied());
    live.extend(heap_slots.keys().copied());
    live.extend(index_pages.iter().copied());
    if catalog_pages.iter().any(|id| heap_slots.contains_key(id))
        || directory_pages
            .iter()
            .any(|id| heap_slots.contains_key(id) || catalog_pages.contains(id))
        || index_pages.iter().any(|id| {
            heap_slots.contains_key(id)
                || catalog_pages.contains(id)
                || directory_pages.contains(id)
                || *id == 0
        })
    {
        return Err(page::corrupt_file());
    }
    let free_set: HashSet<u32> = free_ids.iter().copied().collect();
    if live.iter().any(|id| free_set.contains(id)) {
        return Err(page::corrupt_file());
    }
    if strict {
        for id in 1..meta.page_count {
            if !live.contains(&id) && !free_set.contains(&id) {
                return Err(page::corrupt_file());
            }
        }
        for id in &free_ids {
            let mut page = Page::new();
            pager.read_into(*id, &mut page)?;
            if page::page_type_of(&page) != page::TYPE_FREE {
                return Err(page::corrupt(*id));
            }
        }
    }
    let inline_free = page::inline_free_ids(&meta_page)?;
    let mut data_free = free_ids;
    data_free.sort_unstable();
    let floor = highest_table_id
        .checked_add(1)
        .ok_or_else(|| DbError::Storage("table id space is exhausted".to_string()))?;
    let next_table_id = if meta.next_table_id == 0 {
        floor
    } else if meta.next_table_id < floor {
        return Err(page::corrupt_file());
    } else {
        meta.next_table_id
    };
    let snapshot = Snapshot {
        version: meta.version,
        nonce: meta.nonce,
        commit_id: meta.commit_id,
        page_count: meta.page_count,
        catalog_root: meta.catalog_root,
        freelist_root: meta.freelist_root,
        index_catalog_root: meta.index_catalog_root,
        inline_free,
        free_ids: data_free,
        directory_pages,
        tables,
        indexes,
        catalog_tail: catalog_pages.last().copied().unwrap_or(meta.catalog_root),
        index_catalog_tail: loaded_indexes.catalog_tail,
        next_table_id,
    };
    Ok((images, snapshot))
}

#[derive(Default)]
struct LoadedIndexes {
    pages: Vec<u32>,
    catalog_tail: u32,
    indexes: Vec<LiveIndex>,
}

struct PagerNodes<'a> {
    pager: &'a mut Pager,
}

impl Nodes for PagerNodes<'_> {
    fn read_node(&mut self, id: u32) -> Result<Page, DbError> {
        let mut page = Page::new();
        self.pager.read_into(id, &mut page)?;
        Ok(page)
    }

    fn write_node(&mut self, id: u32, _page: Page) -> Result<(), DbError> {
        Err(DbError::Internal(format!(
            "validation cannot write page {id}"
        )))
    }

    fn alloc_node(&mut self) -> Result<u32, DbError> {
        Err(DbError::Internal(
            "validation cannot allocate a page".to_string(),
        ))
    }

    fn free_node(&mut self, id: u32) -> Result<(), DbError> {
        Err(DbError::Internal(format!(
            "validation cannot free page {id}"
        )))
    }
}

fn load_indexes(
    pager: &mut Pager,
    root: u32,
    page_count: u32,
    images: &[TableImage],
    free_ids: &[u32],
) -> Result<LoadedIndexes, DbError> {
    if root == 0 {
        return Ok(LoadedIndexes::default());
    }
    let (catalog_pages, entries) = read_index_catalog(pager, root, page_count)?;
    let mut seen_pages: HashSet<u32> = catalog_pages.iter().copied().collect();
    let mut loaded = LoadedIndexes {
        pages: catalog_pages.clone(),
        catalog_tail: catalog_pages.last().copied().unwrap_or(0),
        indexes: Vec::with_capacity(entries.len()),
    };
    let mut names = HashSet::new();
    for placed in entries {
        if !names.insert(placed.entry.name.clone()) {
            return Err(page::corrupt_file());
        }
        let Some(image) = images
            .iter()
            .find(|table| table.table_id == placed.entry.table_id)
        else {
            return Err(page::corrupt_file());
        };
        let Some(column) = image.columns.get(usize::from(placed.entry.ordinal)) else {
            return Err(page::corrupt_file());
        };
        if column.name() != placed.entry.column || column.data_type() != DataType::Integer {
            return Err(page::corrupt_file());
        }
        let mut nodes = PagerNodes { pager };
        let found = btree::validate(&mut nodes, placed.entry.root, btree::production())?;
        let owned = btree::owned_pages(&mut nodes, placed.entry.root)?;
        for id in &owned {
            if *id == 0 || *id >= page_count || !seen_pages.insert(*id) || free_ids.contains(id) {
                return Err(page::corrupt(*id));
            }
            loaded.pages.push(*id);
        }
        let mut expected = HashMap::new();
        for (row_id, values) in &image.rows {
            match values.get(usize::from(placed.entry.ordinal)) {
                Some(Value::Null) => {}
                Some(Value::Integer(key)) => {
                    expected.insert(*row_id, *key);
                }
                _ => return Err(page::corrupt_file()),
            }
        }
        if found.len() != expected.len() {
            return Err(page::corrupt_file());
        }
        for (key, row_id) in found {
            match expected.remove(&row_id) {
                Some(stored) if stored == key => {}
                _ => return Err(page::corrupt_file()),
            }
        }
        loaded.indexes.push(LiveIndex {
            name: placed.entry.name,
            table_id: placed.entry.table_id,
            column_ordinal: placed.entry.ordinal,
            column_name: placed.entry.column,
            root: placed.entry.root,
            catalog_page: placed.page,
            catalog_slot: placed.slot,
        });
    }
    Ok(loaded)
}

struct PlacedIndex {
    page: u32,
    slot: u16,
    entry: record::IndexCatalog,
}

fn read_index_catalog(
    pager: &mut Pager,
    root: u32,
    page_count: u32,
) -> Result<(Vec<u32>, Vec<PlacedIndex>), DbError> {
    let mut page_ids = Vec::new();
    let mut entries = Vec::new();
    let mut page_id = root;
    while page_id != 0 {
        if page_id >= page_count
            || page_ids.contains(&page_id)
            || page_ids.len() as u32 >= page_count
        {
            return Err(page::corrupt_file());
        }
        page_ids.push(page_id);
        let mut page = Page::new();
        pager.read_into(page_id, &mut page)?;
        if page::page_type_of(&page) != page::TYPE_INDEX_CATALOG {
            return Err(page::corrupt(page_id));
        }
        for slot in page::live_slots(&page)? {
            let bytes = page::cell(&page, slot)?.to_vec();
            let entry = record::decode_index(&bytes).map_err(|error| {
                if error.to_string().contains("unsupported index format") {
                    error
                } else {
                    page::corrupt(page_id)
                }
            })?;
            entries.push(PlacedIndex {
                page: page_id,
                slot,
                entry,
            });
        }
        page_id = page::next_page_of(&page);
    }
    Ok((page_ids, entries))
}

fn validate_free_ids(ids: &[u32], page_count: u32) -> Result<(), DbError> {
    let mut seen = HashSet::new();
    for id in ids {
        if *id == 0 || *id >= page_count || !seen.insert(*id) {
            return Err(page::corrupt_file());
        }
    }
    Ok(())
}

struct PlacedCatalog {
    page: u32,
    slot: u16,
    entry: CatalogEntry,
}

fn read_catalog(
    pager: &mut Pager,
    root: u32,
    page_count: u32,
) -> Result<(Vec<u32>, Vec<PlacedCatalog>), DbError> {
    let mut page_ids = Vec::new();
    let mut entries = Vec::new();
    let mut page_id = root;
    while page_id != 0 {
        if page_id >= page_count
            || page_ids.contains(&page_id)
            || page_ids.len() as u32 >= page_count
        {
            return Err(page::corrupt_file());
        }
        page_ids.push(page_id);
        let mut page = Page::new();
        pager.read_into(page_id, &mut page)?;
        if page::page_type_of(&page) != page::TYPE_CATALOG {
            return Err(page::corrupt(page_id));
        }
        for slot in page::live_slots(&page)? {
            let bytes = page::cell(&page, slot)?.to_vec();
            let entry = record::decode_catalog(&bytes).map_err(|_| page::corrupt(page_id))?;
            entries.push(PlacedCatalog {
                page: page_id,
                slot,
                entry,
            });
        }
        page_id = page::next_page_of(&page);
    }
    Ok((page_ids, entries))
}

fn read_freelist(
    pager: &mut Pager,
    root: u32,
    page_count: u32,
) -> Result<(Vec<u32>, Vec<u32>), DbError> {
    let mut directories = Vec::new();
    let mut data = Vec::new();
    let mut page_id = root;
    while page_id != 0 {
        if page_id >= page_count
            || directories.contains(&page_id)
            || directories.len() as u32 >= page_count
        {
            return Err(page::corrupt_file());
        }
        directories.push(page_id);
        let mut page = Page::new();
        pager.read_into(page_id, &mut page)?;
        let (next, ids) = page::read_free_directory(&page, page_id)?;
        for id in ids {
            if id == 0 || id >= page_count || data.contains(&id) || directories.contains(&id) {
                return Err(page::corrupt_file());
            }
            data.push(id);
        }
        page_id = next;
    }
    Ok((directories, data))
}

struct WalkedRows {
    rows: Vec<(u64, Vec<Value>)>,
    links: Vec<Link>,
}

fn walk_rows(
    pager: &mut Pager,
    head: Link,
    columns: &[Column],
    page_count: u32,
    table_id: u32,
    heap_slots: &mut HashMap<u32, HashSet<u16>>,
    heap_owner: &mut HashMap<u32, u32>,
) -> Result<WalkedRows, DbError> {
    let mut rows = Vec::new();
    let mut links = Vec::new();
    let mut current = head;
    let mut previous = NULL_LINK;
    let mut seen = HashSet::new();
    while current.page != 0 {
        if current.page >= page_count || !seen.insert((current.page, current.slot)) {
            return Err(page::corrupt_file());
        }
        if let Some(owner) = heap_owner.insert(current.page, table_id)
            && owner != table_id
        {
            return Err(page::corrupt_file());
        }
        let slots = heap_slots.entry(current.page).or_default();
        if !slots.insert(current.slot) {
            return Err(page::corrupt_file());
        }
        let mut page = Page::new();
        pager.read_into(current.page, &mut page)?;
        if page::page_type_of(&page) != page::TYPE_HEAP {
            return Err(page::corrupt(current.page));
        }
        let bytes = page::cell(&page, current.slot)?.to_vec();
        let record = record::decode_record(&bytes).map_err(|_| page::corrupt(current.page))?;
        if record.prev != previous || record.values.len() != columns.len() {
            return Err(page::corrupt_file());
        }
        for (value, column) in record.values.iter().zip(columns) {
            if let Some(actual) = value.data_type()
                && actual != column.data_type()
            {
                return Err(page::corrupt_file());
            }
        }
        links.push(current);
        previous = current;
        current = record.next;
        rows.push((record.row_id, record.values));
    }
    Ok(WalkedRows { rows, links })
}

fn heap_page(page_id: u32) -> Page {
    let mut page = Page::new();
    page::init_slotted(&mut page, page::TYPE_HEAP, page_id);
    page
}

fn catalog_page(page_id: u32) -> Page {
    let mut page = Page::new();
    page::init_slotted(&mut page, page::TYPE_CATALOG, page_id);
    page
}

fn link((page, slot): (u32, u16)) -> Link {
    Link { page, slot }
}

fn insert_on_empty_page(page: &mut Page, payload: &[u8]) -> Result<u16, DbError> {
    match page::insert_cell(page, payload) {
        Ok(slot) => Ok(slot),
        Err(InsertError::TooBig) => Err(record_too_big()),
        Err(InsertError::Full) => Err(record_too_big()),
    }
}

fn record_too_big() -> DbError {
    DbError::Storage(format!(
        "row exceeds the maximum record size of {} bytes",
        page::MAX_RECORD_LEN
    ))
}

fn definition_too_big() -> DbError {
    DbError::Storage(format!(
        "table definition exceeds the maximum record size of {} bytes",
        page::MAX_RECORD_LEN
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::DataType;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempPath(std::path::PathBuf);

    impl TempPath {
        fn new() -> Self {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "rustdb-store-{}-{}-{n}",
                std::process::id(),
                nanos
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir.join("test.db"))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            if let Some(parent) = self.0.parent() {
                let _ = fs::remove_dir_all(parent);
            }
        }
    }

    fn image(name: &str, rows: Vec<(u64, Vec<Value>)>, next_row_id: u64) -> TableImage {
        TableImage {
            name: name.to_string(),
            columns: vec![
                Column::new("id", DataType::Integer),
                Column::new("name", DataType::Text),
                Column::new("active", DataType::Boolean),
            ],
            next_row_id,
            table_id: 0,
            rows,
        }
    }

    #[test]
    fn preserves_row_ids_nulls_and_order_across_reopen() {
        let file = TempPath::new();
        let rows = vec![
            (
                1,
                vec![
                    Value::Integer(1),
                    Value::Text("Ada".to_string()),
                    Value::Boolean(true),
                ],
            ),
            (2, vec![Value::Integer(i64::MIN), Value::Null, Value::Null]),
        ];
        {
            let (mut store, loaded) = Store::open(file.path()).unwrap();
            assert!(loaded.is_empty());
            store.publish(&[image("users", rows.clone(), 3)]).unwrap();
        }
        let (mut store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "users");
        assert_eq!(loaded[0].next_row_id, 3);
        assert_eq!(loaded[0].rows, rows);

        let updated = vec![
            rows[0].clone(),
            (
                2,
                vec![
                    Value::Integer(9),
                    Value::Text("Bea".to_string()),
                    Value::Boolean(false),
                ],
            ),
        ];
        store
            .publish(&[image("users", updated.clone(), 3)])
            .unwrap();
        drop(store);
        let (mut store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows, updated);

        store
            .publish(&[image("users", vec![updated[1].clone()], 3)])
            .unwrap();
        drop(store);
        let (_, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows, vec![updated[1].clone()]);
        assert_eq!(loaded[0].rows[0].0, 2);
    }

    #[test]
    fn spans_pages_and_reuses_freed_pages() {
        let file = TempPath::new();
        let text = "x".repeat(1500);
        let rows: Vec<(u64, Vec<Value>)> = (1..=6)
            .map(|id| {
                (
                    id,
                    vec![
                        Value::Integer(id as i64),
                        Value::Text(text.clone()),
                        Value::Boolean(id % 2 == 0),
                    ],
                )
            })
            .collect();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store.publish(&[image("wide", rows.clone(), 7)]).unwrap();
        let wide_len = fs::metadata(file.path()).unwrap().len();
        assert!(wide_len > u64::try_from(PAGE_SIZE * 3).unwrap());

        store.publish(&[image("wide", Vec::new(), 7)]).unwrap();
        let empty_len = fs::metadata(file.path()).unwrap().len();
        store.publish(&[image("wide", rows.clone(), 7)]).unwrap();
        let reused_len = fs::metadata(file.path()).unwrap().len();
        assert!(reused_len <= empty_len);

        drop(store);
        let (_, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows, rows);
    }

    #[test]
    fn rejects_a_bad_version_and_a_truncated_file() {
        let file = TempPath::new();
        {
            let (store, _) = Store::open(file.path()).unwrap();
            drop(store);
        }
        let mut bytes = fs::read(file.path()).unwrap();
        bytes.truncate(100);
        fs::write(file.path(), &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert_eq!(error.to_string(), "database file is truncated");

        let file = TempPath::new();
        {
            let (store, _) = Store::open(file.path()).unwrap();
            drop(store);
        }
        let mut bytes = fs::read(file.path()).unwrap();
        bytes[32..36].copy_from_slice(&99u32.to_le_bytes());
        let mut page = Page::new();
        page.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        page::seal(&mut page);
        bytes[..PAGE_SIZE].copy_from_slice(page.as_slice());
        fs::write(file.path(), &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert_eq!(error.to_string(), "unsupported database format version 99");

        let file = TempPath::new();
        {
            let (store, _) = Store::open(file.path()).unwrap();
            drop(store);
        }
        let mut bytes = fs::read(file.path()).unwrap();
        bytes[24..32].copy_from_slice(b"NOTRUST!");
        let mut page = Page::new();
        page.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        page::seal(&mut page);
        bytes[..PAGE_SIZE].copy_from_slice(page.as_slice());
        fs::write(file.path(), &bytes).unwrap();
        let error = match Store::open(file.path()) {
            Err(error) => error,
            Ok(_) => panic!("bad magic opened"),
        };
        assert_eq!(error.to_string(), "invalid database header");

        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish(&[image(
                    "users",
                    vec![(
                        1,
                        vec![
                            Value::Integer(1),
                            Value::Text("Ada".to_string()),
                            Value::Boolean(true),
                        ],
                    )],
                    2,
                )])
                .unwrap();
        }
        let mut bytes = fs::read(file.path()).unwrap();
        bytes[80] ^= 0xFF;
        fs::write(file.path(), &bytes).unwrap();
        let error = match Store::open(file.path()) {
            Err(error) => error,
            Ok(_) => panic!("corrupt page opened"),
        };
        assert_eq!(error.to_string(), "database page 0 is corrupt");
    }

    #[test]
    fn rejects_a_duplicated_free_page_id() {
        let file = TempPath::new();
        {
            let (store, _) = Store::open(file.path()).unwrap();
            drop(store);
        }
        let mut bytes = fs::read(file.path()).unwrap();
        bytes[40..44].copy_from_slice(&4u32.to_le_bytes());
        bytes[52..56].copy_from_slice(&2u32.to_le_bytes());
        bytes[72..76].copy_from_slice(&2u32.to_le_bytes());
        bytes[76..80].copy_from_slice(&2u32.to_le_bytes());
        let mut meta = Page::new();
        meta.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        page::seal(&mut meta);
        bytes[..PAGE_SIZE].copy_from_slice(meta.as_slice());
        for id in [2, 3] {
            let mut free = Page::new();
            page::write_free_data(&mut free, id);
            page::seal(&mut free);
            bytes.extend_from_slice(free.as_slice());
        }
        fs::write(file.path(), &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert_eq!(error.to_string(), "database file is corrupt");
    }

    fn sample(id: i64, name: &str) -> Vec<Value> {
        vec![
            Value::Integer(id),
            Value::Text(name.to_string()),
            Value::Boolean(true),
        ]
    }

    fn read_meta(path: &Path) -> page::Meta {
        let bytes = fs::read(path).unwrap();
        let mut page = Page::new();
        page.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        page::read_meta(&page).unwrap()
    }

    fn heap_images(path: &Path) -> Vec<Vec<u8>> {
        fs::read(path)
            .unwrap()
            .chunks(PAGE_SIZE)
            .filter(|page| u16::from_le_bytes([page[0], page[1]]) == page::TYPE_HEAP)
            .map(<[u8]>::to_vec)
            .collect()
    }

    #[test]
    fn leaves_an_unmodified_heap_page_unchanged() {
        let file = TempPath::new();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store
            .publish(&[
                image("notes", vec![(1, sample(1, "note"))], 2),
                image("users", vec![(1, sample(1, "Ada"))], 2),
            ])
            .unwrap();
        let before = heap_images(file.path());
        let before_len = fs::metadata(file.path()).unwrap().len();
        store
            .publish(&[
                image("notes", vec![(1, sample(1, "note"))], 2),
                image("users", vec![(1, sample(1, "Bea"))], 2),
            ])
            .unwrap();
        let after = heap_images(file.path());
        assert_eq!(fs::metadata(file.path()).unwrap().len(), before_len);
        assert_eq!(before.len(), 2);
        assert_eq!(after.len(), 2);
        let unchanged = before.iter().filter(|page| after.contains(page)).count();
        assert_eq!(unchanged, 1);
        assert_eq!(read_meta(file.path()).commit_id, 2);
        assert!(
            wal::path_for(file.path())
                .metadata()
                .map(|m| m.len())
                .unwrap_or(0)
                == 0
        );
    }

    #[test]
    fn reuses_a_freed_page_without_growing_the_file() {
        let file = TempPath::new();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap();
        store.publish(&[image("users", vec![], 2)]).unwrap();
        let after_delete = fs::metadata(file.path()).unwrap().len();
        store
            .publish(&[image("users", vec![(2, sample(2, "Bea"))], 3)])
            .unwrap();
        assert!(fs::metadata(file.path()).unwrap().len() <= after_delete);
        drop(store);
        let (_, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows[0].0, 2);
    }

    #[test]
    fn discards_an_incomplete_log_and_keeps_the_previous_rows() {
        let file = TempPath::new();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap();
        store.set_inject(Inject::BeforeLogSync);
        let error = store
            .publish(&[image(
                "users",
                vec![(1, sample(1, "Ada")), (2, sample(2, "Bea"))],
                3,
            )])
            .unwrap_err();
        assert!(matches!(error, StepError::BeforeCommit(_)));
        drop(store);
        let (_, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows.len(), 1);
        assert_eq!(wal::path_for(file.path()).metadata().unwrap().len(), 0);
    }

    #[test]
    fn replays_a_synced_log_when_the_checkpoint_did_not_start() {
        let file = TempPath::new();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap();
        store.set_inject(Inject::AfterLogSync);
        let error = store
            .publish(&[image(
                "users",
                vec![(1, sample(1, "Ada")), (2, sample(2, "Bea"))],
                3,
            )])
            .unwrap_err();
        let StepError::AfterCommit(error) = error else {
            panic!("expected a fenced commit");
        };
        assert!(error.to_string().contains("checkpoint did not finish"));
        let again = store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap_err();
        assert!(
            again
                .into_db()
                .to_string()
                .contains("checkpoint did not finish")
        );
        drop(store);
        let (_, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows.len(), 2);
        assert_eq!(loaded[0].rows[1].0, 2);
    }

    #[test]
    fn replays_a_checkpoint_that_stopped_after_one_page() {
        let file = TempPath::new();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store.set_inject(Inject::DuringCheckpoint(1));
        let error = store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap_err();
        assert!(matches!(error, StepError::AfterCommit(_)));
        drop(store);
        let (_, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows.len(), 1);
        assert_eq!(read_meta(file.path()).commit_id, 1);
    }

    #[test]
    fn clears_a_log_that_was_already_applied_to_the_database() {
        for inject in [Inject::AfterDbSync, Inject::BeforeLogClear] {
            let file = TempPath::new();
            let (mut store, _) = Store::open(file.path()).unwrap();
            store.set_inject(inject);
            let error = store
                .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
                .unwrap_err();
            assert!(matches!(error, StepError::AfterCommit(_)));
            assert!(fs::metadata(wal::path_for(file.path())).unwrap().len() > 0);
            drop(store);
            let (_, loaded) = Store::open(file.path()).unwrap();
            assert_eq!(loaded[0].rows.len(), 1);
            assert_eq!(wal::path_for(file.path()).metadata().unwrap().len(), 0);
            let (_, again) = Store::open(file.path()).unwrap();
            assert_eq!(again[0].rows.len(), 1);
        }
    }

    #[test]
    fn refuses_a_log_with_the_wrong_nonce_or_commit_id() {
        let file = TempPath::new();
        {
            let (store, _) = Store::open(file.path()).unwrap();
            drop(store);
        }
        let meta = read_meta(file.path());
        let bytes = fs::read(file.path()).unwrap();
        let mut page = Page::new();
        page.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        let foreign = wal::encode_committed(&wal::Plan {
            nonce: meta.nonce.wrapping_add(1),
            base_commit: meta.commit_id,
            frames: &[(0, page)],
        })
        .unwrap();
        let wal_path = wal::path_for(file.path());
        fs::write(&wal_path, &foreign).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("does not belong"));
        assert!(fs::metadata(&wal_path).unwrap().len() > 0);

        let mut page = Page::new();
        page.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        let conflict = wal::encode_committed(&wal::Plan {
            nonce: meta.nonce,
            base_commit: meta.commit_id.wrapping_add(4),
            frames: &[(0, page)],
        })
        .unwrap();
        fs::write(&wal_path, conflict).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("conflicts"));
        assert!(fs::metadata(&wal_path).unwrap().len() > 0);
    }

    #[test]
    fn rejects_an_orphan_page_and_a_broken_heap_link() {
        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish(&[image(
                    "users",
                    vec![(1, sample(1, "Ada")), (2, sample(2, "Bea"))],
                    3,
                )])
                .unwrap();
        }
        let mut bytes = fs::read(file.path()).unwrap();
        let mut extra = Page::new();
        page::init_slotted(&mut extra, page::TYPE_HEAP, 3);
        page::seal(&mut extra);
        bytes.extend_from_slice(extra.as_slice());
        bytes[40..44].copy_from_slice(&4u32.to_le_bytes());
        let mut meta = Page::new();
        meta.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        page::seal(&mut meta);
        bytes[..PAGE_SIZE].copy_from_slice(meta.as_slice());
        fs::write(file.path(), &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");

        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish(&[image(
                    "users",
                    vec![(1, sample(1, "Ada")), (2, sample(2, "Bea"))],
                    3,
                )])
                .unwrap();
        }
        let mut bytes = fs::read(file.path()).unwrap();
        let heap_index = bytes
            .chunks(PAGE_SIZE)
            .position(|page| u16::from_le_bytes([page[0], page[1]]) == page::TYPE_HEAP)
            .unwrap();
        let start = heap_index * PAGE_SIZE;
        let mut heap = Page::new();
        heap.as_mut_slice()
            .copy_from_slice(&bytes[start..start + PAGE_SIZE]);
        let slot = page::live_slots(&heap).unwrap()[0];
        let cell = page::cell_mut(&mut heap, slot).unwrap();
        cell[14..18].copy_from_slice(&99u32.to_le_bytes());
        page::seal(&mut heap);
        bytes[start..start + PAGE_SIZE].copy_from_slice(heap.as_slice());
        fs::write(file.path(), &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
    }

    #[test]
    fn requires_an_explicit_migration_from_version_1() {
        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
                .unwrap();
        }
        let mut bytes = fs::read(file.path()).unwrap();
        bytes[32..36].copy_from_slice(&1u32.to_le_bytes());
        let mut meta = Page::new();
        meta.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        page::seal(&mut meta);
        bytes[..PAGE_SIZE].copy_from_slice(meta.as_slice());
        fs::write(file.path(), &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("explicit migration"), "{error}");
        Store::migrate(file.path()).unwrap();
        let (mut store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows.len(), 1);
        assert_eq!(loaded[0].table_id, 1);
        let before = fs::metadata(file.path()).unwrap().len();
        store
            .publish(&[image(
                "users",
                vec![(1, sample(1, "Ada")), (2, sample(2, "Bea"))],
                3,
            )])
            .unwrap();
        let after = fs::metadata(file.path()).unwrap().len();
        assert!(after <= before + PAGE_SIZE as u64);
        drop(store);
        Store::migrate(file.path()).unwrap();
        let (store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(store.format_version(), 3);
        assert_eq!(loaded[0].rows.len(), 2);
        drop(store);
        let error = Store::migrate(file.path()).unwrap_err();
        assert!(
            error.to_string().contains("already format version 3"),
            "{error}"
        );
    }

    #[test]
    fn table_ids_are_not_reused_after_drop_and_restart() {
        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
                .unwrap();
            store
                .publish(&[
                    image("notes", vec![(1, sample(1, "n"))], 2),
                    image("users", vec![(1, sample(1, "Ada"))], 2),
                ])
                .unwrap();
            assert_eq!(store.state.tables["users"].table_id, 1);
            assert_eq!(store.state.tables["notes"].table_id, 2);
            store
                .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
                .unwrap();
            assert_eq!(store.state.next_table_id, 3);
        }
        let (mut store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(store.state.next_table_id, 3);
        store
            .publish(&[
                image("notes", vec![(1, sample(1, "again"))], 2),
                image("users", vec![(1, sample(1, "Ada"))], 2),
            ])
            .unwrap();
        assert_eq!(store.state.tables["notes"].table_id, 3);
    }

    #[test]
    fn an_interrupted_create_or_drop_keeps_the_table_id_high_water() {
        let file = TempPath::new();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap();
        store.set_inject(Inject::AfterLogSync);
        let error = store
            .publish(&[
                image("notes", vec![(1, sample(1, "n"))], 2),
                image("users", vec![(1, sample(1, "Ada"))], 2),
            ])
            .unwrap_err();
        assert!(matches!(error, StepError::AfterCommit(_)));
        drop(store);

        let (mut store, loaded) = Store::open(file.path()).unwrap();
        let notes = loaded.iter().find(|table| table.name == "notes").unwrap();
        assert_eq!(notes.table_id, 2);
        store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap();
        drop(store);

        let (mut store, _) = Store::open(file.path()).unwrap();
        store.set_inject(Inject::AfterLogSync);
        let error = store
            .publish(&[
                image("notes", vec![(1, sample(1, "again"))], 2),
                image("users", vec![(1, sample(1, "Ada"))], 2),
            ])
            .unwrap_err();
        assert!(matches!(error, StepError::AfterCommit(_)));
        drop(store);

        let (store, loaded) = Store::open(file.path()).unwrap();
        let notes = loaded.iter().find(|table| table.name == "notes").unwrap();
        assert_eq!(notes.table_id, 3);
        assert_eq!(store.state.next_table_id, 4);
    }

    #[test]
    fn rejects_an_invalid_or_exhausted_table_id_counter() {
        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
                .unwrap();
        }
        patch_page0(file.path(), |page| {
            page.as_mut_slice()[PAGE_SIZE - 4..PAGE_SIZE].copy_from_slice(&1u32.to_le_bytes());
        });
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");

        let file = TempPath::new();
        {
            let (store, _) = Store::open(file.path()).unwrap();
            drop(store);
        }
        patch_page0(file.path(), |page| {
            page.as_mut_slice()[PAGE_SIZE - 4..PAGE_SIZE].copy_from_slice(&u32::MAX.to_le_bytes());
        });
        let (mut store, _) = Store::open(file.path()).unwrap();
        let error = store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap_err();
        let StepError::BeforeCommit(error) = error else {
            panic!("expected a pre-commit failure");
        };
        assert!(
            error.to_string().contains("table id space is exhausted"),
            "{error}"
        );
    }

    #[test]
    fn keeps_a_damaged_commit_record_and_a_foreign_incomplete_log() {
        let file = TempPath::new();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap();
        store.set_inject(Inject::AfterLogSync);
        let error = store
            .publish(&[image(
                "users",
                vec![(1, sample(1, "Ada")), (2, sample(2, "Bea"))],
                3,
            )])
            .unwrap_err();
        assert!(matches!(error, StepError::AfterCommit(_)));
        drop(store);
        let wal_path = wal::path_for(file.path());
        let before = fs::metadata(&wal_path).unwrap().len();
        let mut bytes = fs::read(&wal_path).unwrap();
        let checksum = bytes.len() - 32 + 12;
        bytes[checksum] ^= 0xff;
        fs::write(&wal_path, &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("damaged"), "{error}");
        assert_eq!(fs::metadata(&wal_path).unwrap().len(), before);

        let file = TempPath::new();
        {
            let (store, _) = Store::open(file.path()).unwrap();
            drop(store);
        }
        let meta = read_meta(file.path());
        let db = fs::read(file.path()).unwrap();
        let mut page = Page::new();
        page.as_mut_slice().copy_from_slice(&db[..PAGE_SIZE]);
        let foreign = wal::encode_uncommitted(&wal::Plan {
            nonce: meta.nonce.wrapping_add(1),
            base_commit: meta.commit_id,
            frames: &[(0, page)],
        })
        .unwrap();
        let wal_path = wal::path_for(file.path());
        fs::write(&wal_path, &foreign).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("does not belong"), "{error}");
        assert_eq!(fs::metadata(&wal_path).unwrap().len(), foreign.len() as u64);
    }

    #[test]
    fn a_legacy_file_without_a_high_water_mark_can_reuse_a_deleted_id_once() {
        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
                .unwrap();
            store
                .publish(&[
                    image("notes", vec![(1, sample(1, "n"))], 2),
                    image("users", vec![(1, sample(1, "Ada"))], 2),
                ])
                .unwrap();
            store
                .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
                .unwrap();
        }
        patch_page0(file.path(), |page| {
            page.as_mut_slice()[PAGE_SIZE - 4..PAGE_SIZE].copy_from_slice(&0u32.to_le_bytes());
        });
        let (mut store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].table_id, 1);
        assert_eq!(store.state.next_table_id, 2);
        store
            .publish(&[
                image("notes", vec![(1, sample(1, "again"))], 2),
                image("users", vec![(1, sample(1, "Ada"))], 2),
            ])
            .unwrap();
        assert_eq!(store.state.tables["notes"].table_id, 2);
        store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap();
        drop(store);
        let (mut store, _) = Store::open(file.path()).unwrap();
        assert_eq!(store.state.next_table_id, 3);
        store
            .publish(&[
                image("notes", vec![(1, sample(1, "third"))], 2),
                image("users", vec![(1, sample(1, "Ada"))], 2),
            ])
            .unwrap();
        assert_eq!(store.state.tables["notes"].table_id, 3);
    }

    #[test]
    fn an_ambiguous_sync_leaves_the_database_bytes_unchanged() {
        let file = TempPath::new();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap();
        let before = fs::read(file.path()).unwrap();
        let commit = read_meta(file.path()).commit_id;
        store.set_inject(Inject::CommitSyncFailed);
        let error = store
            .publish(&[image(
                "users",
                vec![(1, sample(1, "Ada")), (2, sample(2, "Bea"))],
                3,
            )])
            .unwrap_err();
        assert!(matches!(error, StepError::AfterCommit(_)));
        assert_eq!(fs::read(file.path()).unwrap(), before);
        assert_eq!(read_meta(file.path()).commit_id, commit);
        let wal_path = wal::path_for(file.path());
        assert!(fs::metadata(&wal_path).unwrap().len() > 0);
        let error = store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap_err();
        assert!(matches!(error, StepError::AfterCommit(_)));
        drop(store);

        let (_, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows.len(), 2);
        assert_eq!(fs::metadata(&wal_path).unwrap().len(), 0);
    }

    #[test]
    fn a_trailer_with_a_bad_length_or_a_disagreeing_meta_frame_is_kept() {
        let file = TempPath::new();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap();
        store.set_inject(Inject::AfterLogSync);
        let error = store
            .publish(&[image(
                "users",
                vec![(1, sample(1, "Ada")), (2, sample(2, "Bea"))],
                3,
            )])
            .unwrap_err();
        assert!(matches!(error, StepError::AfterCommit(_)));
        drop(store);
        let wal_path = wal::path_for(file.path());
        let before = fs::metadata(&wal_path).unwrap().len();
        let mut bytes = fs::read(&wal_path).unwrap();
        let count_at = bytes.len() - 32 + 8;
        bytes[count_at] ^= 0x01;
        fs::write(&wal_path, &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("damaged"), "{error}");
        assert_eq!(fs::metadata(&wal_path).unwrap().len(), before);

        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
                .unwrap();
        }
        let before = fs::read(file.path()).unwrap();
        let meta = read_meta(file.path());
        let mut old = Page::new();
        old.as_mut_slice().copy_from_slice(&before[..PAGE_SIZE]);
        let stale = wal::encode_committed(&wal::Plan {
            nonce: meta.nonce,
            base_commit: meta.commit_id,
            frames: &[(0, old)],
        })
        .unwrap();
        let wal_path = wal::path_for(file.path());
        fs::write(&wal_path, &stale).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("conflicts"), "{error}");
        assert_eq!(fs::read(file.path()).unwrap(), before);
        assert_eq!(fs::metadata(&wal_path).unwrap().len(), stale.len() as u64);
    }

    #[test]
    fn a_committed_log_that_repeats_a_page_is_kept() {
        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
                .unwrap();
        }
        let before = fs::read(file.path()).unwrap();
        let meta = read_meta(file.path());
        let mut updated = meta;
        updated.commit_id = meta.commit_id.checked_add(1).expect("commit id fits");
        let mut frame = Page::new();
        page::write_meta(&mut frame, updated, &[]).unwrap();
        page::seal(&mut frame);
        let mut again = Page::new();
        again.as_mut_slice().copy_from_slice(frame.as_slice());
        let bytes = wal::encode_committed(&wal::Plan {
            nonce: meta.nonce,
            base_commit: meta.commit_id,
            frames: &[(0, frame), (0, again)],
        })
        .unwrap();
        let wal_path = wal::path_for(file.path());
        fs::write(&wal_path, &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("conflicts"), "{error}");
        assert_eq!(fs::read(file.path()).unwrap(), before);
        assert_eq!(fs::metadata(&wal_path).unwrap().len(), bytes.len() as u64);
    }

    #[test]
    fn a_failed_open_does_not_keep_the_lock() {
        let file = TempPath::new();
        fs::write(file.path(), [0u8; 100]).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("truncated"), "{error}");
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("truncated"), "{error}");
        assert!(!error.to_string().contains("is locked"), "{error}");
    }

    #[test]
    fn a_single_row_insert_and_update_rewrite_only_the_changed_pages() {
        let file = TempPath::new();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store.publish(&[image("users", Vec::new(), 1)]).unwrap();
        let before = fs::read(file.path()).unwrap();
        let before_len = before.len();
        store
            .publish(&[image("users", vec![(1, sample(1, "Ada"))], 2)])
            .unwrap();
        let inserted = fs::read(file.path()).unwrap();
        assert_eq!(inserted.len(), before_len + PAGE_SIZE);
        assert_eq!(changed_pages(&before, &inserted), 3);

        store
            .publish(&[image("users", vec![(1, sample(1, "Bea"))], 2)])
            .unwrap();
        let updated = fs::read(file.path()).unwrap();
        assert_eq!(updated.len(), inserted.len());
        assert_eq!(changed_pages(&inserted, &updated), 2);

        store.publish(&[image("users", Vec::new(), 2)]).unwrap();
        let deleted = fs::read(file.path()).unwrap();
        assert_eq!(deleted.len(), updated.len());
        let freed = deleted
            .chunks(PAGE_SIZE)
            .filter(|page| u16::from_le_bytes([page[0], page[1]]) == page::TYPE_FREE)
            .count();
        assert_eq!(freed, 1);
        store
            .publish(&[image("users", vec![(2, sample(2, "Bea"))], 3)])
            .unwrap();
        assert_eq!(
            fs::metadata(file.path()).unwrap().len(),
            deleted.len() as u64
        );
    }

    #[test]
    fn a_one_row_update_compares_every_row() {
        let file = TempPath::new();
        let rows: Vec<(u64, Vec<Value>)> =
            (1..=100).map(|id| (id, sample(id as i64, "Ada"))).collect();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store.publish(&[image("users", rows.clone(), 101)]).unwrap();
        let _profile = profile::enable();
        profile::reset();
        let mut updated = rows;
        updated[0].1 = sample(1, "Bea");
        store.publish(&[image("users", updated, 101)]).unwrap();
        assert_eq!(profile::take().row_compares, 100);
    }

    #[test]
    #[ignore = "release profile of one UPDATE on 10000 rows"]
    fn profile_single_row_update_on_ten_thousand_rows() {
        let rows: Vec<(u64, Vec<Value>)> = (1..=10_000)
            .map(|id| (id, sample(id as i64, &"n".repeat(64))))
            .collect();
        let mut memory = crate::Database::new();
        memory
            .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
            .unwrap();
        for (id, values) in &rows {
            let Value::Text(name) = &values[1] else {
                panic!("name");
            };
            memory
                .execute(&format!("INSERT INTO users VALUES ({id}, '{name}', true);"))
                .unwrap();
        }
        let mut memory_samples = Vec::new();
        for _ in 0..5 {
            let started = std::time::Instant::now();
            memory
                .execute("UPDATE users SET active = false WHERE id = 1;")
                .unwrap();
            memory_samples.push(started.elapsed());
            memory
                .execute("UPDATE users SET active = true WHERE id = 1;")
                .unwrap();
        }
        memory_samples.sort();
        eprintln!(
            "in-memory update us: {}",
            memory_samples
                .iter()
                .map(|sample| sample.as_micros().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );

        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store.publish(&[image("users", rows, 10_001)]).unwrap();
        }
        let mut database = crate::Database::open(file.path()).unwrap();
        let _profile = profile::enable();
        let mut samples = Vec::new();
        for _ in 0..5 {
            profile::reset();
            let started = std::time::Instant::now();
            database
                .execute("UPDATE users SET active = false WHERE id = 1;")
                .unwrap();
            let total = started.elapsed();
            let phases = profile::take();
            samples.push((total, phases));
            database
                .execute("UPDATE users SET active = true WHERE id = 1;")
                .unwrap();
        }
        samples.sort_by_key(|(total, _)| *total);
        for (index, (total, phases)) in samples.iter().enumerate() {
            eprintln!(
                "update profile sample={} memory_median_us={} total_us={} compare_us={} apply_us={} wal_write_us={} wal_sync_us={} db_write_us={} db_sync_us={} wal_clear_us={} frames={} row_compares={}",
                index,
                memory_samples[2].as_micros(),
                total.as_micros(),
                phases.compare.as_micros(),
                phases.apply.as_micros(),
                phases.wal_write.as_micros(),
                phases.wal_sync.as_micros(),
                phases.db_write.as_micros(),
                phases.db_sync.as_micros(),
                phases.wal_clear.as_micros(),
                phases.frames,
                phases.row_compares,
            );
        }
        let (_, phases) = &samples[2];
        assert_eq!(phases.row_compares, 10_000);
        assert!(phases.frames <= 4, "frames {}", phases.frames);
    }

    fn changed_pages(before: &[u8], after: &[u8]) -> usize {
        after
            .chunks(PAGE_SIZE)
            .enumerate()
            .filter(|(index, page)| {
                let start = index * PAGE_SIZE;
                before.get(start..start + PAGE_SIZE) != Some(*page)
            })
            .count()
    }

    fn patch_page0(path: &std::path::Path, edit: impl FnOnce(&mut Page)) {
        let mut bytes = fs::read(path).unwrap();
        let mut page = Page::new();
        page.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        edit(&mut page);
        page::seal(&mut page);
        bytes[..PAGE_SIZE].copy_from_slice(page.as_slice());
        fs::write(path, bytes).unwrap();
    }

    fn migrated_users(rows: Vec<(u64, Vec<Value>)>, next_row_id: u64) -> (TempPath, u64, u64) {
        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store.publish(&[image("users", rows, next_row_id)]).unwrap();
        }
        let before = read_meta(file.path());
        Store::migrate(file.path()).unwrap();
        let after = read_meta(file.path());
        assert_eq!(after.version, 3);
        assert_eq!(after.nonce, before.nonce);
        assert_eq!(after.commit_id, before.commit_id);
        assert_eq!(after.index_catalog_root, 0);
        (file, before.nonce, before.commit_id)
    }

    #[test]
    fn builds_an_index_on_existing_rows_and_reopens_it() {
        let rows = vec![
            (1, sample(30, "Ada")),
            (2, sample(30, "Bea")),
            (4, sample(10, "Cy")),
        ];
        let (file, _, _) = migrated_users(rows, 5);
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            let error = store
                .publish_indexes(
                    &[image(
                        "users",
                        vec![
                            (1, sample(30, "Ada")),
                            (2, sample(30, "Bea")),
                            (4, sample(10, "Cy")),
                        ],
                        5,
                    )],
                    &[
                        IndexImage {
                            name: "idx_users_id".to_string(),
                            table: "users".to_string(),
                            column: "id".to_string(),
                        },
                        IndexImage {
                            name: "idx_users_id".to_string(),
                            table: "users".to_string(),
                            column: "id".to_string(),
                        },
                    ],
                )
                .unwrap_err();
            let StepError::BeforeCommit(error) = error else {
                panic!("duplicate index was not rejected before commit");
            };
            assert!(error.to_string().contains("already exists"), "{error}");
            store
                .publish_indexes(
                    &[image(
                        "users",
                        vec![
                            (1, sample(30, "Ada")),
                            (2, sample(30, "Bea")),
                            (4, sample(10, "Cy")),
                        ],
                        5,
                    )],
                    &[IndexImage {
                        name: "idx_users_id".to_string(),
                        table: "users".to_string(),
                        column: "id".to_string(),
                    }],
                )
                .unwrap();
        }
        let (store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows.len(), 3);
        assert_eq!(store.indexes().len(), 1);
        assert_eq!(store.indexes()[0].column, "id");
    }

    #[test]
    fn maintains_indexes_across_null_update_and_delete() {
        let (file, _, _) = migrated_users(vec![(1, sample(7, "Ada"))], 2);
        let index = IndexImage {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            column: "id".to_string(),
        };
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish_indexes(
                    &[image("users", vec![(1, sample(7, "Ada"))], 2)],
                    std::slice::from_ref(&index),
                )
                .unwrap();
            let mut changed = sample(7, "Ada");
            changed[2] = Value::Null;
            store
                .publish_indexes(
                    &[image("users", vec![(1, changed)], 2)],
                    std::slice::from_ref(&index),
                )
                .unwrap();
            let mut nulled = sample(7, "Ada");
            nulled[0] = Value::Null;
            store
                .publish_indexes(
                    &[image("users", vec![(1, nulled.clone())], 2)],
                    std::slice::from_ref(&index),
                )
                .unwrap();
            nulled[0] = Value::Integer(9);
            store
                .publish_indexes(
                    &[image("users", vec![(1, nulled), (2, sample(9, "Bea"))], 3)],
                    std::slice::from_ref(&index),
                )
                .unwrap();
            store
                .publish_indexes(&[image("users", vec![(2, sample(9, "Bea"))], 3)], &[index])
                .unwrap();
        }
        let (store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows.len(), 1);
        assert_eq!(loaded[0].rows[0].0, 2);
        assert_eq!(store.indexes().len(), 1);
    }

    #[test]
    fn drop_index_and_drop_table_release_index_pages() {
        let (file, _, _) = migrated_users(vec![(1, sample(1, "Ada"))], 2);
        let index = IndexImage {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            column: "id".to_string(),
        };
        let grown = {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish_indexes(&[image("users", vec![(1, sample(1, "Ada"))], 2)], &[index])
                .unwrap();
            fs::metadata(file.path()).unwrap().len()
        };
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish_indexes(&[image("users", vec![(1, sample(1, "Ada"))], 2)], &[])
                .unwrap();
            assert!(store.indexes().is_empty());
            store.publish(&[]).unwrap();
        }
        let (store, loaded) = Store::open(file.path()).unwrap();
        assert!(loaded.is_empty());
        assert!(store.indexes().is_empty());
        assert!(fs::metadata(file.path()).unwrap().len() <= grown);
    }

    #[test]
    fn rejects_a_corrupt_index_leaf_and_a_missing_row_reference() {
        let (file, _, _) = migrated_users(vec![(1, sample(4, "Ada")), (2, sample(5, "Bea"))], 3);
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish_indexes(
                    &[image(
                        "users",
                        vec![(1, sample(4, "Ada")), (2, sample(5, "Bea"))],
                        3,
                    )],
                    &[IndexImage {
                        name: "idx_users_id".to_string(),
                        table: "users".to_string(),
                        column: "id".to_string(),
                    }],
                )
                .unwrap();
        }
        let mut bytes = fs::read(file.path()).unwrap();
        let leaf = bytes
            .chunks(PAGE_SIZE)
            .position(|page| u16::from_le_bytes([page[0], page[1]]) == page::TYPE_INDEX_LEAF)
            .unwrap();
        let start = leaf * PAGE_SIZE;
        let mut page = Page::new();
        page.as_mut_slice()
            .copy_from_slice(&bytes[start..start + PAGE_SIZE]);
        page.as_mut_slice()[24] = 0;
        page::seal(&mut page);
        bytes[start..start + PAGE_SIZE].copy_from_slice(page.as_slice());
        fs::write(file.path(), &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");

        let (file, _, _) = migrated_users(vec![(1, sample(4, "Ada"))], 2);
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish_indexes(
                    &[image("users", vec![(1, sample(4, "Ada"))], 2)],
                    &[IndexImage {
                        name: "idx_users_id".to_string(),
                        table: "users".to_string(),
                        column: "id".to_string(),
                    }],
                )
                .unwrap();
        }
        let mut bytes = fs::read(file.path()).unwrap();
        let leaf = bytes
            .chunks(PAGE_SIZE)
            .position(|page| u16::from_le_bytes([page[0], page[1]]) == page::TYPE_INDEX_LEAF)
            .unwrap();
        let start = leaf * PAGE_SIZE;
        let mut page = Page::new();
        page.as_mut_slice()
            .copy_from_slice(&bytes[start..start + PAGE_SIZE]);
        page.as_mut_slice()[28 + 8..28 + 16].copy_from_slice(&99u64.to_le_bytes());
        page::seal(&mut page);
        bytes[start..start + PAGE_SIZE].copy_from_slice(page.as_slice());
        fs::write(file.path(), &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
    }

    #[test]
    fn rejects_an_index_build_that_exceeds_the_commit_frame_limit() {
        let _guard = FrameCapGuard(4);
        let rows: Vec<_> = (1..=40).map(|id| (id, sample(id as i64, "n"))).collect();
        let (file, _, _) = migrated_users(rows.clone(), 41);
        page::TEST_FRAME_CAP.with(|cap| cap.set(2));
        let before = fs::read(file.path()).unwrap();
        let error = {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish_indexes(
                    &[image("users", rows, 41)],
                    &[IndexImage {
                        name: "idx_users_id".to_string(),
                        table: "users".to_string(),
                        column: "id".to_string(),
                    }],
                )
                .unwrap_err()
        };
        let StepError::BeforeCommit(error) = error else {
            panic!("an oversized index build was not rejected before commit");
        };
        assert!(
            error.to_string().contains("more pages than one commit"),
            "{error}"
        );
        assert_eq!(fs::read(file.path()).unwrap(), before);
        drop(_guard);
        let (store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows.len(), 40);
        assert!(store.indexes().is_empty());
    }

    #[test]
    fn recovers_an_index_update_after_a_partial_checkpoint() {
        let (file, _, _) = migrated_users(vec![(1, sample(1, "Ada"))], 2);
        let index = IndexImage {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            column: "id".to_string(),
        };
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish_indexes(
                    &[image("users", vec![(1, sample(1, "Ada"))], 2)],
                    std::slice::from_ref(&index),
                )
                .unwrap();
            store.set_inject(Inject::AfterLogSync);
            let error = store
                .publish_indexes(
                    &[image(
                        "users",
                        vec![(1, sample(1, "Ada")), (2, sample(2, "Bea"))],
                        3,
                    )],
                    &[index],
                )
                .unwrap_err();
            assert!(matches!(error, StepError::AfterCommit(_)));
        }
        let (store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows.len(), 2);
        assert_eq!(store.indexes().len(), 1);
    }

    fn append_free(bytes: &mut Vec<u8>, id: u32, directory: Option<(u32, &[u32])>) {
        let mut page = Page::new();
        if let Some((next, ids)) = directory {
            page::write_free_directory(&mut page, id, next, ids).unwrap();
        } else {
            page::write_free_data(&mut page, id);
        }
        page::seal(&mut page);
        bytes.extend_from_slice(page.as_slice());
    }

    fn write_v2_meta(
        bytes: &mut [u8],
        page_count: u32,
        freelist_root: u32,
        next_table_id: u32,
        inline: &[u32],
    ) {
        let mut meta = Page::new();
        meta.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        let old = page::read_meta(&meta).unwrap();
        page::write_meta(
            &mut meta,
            page::Meta {
                version: page::FORMAT_VERSION,
                page_count,
                catalog_root: old.catalog_root,
                freelist_root,
                nonce: old.nonce,
                commit_id: old.commit_id,
                next_table_id,
                index_catalog_root: 0,
            },
            inline,
        )
        .unwrap();
        page::seal(&mut meta);
        bytes[..PAGE_SIZE].copy_from_slice(meta.as_slice());
    }

    fn fresh_v2() -> TempPath {
        let file = TempPath::new();
        let (mut store, _) = Store::open(file.path()).unwrap();
        store.publish(&[image("users", vec![], 1)]).unwrap();
        file
    }

    #[test]
    fn migration_preserves_one_thousand_six_inline_free_ids() {
        let file = fresh_v2();
        let mut bytes = fs::read(file.path()).unwrap();
        let count = page::META_FREE_CAPACITY_LEGACY;
        let ids: Vec<u32> = (2..2 + count as u32).collect();
        assert_eq!(ids.len(), 1006);
        for id in &ids {
            append_free(&mut bytes, *id, None);
        }
        write_v2_meta(
            &mut bytes,
            2 + count as u32,
            0,
            2,
            &ids[..page::META_FREE_CAPACITY],
        );
        let mut meta = Page::new();
        meta.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        meta.as_mut_slice()[52..56].copy_from_slice(&(count as u32).to_le_bytes());
        let mark = 72 + (count - 1) * 4;
        assert_eq!(mark, 4092);
        meta.as_mut_slice()[mark..mark + 4].copy_from_slice(&ids[count - 1].to_le_bytes());
        page::seal(&mut meta);
        bytes[..PAGE_SIZE].copy_from_slice(meta.as_slice());
        fs::write(file.path(), &bytes).unwrap();

        let occupied = read_meta(file.path());
        assert_eq!(occupied.version, 2);
        assert_eq!(occupied.next_table_id, 0);
        let inline = {
            let mut page = Page::new();
            page.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
            page::inline_free_ids(&page).unwrap()
        };
        assert_eq!(inline, ids);
        {
            let (store, loaded) = Store::open(file.path()).unwrap();
            assert_eq!(store.format_version(), 2);
            assert_eq!(store.state.next_table_id, 2);
            assert_eq!(store.state.free_ids, ids);
            assert_eq!(loaded[0].table_id, 1);
            assert_eq!(store.state.index_catalog_root, 0);
        }

        Store::migrate(file.path()).unwrap();
        let path = file.path().to_path_buf();
        {
            let (store, loaded) = Store::open(&path).unwrap();
            assert_eq!(store.format_version(), 3);
            assert_eq!(store.state.index_catalog_root, 0);
            assert_eq!(store.state.next_table_id, 2);
            assert_eq!(store.state.free_ids, ids);
            assert!(!store.state.free_ids.contains(&store.state.freelist_root));
            assert_eq!(store.state.freelist_root, 1008);
            assert_eq!(loaded[0].table_id, 1);
            assert_eq!(read_meta(&path).index_catalog_root, 0);
        }
        let error = match Pager::open_v1(&path) {
            Ok(_) => panic!("a version 3 file was opened as version 1"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("already format version 3"),
            "{error}"
        );

        let mut database = crate::Database::open(&path).unwrap();
        database
            .execute("CREATE INDEX idx_users_id ON users(id);")
            .unwrap();
        database
            .execute("INSERT INTO users VALUES (1, 'Ada', TRUE);")
            .unwrap();
        database
            .execute("UPDATE users SET name = 'Bea' WHERE id = 1;")
            .unwrap();
        database
            .execute("UPDATE users SET id = 4 WHERE id = 1;")
            .unwrap();
        database.execute("DELETE FROM users WHERE id = 4;").unwrap();
        database.execute("CREATE TABLE notes (id INT);").unwrap();
        drop(database);
        let (store, loaded) = Store::open(&path).unwrap();
        assert!(
            loaded
                .iter()
                .find(|table| table.name == "users")
                .unwrap()
                .rows
                .is_empty()
        );
        assert_eq!(
            loaded
                .iter()
                .find(|table| table.name == "notes")
                .unwrap()
                .table_id,
            2
        );
        assert_eq!(store.indexes().len(), 1);
        assert_eq!(store.state.next_table_id, 3);
    }

    #[test]
    fn migration_keeps_the_table_id_mark_and_the_id_in_the_index_root_slot() {
        let file = fresh_v2();
        let mut bytes = fs::read(file.path()).unwrap();
        let inline: Vec<u32> = (2..=1006).collect();
        assert_eq!(inline.len(), page::META_FREE_CAPACITY);
        for id in 2..=1006 {
            append_free(&mut bytes, id, None);
        }
        append_free(&mut bytes, 1007, Some((0, &[1008, 1009])));
        append_free(&mut bytes, 1008, None);
        append_free(&mut bytes, 1009, None);
        write_v2_meta(&mut bytes, 1010, 1007, 9, &inline);
        fs::write(file.path(), &bytes).unwrap();
        let before = read_meta(file.path());
        assert_eq!(before.version, 2);
        assert_eq!(before.next_table_id, 9);
        assert_eq!(before.index_catalog_root, 0);
        let mut expected = inline.clone();
        expected.extend([1008, 1009]);
        expected.sort_unstable();
        {
            let (store, _) = Store::open(file.path()).unwrap();
            assert_eq!(store.state.next_table_id, 9);
            assert_eq!(store.state.free_ids, expected);
            assert!(store.state.free_ids.contains(&1006));
        }
        Store::migrate(file.path()).unwrap();
        let (store, _) = Store::open(file.path()).unwrap();
        assert_eq!(store.format_version(), 3);
        assert_eq!(store.state.next_table_id, 9);
        assert_eq!(store.state.index_catalog_root, 0);
        assert_eq!(store.state.free_ids, expected);
        assert!(store.state.directory_pages.contains(&1007));
        assert!(store.state.directory_pages.contains(&1010));
        assert!(!store.state.free_ids.contains(&1007));
        assert!(!store.state.free_ids.contains(&1010));
        assert_eq!(read_meta(file.path()).next_table_id, 9);
        assert_eq!(read_meta(file.path()).index_catalog_root, 0);
    }

    #[test]
    fn migration_rejects_a_redo_log_and_retries_after_a_leftover_temp() {
        let file = fresh_v2();
        let before = fs::read(file.path()).unwrap();
        let wal = wal::path_for(file.path());
        fs::write(&wal, b"not empty").unwrap();
        let error = Store::migrate(file.path()).unwrap_err();
        assert!(error.to_string().contains("redo log"), "{error}");
        assert_eq!(fs::read(file.path()).unwrap(), before);
        assert_eq!(read_meta(file.path()).version, 2);
        fs::remove_file(&wal).unwrap();

        let junk = file.path().parent().unwrap().join(".test.db.1.1.tmp");
        fs::write(&junk, vec![9u8; 128]).unwrap();
        Store::migrate(file.path()).unwrap();
        assert_eq!(fs::read(&junk).unwrap(), vec![9u8; 128]);
        let (store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(store.format_version(), 3);
        assert_eq!(loaded[0].name, "users");
        assert_eq!(store.state.index_catalog_root, 0);
    }

    #[cfg(unix)]
    #[test]
    fn migration_failure_before_rename_keeps_the_version_2_file() {
        use std::os::unix::fs::PermissionsExt;

        struct Restore(std::path::PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Ok(metadata) = fs::metadata(&self.0) {
                    let mut permissions = metadata.permissions();
                    permissions.set_mode(0o755);
                    let _ = fs::set_permissions(&self.0, permissions);
                }
            }
        }

        let file = fresh_v2();
        let parent = file.path().parent().unwrap().to_path_buf();
        let _restore = Restore(parent.clone());
        let before = fs::read(file.path()).unwrap();
        let mut permissions = fs::metadata(&parent).unwrap().permissions();
        permissions.set_mode(0o555);
        fs::set_permissions(&parent, permissions).unwrap();
        let error = Store::migrate(file.path()).unwrap_err();
        assert_eq!(fs::read(file.path()).unwrap(), before);
        assert_eq!(read_meta(file.path()).version, 2);
        let temps = fs::read_dir(&parent).unwrap().any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().into_string().ok())
                .is_some_and(|name| name.contains(".tmp"))
        });
        assert!(!temps, "{error}");
    }

    fn integer_rows(count: u64) -> Vec<(u64, Vec<Value>)> {
        (1..=count).map(|id| (id, sample(id as i64, "n"))).collect()
    }

    fn id_index() -> IndexImage {
        IndexImage {
            name: "idx_users_id".to_string(),
            table: "users".to_string(),
            column: "id".to_string(),
        }
    }

    fn reopen_matches(path: &Path, rows: usize, indexes: usize) {
        let (store, loaded) = Store::open(path).unwrap();
        assert_eq!(loaded[0].rows.len(), rows);
        assert_eq!(store.indexes().len(), indexes);
        drop(store);
        let (store, loaded) = Store::open(path).unwrap();
        assert_eq!(loaded[0].rows.len(), rows);
        assert_eq!(store.indexes().len(), indexes);
    }

    #[test]
    fn recovery_replays_an_index_split_and_is_idempotent() {
        for inject in [Inject::AfterLogSync, Inject::DuringCheckpoint(1)] {
            let rows = integer_rows(255);
            let (file, _, _) = migrated_users(rows.clone(), 256);
            {
                let (mut store, _) = Store::open(file.path()).unwrap();
                store.set_inject(inject);
                let error = store
                    .publish_indexes(&[image("users", rows, 256)], &[id_index()])
                    .unwrap_err();
                assert!(matches!(error, StepError::AfterCommit(_)), "{error:?}");
            }
            reopen_matches(file.path(), 255, 1);
            let bytes = fs::read(file.path()).unwrap();
            let internals = bytes
                .chunks(PAGE_SIZE)
                .filter(|page| u16::from_le_bytes([page[0], page[1]]) == page::TYPE_INDEX_INTERNAL)
                .count();
            assert!(internals >= 1, "the recovered tree did not keep its split");
        }
    }

    #[test]
    fn recovery_replays_an_index_merge() {
        for inject in [Inject::AfterLogSync, Inject::DuringCheckpoint(1)] {
            let rows = integer_rows(255);
            let (file, _, _) = migrated_users(rows.clone(), 256);
            {
                let (mut store, _) = Store::open(file.path()).unwrap();
                store
                    .publish_indexes(&[image("users", rows.clone(), 256)], &[id_index()])
                    .unwrap();
                store.set_inject(inject);
                let kept: Vec<_> = rows.into_iter().filter(|(id, _)| *id > 2).collect();
                let error = store
                    .publish_indexes(&[image("users", kept, 256)], &[id_index()])
                    .unwrap_err();
                assert!(matches!(error, StepError::AfterCommit(_)), "{error:?}");
            }
            reopen_matches(file.path(), 253, 1);
            let bytes = fs::read(file.path()).unwrap();
            let leaves = bytes
                .chunks(PAGE_SIZE)
                .filter(|page| u16::from_le_bytes([page[0], page[1]]) == page::TYPE_INDEX_LEAF)
                .count();
            let internals = bytes
                .chunks(PAGE_SIZE)
                .filter(|page| u16::from_le_bytes([page[0], page[1]]) == page::TYPE_INDEX_INTERNAL)
                .count();
            assert_eq!(leaves, 1, "the recovered delete did not merge the leaves");
            assert_eq!(
                internals, 0,
                "the recovered delete did not contract the root"
            );
        }
    }

    #[test]
    fn recovery_replays_drop_index() {
        let rows = integer_rows(4);
        let (file, _, _) = migrated_users(rows.clone(), 5);
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish_indexes(&[image("users", rows.clone(), 5)], &[id_index()])
                .unwrap();
            store.set_inject(Inject::AfterLogSync);
            let error = store
                .publish_indexes(&[image("users", rows, 5)], &[])
                .unwrap_err();
            assert!(matches!(error, StepError::AfterCommit(_)), "{error:?}");
        }
        reopen_matches(file.path(), 4, 0);
    }

    fn rich_table(rows: Vec<(u64, Vec<Value>)>, next_row_id: u64) -> TableImage {
        TableImage {
            name: "users".to_string(),
            columns: vec![
                Column::new("id", DataType::Integer),
                Column::new("age", DataType::Integer),
                Column::new("name", DataType::Text),
            ],
            table_id: 0,
            next_row_id,
            rows,
        }
    }

    fn rich_row(id: u64, age: i64, name: &str) -> (u64, Vec<Value>) {
        (
            id,
            vec![
                Value::Integer(id as i64),
                Value::Integer(age),
                Value::Text(name.to_string()),
            ],
        )
    }

    fn tree_pages(bytes: &[u8]) -> Vec<Vec<u8>> {
        bytes
            .chunks(PAGE_SIZE)
            .filter(|page| {
                let kind = u16::from_le_bytes([page[0], page[1]]);
                kind == page::TYPE_INDEX_LEAF || kind == page::TYPE_INDEX_INTERNAL
            })
            .map(<[u8]>::to_vec)
            .collect()
    }

    fn owned_index_pages(store: &mut Store) -> Vec<Vec<u32>> {
        let roots: Vec<u32> = store.state.indexes.iter().map(|index| index.root).collect();
        let mut nodes = PagerNodes {
            pager: &mut store.pager,
        };
        roots
            .into_iter()
            .map(|root| btree::owned_pages(&mut nodes, root).unwrap())
            .collect()
    }

    #[test]
    fn multi_row_updates_keep_every_index_and_a_failed_update_is_not_published() {
        let _guard = FrameCapGuard(0);
        let rows: Vec<_> = (1..=8).map(|id| rich_row(id, id as i64, "a")).collect();
        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store.publish(&[rich_table(rows.clone(), 9)]).unwrap();
        }
        Store::migrate(file.path()).unwrap();
        let indexes = [
            IndexImage {
                name: "idx_users_id".to_string(),
                table: "users".to_string(),
                column: "id".to_string(),
            },
            IndexImage {
                name: "idx_users_age".to_string(),
                table: "users".to_string(),
                column: "age".to_string(),
            },
        ];
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish_indexes(&[rich_table(rows.clone(), 9)], &indexes)
                .unwrap();
            let aged: Vec<_> = rows.iter().map(|(id, _)| rich_row(*id, 5, "a")).collect();
            store
                .publish_indexes(&[rich_table(aged, 9)], &indexes)
                .unwrap();
        }
        let indexed = fs::read(file.path()).unwrap();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            let renamed: Vec<_> = (1..=8).map(|id| rich_row(id, 5, "z")).collect();
            store
                .publish_indexes(&[rich_table(renamed, 9)], &indexes)
                .unwrap();
        }
        let renamed = fs::read(file.path()).unwrap();
        assert_ne!(indexed, renamed);
        assert_eq!(tree_pages(&indexed), tree_pages(&renamed));
        page::TEST_FRAME_CAP.with(|cap| cap.set(2));
        let before = fs::read(file.path()).unwrap();
        let error = {
            let (mut store, _) = Store::open(file.path()).unwrap();
            let changed: Vec<_> = (1..=8).map(|id| rich_row(id, 9, "z")).collect();
            store
                .publish_indexes(&[rich_table(changed, 9)], &indexes)
                .unwrap_err()
        };
        let StepError::BeforeCommit(error) = error else {
            panic!("the oversized update was published");
        };
        assert!(
            error.to_string().contains("more pages than one commit"),
            "{error}"
        );
        assert_eq!(fs::read(file.path()).unwrap(), before);
        drop(_guard);
        let (store, loaded) = Store::open(file.path()).unwrap();
        assert_eq!(loaded[0].rows.len(), 8);
        assert_eq!(store.indexes().len(), 2);
        assert!(
            loaded[0]
                .rows
                .iter()
                .all(|(_, values)| values[1] == Value::Integer(5))
        );
        assert!(
            loaded[0]
                .rows
                .iter()
                .all(|(_, values)| values[2] == Value::Text("z".to_string()))
        );
    }

    #[test]
    fn drop_index_frees_only_that_tree_and_drop_table_frees_the_rest() {
        let rows: Vec<_> = (1..=260).map(|id| rich_row(id, id as i64, "n")).collect();
        let file = TempPath::new();
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store.publish(&[rich_table(rows.clone(), 261)]).unwrap();
        }
        Store::migrate(file.path()).unwrap();
        let indexes = [
            IndexImage {
                name: "idx_users_id".to_string(),
                table: "users".to_string(),
                column: "id".to_string(),
            },
            IndexImage {
                name: "idx_users_age".to_string(),
                table: "users".to_string(),
                column: "age".to_string(),
            },
        ];
        let (id_pages, age_pages) = {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish_indexes(&[rich_table(rows, 261)], &indexes)
                .unwrap();
            let owned = owned_index_pages(&mut store);
            assert_eq!(owned.len(), 2);
            let mut id_pages = owned[0].clone();
            let mut age_pages = owned[1].clone();
            id_pages.sort_unstable();
            age_pages.sort_unstable();
            assert!(id_pages.iter().all(|id| !age_pages.contains(id)));
            assert!(id_pages.iter().all(|id| !store.state.free_ids.contains(id)));
            assert!(
                age_pages
                    .iter()
                    .all(|id| !store.state.free_ids.contains(id))
            );
            (id_pages, age_pages)
        };
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store
                .publish_indexes(
                    &[rich_table(
                        (1..=260).map(|id| rich_row(id, id as i64, "n")).collect(),
                        261,
                    )],
                    &indexes[1..],
                )
                .unwrap();
        }
        {
            let (mut store, loaded) = Store::open(file.path()).unwrap();
            assert_eq!(loaded[0].rows.len(), 260);
            assert_eq!(store.indexes().len(), 1);
            assert_eq!(store.indexes()[0].column, "age");
            let owned = owned_index_pages(&mut store);
            assert!(owned[0].iter().all(|id| !store.state.free_ids.contains(id)));
            assert!(owned[0].iter().all(|id| !id_pages.contains(id)));
            for id in &id_pages {
                let mut page = Page::new();
                store.pager.read_into(*id, &mut page).unwrap();
                assert_eq!(page::page_type_of(&page), page::TYPE_FREE);
                assert!(
                    store.state.free_ids.contains(id) || store.state.directory_pages.contains(id)
                );
            }
            let _ = age_pages;
        }
        {
            let (mut store, _) = Store::open(file.path()).unwrap();
            store.publish(&[]).unwrap();
        }
        let (store, loaded) = Store::open(file.path()).unwrap();
        assert!(loaded.is_empty());
        assert!(store.indexes().is_empty());
        assert!(store.state.indexes.is_empty());
    }

    #[test]
    fn corrupt_index_metadata_and_cycles_fail_on_open() {
        let rows = integer_rows(4);
        let sibling = migrated_users(rows.clone(), 5);
        {
            let (mut store, _) = Store::open(sibling.0.path()).unwrap();
            store
                .publish_indexes(&[image("users", rows.clone(), 5)], &[id_index()])
                .unwrap();
        }
        let mut bytes = fs::read(sibling.0.path()).unwrap();
        let leaf = bytes
            .chunks(PAGE_SIZE)
            .position(|page| u16::from_le_bytes([page[0], page[1]]) == page::TYPE_INDEX_LEAF)
            .unwrap();
        let start = leaf * PAGE_SIZE;
        let mut page = Page::new();
        page.as_mut_slice()
            .copy_from_slice(&bytes[start..start + PAGE_SIZE]);
        page.as_mut_slice()[12..16].copy_from_slice(&(leaf as u32).to_le_bytes());
        page::seal(&mut page);
        bytes[start..start + PAGE_SIZE].copy_from_slice(page.as_slice());
        fs::write(sibling.0.path(), &bytes).unwrap();
        let error = Store::open(sibling.0.path()).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");

        let catalog = migrated_users(rows.clone(), 5);
        {
            let (mut store, _) = Store::open(catalog.0.path()).unwrap();
            store
                .publish_indexes(&[image("users", rows, 5)], &[id_index()])
                .unwrap();
        }
        let mut bytes = fs::read(catalog.0.path()).unwrap();
        let index_catalog = bytes
            .chunks(PAGE_SIZE)
            .position(|page| u16::from_le_bytes([page[0], page[1]]) == page::TYPE_INDEX_CATALOG)
            .unwrap();
        let start = index_catalog * PAGE_SIZE;
        let mut page = Page::new();
        page.as_mut_slice()
            .copy_from_slice(&bytes[start..start + PAGE_SIZE]);
        let slot_offset = u16::from_le_bytes([page.as_slice()[24], page.as_slice()[25]]) as usize;
        let slot_len = u16::from_le_bytes([page.as_slice()[26], page.as_slice()[27]]) as usize;
        page.as_mut_slice()[slot_offset + slot_len - 3] = 9;
        page::seal(&mut page);
        bytes[start..start + PAGE_SIZE].copy_from_slice(page.as_slice());
        fs::write(catalog.0.path(), &bytes).unwrap();
        let error = Store::open(catalog.0.path()).unwrap_err();
        assert!(
            error.to_string().contains("unsupported index format"),
            "{error}"
        );

        let cycled = migrated_users(integer_rows(255), 256);
        {
            let (mut store, _) = Store::open(cycled.0.path()).unwrap();
            store
                .publish_indexes(&[image("users", integer_rows(255), 256)], &[id_index()])
                .unwrap();
        }
        let mut bytes = fs::read(cycled.0.path()).unwrap();
        let internal = bytes
            .chunks(PAGE_SIZE)
            .position(|page| u16::from_le_bytes([page[0], page[1]]) == page::TYPE_INDEX_INTERNAL)
            .unwrap();
        let start = internal * PAGE_SIZE;
        let mut page = Page::new();
        page.as_mut_slice()
            .copy_from_slice(&bytes[start..start + PAGE_SIZE]);
        page.as_mut_slice()[28..32].copy_from_slice(&(internal as u32).to_le_bytes());
        page::seal(&mut page);
        bytes[start..start + PAGE_SIZE].copy_from_slice(page.as_slice());
        fs::write(cycled.0.path(), &bytes).unwrap();
        let error = Store::open(cycled.0.path()).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");

        let rooted = migrated_users(integer_rows(3), 4);
        {
            let (mut store, _) = Store::open(rooted.0.path()).unwrap();
            store
                .publish_indexes(&[image("users", integer_rows(3), 4)], &[id_index()])
                .unwrap();
        }
        patch_page0(rooted.0.path(), |page| {
            page.as_mut_slice()[4088..4092].copy_from_slice(&1u32.to_le_bytes());
        });
        let error = Store::open(rooted.0.path()).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
    }

    #[test]
    fn an_unknown_format_is_rejected_before_the_redo_log_is_touched() {
        let file = fresh_v2();
        let wal = wal::path_for(file.path());
        fs::write(&wal, b"do-not-touch").unwrap();
        let wal_bytes = fs::read(&wal).unwrap();
        patch_page0(file.path(), |page| {
            page.as_mut_slice()[32..36].copy_from_slice(&4u32.to_le_bytes());
        });
        let patched = fs::read(file.path()).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported database format version 4"),
            "{error}"
        );
        assert_eq!(fs::read(file.path()).unwrap(), patched);
        assert_eq!(fs::read(&wal).unwrap(), wal_bytes);
        let migrate_error = Store::migrate(file.path()).unwrap_err();
        assert!(
            migrate_error.to_string().contains("redo log"),
            "{migrate_error}"
        );
        assert_eq!(fs::read(file.path()).unwrap(), patched);
        assert_eq!(fs::read(&wal).unwrap(), wal_bytes);
        fs::remove_file(&wal).unwrap();
        let migrate_error = Store::migrate(file.path()).unwrap_err();
        assert!(
            migrate_error
                .to_string()
                .contains("unsupported database format version 4"),
            "{migrate_error}"
        );
        assert_eq!(fs::read(file.path()).unwrap(), patched);
    }

    struct FrameCapGuard(usize);
    impl Drop for FrameCapGuard {
        fn drop(&mut self) {
            let _ = self.0;
            page::TEST_FRAME_CAP.with(|cap| cap.set(0));
        }
    }
}
