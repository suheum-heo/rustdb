//! Page edits for one statement.
//!
//! The editor copies a page into memory the first time that statement changes
//! it. Unchanged pages stay out of the dirty set, so the redo log does not
//! contain them.

use std::collections::{HashMap, HashSet};

use crate::error::DbError;
use crate::storage::page::{self, PAGE_SIZE, Page};
use crate::storage::pager::Pager;
use crate::storage::profile;
use crate::storage::record::{self, Link, NULL_LINK, StoredRecord};
use crate::storage::store::TableImage;
use crate::value::Value;

const FREE_DIRECTORY_CAPACITY: usize = (PAGE_SIZE - 28) / 4;

#[derive(Clone, Debug)]
pub(crate) struct LiveTable {
    pub table_id: u32,
    pub catalog_page: u32,
    pub catalog_slot: u16,
    pub head: Link,
    pub tail: Link,
    pub next_row_id: u64,
    pub row_ids: Vec<u64>,
    pub links: Vec<Link>,
}

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub nonce: u64,
    pub commit_id: u64,
    pub page_count: u32,
    pub catalog_root: u32,
    pub freelist_root: u32,
    pub inline_free: Vec<u32>,
    pub free_ids: Vec<u32>,
    pub directory_pages: Vec<u32>,
    pub tables: HashMap<String, LiveTable>,
    pub catalog_tail: u32,
    /// Next table id to assign. Stored at the end of page 0. It never goes
    /// backward, including across DROP TABLE and restart.
    pub next_table_id: u32,
}

pub(crate) struct Prepared {
    pub snapshot: Snapshot,
    pub pages: Vec<(u32, Page)>,
    pub page_count: u32,
}

pub(crate) struct Editor {
    snapshot: Snapshot,
    pages: HashMap<u32, Page>,
    dirty: HashSet<u32>,
    freelist_dirty: bool,
    base_count: u32,
}

impl Editor {
    pub(crate) fn new(snapshot: Snapshot) -> Self {
        let base_count = snapshot.page_count;
        Self {
            snapshot,
            pages: HashMap::new(),
            dirty: HashSet::new(),
            freelist_dirty: false,
            base_count,
        }
    }

    pub(crate) fn apply(
        mut self,
        pager: &mut Pager,
        desired: &[TableImage],
    ) -> Result<Option<Prepared>, DbError> {
        let desired_names: HashSet<&str> =
            desired.iter().map(|table| table.name.as_str()).collect();
        let existing: Vec<String> = self.snapshot.tables.keys().cloned().collect();
        for name in existing {
            if !desired_names.contains(name.as_str()) {
                self.drop_table(pager, &name)?;
            }
        }
        for table in desired {
            if self.snapshot.tables.contains_key(&table.name) {
                self.sync_table(pager, table)?;
            } else {
                self.create_table(pager, table)?;
                for (row_id, values) in &table.rows {
                    self.append_row(pager, &table.name, *row_id, values)?;
                }
                self.set_next_row_id(pager, &table.name, table.next_row_id)?;
            }
        }
        if self.dirty.is_empty() && !self.freelist_dirty {
            return Ok(None);
        }
        self.finish(pager)?;
        let dirty = std::mem::take(&mut self.dirty);
        let mut pages: Vec<(u32, Page)> = self
            .pages
            .into_iter()
            .filter(|(id, _)| dirty.contains(id))
            .collect();
        pages.sort_by_key(|(id, _)| *id);
        Ok(Some(Prepared {
            page_count: self.snapshot.page_count,
            snapshot: self.snapshot,
            pages,
        }))
    }

    fn create_table(&mut self, pager: &mut Pager, table: &TableImage) -> Result<(), DbError> {
        let table_id = self.next_table_id()?;
        let bytes = record::encode_catalog(table_id, 1, NULL_LINK, &table.name, &table.columns)?;
        let (page_id, slot) = self.insert_catalog(pager, &bytes)?;
        self.snapshot.tables.insert(
            table.name.clone(),
            LiveTable {
                table_id,
                catalog_page: page_id,
                catalog_slot: slot,
                head: NULL_LINK,
                tail: NULL_LINK,
                next_row_id: 1,
                row_ids: Vec::new(),
                links: Vec::new(),
            },
        );
        Ok(())
    }

    fn drop_table(&mut self, pager: &mut Pager, name: &str) -> Result<(), DbError> {
        let live = self
            .snapshot
            .tables
            .get(name)
            .cloned()
            .ok_or_else(|| DbError::Internal(format!("missing table '{name}'")))?;
        while !self
            .snapshot
            .tables
            .get(name)
            .is_some_and(|table| table.links.is_empty())
        {
            let index = self.snapshot.tables[name].links.len() - 1;
            self.unlink_row(pager, name, index)?;
        }
        self.delete_catalog_cell(pager, live.catalog_page, live.catalog_slot)?;
        self.snapshot.tables.remove(name);
        Ok(())
    }

    fn sync_table(&mut self, pager: &mut Pager, desired: &TableImage) -> Result<(), DbError> {
        let current_ids = self.snapshot.tables[&desired.name].row_ids.clone();
        let desired_ids: Vec<u64> = desired.rows.iter().map(|(id, _)| *id).collect();
        if desired_ids == current_ids {
            for (index, (row_id, values)) in desired.rows.iter().enumerate() {
                if !self.same_values(pager, &desired.name, index, values)? {
                    self.rewrite_row(pager, &desired.name, index, *row_id, values)?;
                }
            }
        } else if desired_ids.starts_with(&current_ids) {
            for (row_id, values) in desired.rows.iter().skip(current_ids.len()) {
                self.append_row(pager, &desired.name, *row_id, values)?;
            }
        } else if is_subsequence(&desired_ids, &current_ids) {
            let keep: HashSet<u64> = desired_ids.into_iter().collect();
            loop {
                let ids = &self.snapshot.tables[&desired.name].row_ids;
                let Some(index) = ids.iter().rposition(|id| !keep.contains(id)) else {
                    break;
                };
                self.unlink_row(pager, &desired.name, index)?;
            }
        } else {
            return Err(DbError::Internal(format!(
                "cannot apply a non-incremental change to table '{}'",
                desired.name
            )));
        }
        self.set_next_row_id(pager, &desired.name, desired.next_row_id)?;
        Ok(())
    }

    fn same_values(
        &mut self,
        pager: &mut Pager,
        name: &str,
        index: usize,
        values: &[Value],
    ) -> Result<bool, DbError> {
        let mark = profile::start();
        let link = self.snapshot.tables[name].links[index];
        self.ensure(pager, link.page)?;
        let bytes = page::cell(&self.pages[&link.page], link.slot)?.to_vec();
        let record = record::decode_record(&bytes).map_err(|_| page::corrupt(link.page))?;
        let same = record.values == values;
        profile::note_row_compare();
        profile::add_compare(mark);
        Ok(same)
    }

    fn append_row(
        &mut self,
        pager: &mut Pager,
        name: &str,
        row_id: u64,
        values: &[Value],
    ) -> Result<(), DbError> {
        let bytes = record::encode_record(&StoredRecord {
            row_id,
            prev: NULL_LINK,
            next: NULL_LINK,
            values: values.to_vec(),
        })?;
        let tail = self.snapshot.tables[name].tail;
        let placed = if tail == NULL_LINK {
            let page_id = self.alloc_heap(pager)?;
            let slot = self.insert_new_heap(page_id, &bytes)?;
            Link {
                page: page_id,
                slot,
            }
        } else {
            self.ensure(pager, tail.page)?;
            let inserted = page::insert_cell(self.pages.get_mut(&tail.page).expect("tail"), &bytes);
            self.dirty.insert(tail.page);
            match inserted {
                Ok(slot) => Link {
                    page: tail.page,
                    slot,
                },
                Err(page::InsertError::Full) => {
                    let page_id = self.alloc_heap(pager)?;
                    let slot = self.insert_new_heap(page_id, &bytes)?;
                    Link {
                        page: page_id,
                        slot,
                    }
                }
                Err(page::InsertError::TooBig) => {
                    return Err(DbError::Storage(format!(
                        "row exceeds the maximum record size of {} bytes",
                        page::MAX_RECORD_LEN
                    )));
                }
            }
        };
        if tail != NULL_LINK {
            let (prev, _) = self.read_links(tail)?;
            self.patch(pager, tail, prev, placed)?;
            self.patch(pager, placed, tail, NULL_LINK)?;
        }
        let live = self.snapshot.tables.get_mut(name).expect("table");
        if live.head == NULL_LINK {
            live.head = placed;
        }
        live.tail = placed;
        live.row_ids.push(row_id);
        live.links.push(placed);
        let head = live.head;
        let next_row_id = row_id
            .checked_add(1)
            .ok_or_else(|| DbError::Storage("row id space is exhausted".to_string()))?;
        live.next_row_id = next_row_id;
        let catalog_page = live.catalog_page;
        let catalog_slot = live.catalog_slot;
        self.patch_catalog(pager, catalog_page, catalog_slot, next_row_id, head)?;
        Ok(())
    }

    fn rewrite_row(
        &mut self,
        pager: &mut Pager,
        name: &str,
        index: usize,
        row_id: u64,
        values: &[Value],
    ) -> Result<(), DbError> {
        let link = self.snapshot.tables[name].links[index];
        self.ensure(pager, link.page)?;
        let (prev, next) = self.read_links(link)?;
        let bytes = record::encode_record(&StoredRecord {
            row_id,
            prev,
            next,
            values: values.to_vec(),
        })?;
        self.ensure(pager, link.page)?;
        let old_len = page::cell(&self.pages[&link.page], link.slot)?.len();
        if bytes.len() <= old_len {
            page::overwrite_cell(
                self.pages.get_mut(&link.page).expect("page"),
                link.slot,
                &bytes,
            )?;
            self.dirty.insert(link.page);
            return Ok(());
        }
        self.delete_cell(pager, link)?;
        self.ensure(pager, link.page)?;
        let relocated =
            match page::insert_cell(self.pages.get_mut(&link.page).expect("page"), &bytes) {
                Ok(slot) => {
                    self.dirty.insert(link.page);
                    Link {
                        page: link.page,
                        slot,
                    }
                }
                Err(page::InsertError::Full) => {
                    let page_id = self.alloc_heap(pager)?;
                    let slot = self.insert_new_heap(page_id, &bytes)?;
                    Link {
                        page: page_id,
                        slot,
                    }
                }
                Err(page::InsertError::TooBig) => {
                    return Err(DbError::Storage(format!(
                        "row exceeds the maximum record size of {} bytes",
                        page::MAX_RECORD_LEN
                    )));
                }
            };
        self.snapshot.tables.get_mut(name).expect("table").links[index] = relocated;
        if prev != NULL_LINK {
            self.ensure(pager, prev.page)?;
            let (prev_prev, _) = self.read_links(prev)?;
            self.patch(pager, prev, prev_prev, relocated)?;
        }
        if next != NULL_LINK {
            self.ensure(pager, next.page)?;
            let (_, next_next) = self.read_links(next)?;
            self.patch(pager, next, relocated, next_next)?;
        }
        let (head_changed, tail_changed) = {
            let live = &mut self.snapshot.tables.get_mut(name).expect("table");
            let mut head_changed = false;
            let mut tail_changed = false;
            if live.head == link {
                live.head = relocated;
                head_changed = true;
            }
            if live.tail == link {
                live.tail = relocated;
                tail_changed = true;
            }
            (head_changed, tail_changed)
        };
        if head_changed || tail_changed {
            let live = &self.snapshot.tables[name];
            self.patch_catalog(
                pager,
                live.catalog_page,
                live.catalog_slot,
                live.next_row_id,
                live.head,
            )?;
        }
        self.maybe_free_heap(link.page)?;
        Ok(())
    }

    fn unlink_row(&mut self, pager: &mut Pager, name: &str, index: usize) -> Result<(), DbError> {
        let link = self.snapshot.tables[name].links[index];
        let prev = if index == 0 {
            NULL_LINK
        } else {
            self.snapshot.tables[name].links[index - 1]
        };
        let next = if index + 1 == self.snapshot.tables[name].links.len() {
            NULL_LINK
        } else {
            self.snapshot.tables[name].links[index + 1]
        };
        if prev != NULL_LINK {
            self.ensure(pager, prev.page)?;
            let (prev_prev, _) = self.read_links(prev)?;
            self.patch(pager, prev, prev_prev, next)?;
        }
        if next != NULL_LINK {
            self.ensure(pager, next.page)?;
            let (_, next_next) = self.read_links(next)?;
            self.patch(pager, next, prev, next_next)?;
        }
        self.delete_cell(pager, link)?;
        let live = self.snapshot.tables.get_mut(name).expect("table");
        live.links.remove(index);
        live.row_ids.remove(index);
        live.head = live.links.first().copied().unwrap_or(NULL_LINK);
        live.tail = live.links.last().copied().unwrap_or(NULL_LINK);
        let head = live.head;
        let next_row_id = live.next_row_id;
        let catalog_page = live.catalog_page;
        let catalog_slot = live.catalog_slot;
        self.patch_catalog(pager, catalog_page, catalog_slot, next_row_id, head)?;
        self.maybe_free_heap(link.page)?;
        Ok(())
    }

    fn set_next_row_id(
        &mut self,
        pager: &mut Pager,
        name: &str,
        next_row_id: u64,
    ) -> Result<(), DbError> {
        let live = self.snapshot.tables.get_mut(name).expect("table");
        if live.next_row_id == next_row_id {
            return Ok(());
        }
        live.next_row_id = next_row_id;
        let head = live.head;
        let catalog_page = live.catalog_page;
        let catalog_slot = live.catalog_slot;
        self.patch_catalog(pager, catalog_page, catalog_slot, next_row_id, head)
    }

    fn insert_catalog(&mut self, pager: &mut Pager, bytes: &[u8]) -> Result<(u32, u16), DbError> {
        let tail = self.snapshot.catalog_tail;
        self.ensure(pager, tail)?;
        match page::insert_cell(self.pages.get_mut(&tail).expect("catalog"), bytes) {
            Ok(slot) => {
                self.dirty.insert(tail);
                Ok((tail, slot))
            }
            Err(page::InsertError::Full) => {
                let page_id = self.alloc_page()?;
                let mut page = Page::new();
                page::init_slotted(&mut page, page::TYPE_CATALOG, page_id);
                let slot = match page::insert_cell(&mut page, bytes) {
                    Ok(slot) => slot,
                    Err(_) => {
                        return Err(DbError::Storage(format!(
                            "table definition exceeds the maximum record size of {} bytes",
                            page::MAX_RECORD_LEN
                        )));
                    }
                };
                self.pages.insert(page_id, page);
                self.dirty.insert(page_id);
                self.ensure(pager, tail)?;
                page::set_next_page(self.pages.get_mut(&tail).expect("catalog"), page_id);
                self.dirty.insert(tail);
                self.snapshot.catalog_tail = page_id;
                Ok((page_id, slot))
            }
            Err(page::InsertError::TooBig) => Err(DbError::Storage(format!(
                "table definition exceeds the maximum record size of {} bytes",
                page::MAX_RECORD_LEN
            ))),
        }
    }

    fn delete_catalog_cell(
        &mut self,
        pager: &mut Pager,
        page_id: u32,
        slot: u16,
    ) -> Result<(), DbError> {
        self.delete_cell(
            pager,
            Link {
                page: page_id,
                slot,
            },
        )?;
        self.ensure(pager, page_id)?;
        if page::live_slots(&self.pages[&page_id])?.is_empty()
            && page_id != self.snapshot.catalog_root
        {
            self.unlink_catalog_page(pager, page_id)?;
            self.free_page(page_id)?;
        }
        Ok(())
    }

    fn unlink_catalog_page(&mut self, pager: &mut Pager, page_id: u32) -> Result<(), DbError> {
        let mut previous = 0;
        let mut current = self.snapshot.catalog_root;
        while current != 0 {
            self.ensure(pager, current)?;
            let next = page::next_page_of(&self.pages[&current]);
            if current == page_id {
                if previous == 0 {
                    return Err(DbError::Internal(
                        "cannot unlink the catalog root".to_string(),
                    ));
                }
                self.ensure(pager, previous)?;
                page::set_next_page(self.pages.get_mut(&previous).expect("catalog"), next);
                self.dirty.insert(previous);
                if self.snapshot.catalog_tail == page_id {
                    self.snapshot.catalog_tail = previous;
                }
                return Ok(());
            }
            previous = current;
            current = next;
        }
        Err(page::corrupt_file())
    }

    fn insert_new_heap(&mut self, page_id: u32, bytes: &[u8]) -> Result<u16, DbError> {
        let mut page = Page::new();
        page::init_slotted(&mut page, page::TYPE_HEAP, page_id);
        let slot = match page::insert_cell(&mut page, bytes) {
            Ok(slot) => slot,
            Err(_) => {
                return Err(DbError::Storage(format!(
                    "row exceeds the maximum record size of {} bytes",
                    page::MAX_RECORD_LEN
                )));
            }
        };
        self.pages.insert(page_id, page);
        self.dirty.insert(page_id);
        Ok(slot)
    }

    fn alloc_heap(&mut self, pager: &mut Pager) -> Result<u32, DbError> {
        let id = self.alloc_page()?;
        if id < self.base_count {
            self.ensure(pager, id)?;
        }
        Ok(id)
    }

    fn alloc_page(&mut self) -> Result<u32, DbError> {
        self.freelist_dirty = true;
        if let Some(id) = self.snapshot.free_ids.pop() {
            return Ok(id);
        }
        let id = self.snapshot.page_count;
        self.snapshot.page_count =
            self.snapshot.page_count.checked_add(1).ok_or_else(|| {
                DbError::Storage("database page id space is exhausted".to_string())
            })?;
        Ok(id)
    }

    fn free_page(&mut self, page_id: u32) -> Result<(), DbError> {
        let mut page = Page::new();
        page::write_free_data(&mut page, page_id);
        self.pages.insert(page_id, page);
        self.dirty.insert(page_id);
        self.snapshot.free_ids.push(page_id);
        self.snapshot.free_ids.sort_unstable();
        self.snapshot.free_ids.dedup();
        self.freelist_dirty = true;
        Ok(())
    }

    fn maybe_free_heap(&mut self, page_id: u32) -> Result<(), DbError> {
        if page_id == 0 || !self.pages.contains_key(&page_id) {
            return Ok(());
        }
        let still_used = self
            .snapshot
            .tables
            .values()
            .any(|table| table.links.iter().any(|link| link.page == page_id));
        if still_used {
            return Ok(());
        }
        if page::page_type_of(&self.pages[&page_id]) != page::TYPE_HEAP {
            return Ok(());
        }
        self.free_page(page_id)
    }

    fn delete_cell(&mut self, pager: &mut Pager, link: Link) -> Result<(), DbError> {
        self.ensure(pager, link.page)?;
        page::delete_cell(self.pages.get_mut(&link.page).expect("page"), link.slot)?;
        self.dirty.insert(link.page);
        Ok(())
    }

    fn patch(
        &mut self,
        pager: &mut Pager,
        link: Link,
        prev: Link,
        next: Link,
    ) -> Result<(), DbError> {
        self.ensure(pager, link.page)?;
        let cell = page::cell_mut(self.pages.get_mut(&link.page).expect("page"), link.slot)?;
        record::patch_links(cell, prev, next)?;
        self.dirty.insert(link.page);
        Ok(())
    }

    fn patch_catalog(
        &mut self,
        pager: &mut Pager,
        page_id: u32,
        slot: u16,
        next_row_id: u64,
        head: Link,
    ) -> Result<(), DbError> {
        self.ensure(pager, page_id)?;
        let cell = page::cell_mut(self.pages.get_mut(&page_id).expect("catalog"), slot)?;
        record::patch_catalog_head(cell, next_row_id, head)?;
        self.dirty.insert(page_id);
        Ok(())
    }

    fn read_links(&mut self, pager_link: Link) -> Result<(Link, Link), DbError> {
        let page = self.pages.get(&pager_link.page).ok_or_else(|| {
            DbError::Internal(format!("page {} is not in the edit set", pager_link.page))
        })?;
        let bytes = page::cell(page, pager_link.slot)?.to_vec();
        let record = record::decode_record(&bytes).map_err(|_| page::corrupt(pager_link.page))?;
        Ok((record.prev, record.next))
    }

    fn ensure(&mut self, pager: &mut Pager, id: u32) -> Result<(), DbError> {
        if self.pages.contains_key(&id) {
            return Ok(());
        }
        if id >= self.base_count {
            return Err(DbError::Internal(format!(
                "page {id} was allocated without an image"
            )));
        }
        let mut page = Page::new();
        pager.read_into(id, &mut page)?;
        self.pages.insert(id, page);
        Ok(())
    }

    fn finish(&mut self, pager: &mut Pager) -> Result<(), DbError> {
        if self.freelist_dirty || self.snapshot.inline_free.len() > page::META_FREE_CAPACITY {
            self.relayout()?;
        }
        self.write_meta(pager)?;
        let ids: Vec<u32> = self.dirty.iter().copied().collect();
        for id in ids {
            let page = self
                .pages
                .get_mut(&id)
                .ok_or_else(|| DbError::Internal(format!("dirty page {id} is missing")))?;
            page::seal(page);
            page::verify(page, id)?;
        }
        Ok(())
    }

    fn write_meta(&mut self, _pager: &mut Pager) -> Result<(), DbError> {
        let mut page = Page::new();
        page::write_meta(
            &mut page,
            page::Meta {
                version: page::FORMAT_VERSION,
                page_count: self.snapshot.page_count,
                catalog_root: self.snapshot.catalog_root,
                freelist_root: self.snapshot.freelist_root,
                nonce: self.snapshot.nonce,
                commit_id: self.snapshot.commit_id.checked_add(1).ok_or_else(|| {
                    DbError::Storage("database commit id space is exhausted".to_string())
                })?,
                next_table_id: self.snapshot.next_table_id,
            },
            &self.snapshot.inline_free,
        )?;
        self.pages.insert(0, page);
        self.dirty.insert(0);
        self.snapshot.commit_id =
            self.snapshot.commit_id.checked_add(1).ok_or_else(|| {
                DbError::Storage("database commit id space is exhausted".to_string())
            })?;
        Ok(())
    }

    fn relayout(&mut self) -> Result<(), DbError> {
        let old_directories = self.snapshot.directory_pages.clone();
        let mut pool = self.snapshot.free_ids.clone();
        pool.extend(old_directories.iter().copied());
        pool.sort_unstable();
        pool.dedup();
        let need = directories_needed(pool.len());
        if need > pool.len() {
            return Err(DbError::Internal(
                "cannot reserve freelist directory pages".to_string(),
            ));
        }
        let directories: Vec<u32> = pool.split_off(pool.len() - need);
        let data = pool;
        let inline_len = data.len().min(page::META_FREE_CAPACITY);
        self.snapshot.inline_free = data[..inline_len].to_vec();
        let overflow = data[inline_len..].to_vec();
        let chunks: Vec<&[u32]> = overflow.chunks(FREE_DIRECTORY_CAPACITY).collect();
        if chunks.len() > directories.len() {
            return Err(DbError::Internal(
                "freelist directory count does not match the free-page list".to_string(),
            ));
        }
        for (index, directory_id) in directories.iter().copied().enumerate() {
            let next = directories.get(index + 1).copied().unwrap_or(0);
            let chunk = chunks.get(index).copied().unwrap_or(&[]);
            let mut page = Page::new();
            page::write_free_directory(&mut page, directory_id, next, chunk)?;
            self.pages.insert(directory_id, page);
            self.dirty.insert(directory_id);
        }
        for id in &data {
            if old_directories.contains(id) {
                let mut page = Page::new();
                page::write_free_data(&mut page, *id);
                self.pages.insert(*id, page);
                self.dirty.insert(*id);
            }
        }
        self.snapshot.free_ids = data;
        self.snapshot.directory_pages = directories;
        self.snapshot.freelist_root = self.snapshot.directory_pages.first().copied().unwrap_or(0);
        Ok(())
    }

    fn next_table_id(&mut self) -> Result<u32, DbError> {
        let max_existing = self
            .snapshot
            .tables
            .values()
            .map(|table| table.table_id)
            .max()
            .unwrap_or(0);
        let floor = max_existing
            .checked_add(1)
            .ok_or_else(|| DbError::Storage("table id space is exhausted".to_string()))?;
        let id = self.snapshot.next_table_id.max(floor);
        if id == 0 {
            return Err(DbError::Internal(
                "table id counter was not initialized".to_string(),
            ));
        }
        self.snapshot.next_table_id = id
            .checked_add(1)
            .ok_or_else(|| DbError::Storage("table id space is exhausted".to_string()))?;
        Ok(id)
    }
}

fn directories_needed(unused: usize) -> usize {
    if unused <= page::META_FREE_CAPACITY {
        0
    } else {
        (unused - page::META_FREE_CAPACITY).div_ceil(FREE_DIRECTORY_CAPACITY + 1)
    }
}

fn is_subsequence(needle: &[u64], haystack: &[u64]) -> bool {
    let mut hay = haystack.iter();
    needle.iter().all(|id| hay.any(|candidate| candidate == id))
}
