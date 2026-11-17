//! Page edits for one statement.
//!
//! The editor copies a page into memory the first time that statement changes
//! it. Unchanged pages stay out of the dirty set, so the redo log does not
//! contain them.

use std::collections::{HashMap, HashSet};

use crate::error::DbError;
use crate::storage::btree::{self, Nodes};
use crate::storage::page::{self, PAGE_SIZE, Page};
use crate::storage::pager::Pager;
use crate::storage::profile;
use crate::storage::record::{self, IndexCatalog, Link, NULL_LINK, StoredRecord};
use crate::storage::store::{IndexImage, MemoryRows, TableImage};
use crate::value::{DataType, Value};

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
pub(crate) struct LiveIndex {
    pub name: String,
    pub table_id: u32,
    pub column_ordinal: u16,
    pub column_name: String,
    pub root: u32,
    pub catalog_page: u32,
    pub catalog_slot: u16,
}

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub version: u32,
    pub nonce: u64,
    pub commit_id: u64,
    pub page_count: u32,
    pub catalog_root: u32,
    pub freelist_root: u32,
    pub index_catalog_root: u32,
    pub inline_free: Vec<u32>,
    pub free_ids: Vec<u32>,
    pub directory_pages: Vec<u32>,
    pub tables: HashMap<String, LiveTable>,
    pub indexes: Vec<LiveIndex>,
    pub catalog_tail: u32,
    pub index_catalog_tail: u32,
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
        indexes: &[IndexImage],
    ) -> Result<Option<Prepared>, DbError> {
        let desired_names: HashSet<&str> =
            desired.iter().map(|table| table.name.as_str()).collect();
        self.drop_absent_indexes(pager, indexes)?;
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
        self.create_missing_indexes(pager, indexes)?;
        self.prepared(pager)
    }

    /// Rewrite `changes` and leave every other stored row unread.
    ///
    /// A value that already matches its slot does not dirty a page. A larger
    /// record uses the existing relocate path. Index pages are dirtied only
    /// when an integer key changes. The returned pages are one atomic commit.
    pub(crate) fn rewrite_changes(
        mut self,
        pager: &mut Pager,
        table: &str,
        changes: &[(u64, Vec<Value>)],
    ) -> Result<Option<Prepared>, DbError> {
        if changes.is_empty() {
            return Ok(None);
        }
        let positions = {
            let live = self.snapshot.tables.get(table).ok_or_else(|| {
                DbError::Internal(format!("updated table '{table}' is not stored"))
            })?;
            let mut positions = HashMap::with_capacity(live.row_ids.len());
            for (index, row_id) in live.row_ids.iter().copied().enumerate() {
                if positions.insert(row_id, index).is_some() {
                    return Err(DbError::Internal(format!(
                        "stored table '{table}' repeats row {row_id}"
                    )));
                }
            }
            positions
        };
        let mut seen = HashSet::with_capacity(changes.len());
        for (row_id, values) in changes {
            if !seen.insert(*row_id) {
                return Err(DbError::Internal(format!(
                    "update lists row {row_id} twice in '{table}'"
                )));
            }
            let Some(index) = positions.get(row_id).copied() else {
                return Err(DbError::Internal(format!(
                    "updated row {row_id} is not stored in '{table}'"
                )));
            };
            self.rewrite_row(pager, table, index, *row_id, values)?;
        }
        self.prepared(pager)
    }

    /// Unlink `deleted` and compare `scans`.
    ///
    /// The ids were captured before compaction. They are checked against the
    /// stored snapshot before any row is removed: a duplicate, a zero, an id
    /// past the counter, or an id that is not stored fails with no page
    /// marked dirty. Surviving ids stay in insertion order. `next_row_id` is
    /// left unchanged. `scans` are tables whose stored ids still match memory,
    /// including a delete that matched nothing. Those rows are decoded and
    /// compared, which is the check that a stored record still matches memory.
    /// A survivor of a real delete is not in `scans` and is not cloned.
    ///
    /// One deleted row uses the per-row unlink: it cannot share a leaf with
    /// another deleted posting, and cloning both heap chains is slower than
    /// one removal. Two or more rows remove the heap slots in one pass.
    /// Index postings that share a leaf are removed together when that leaf
    /// stays at its minimum, and the rest use the single-entry delete. A test
    /// can force the per-row loop.
    pub(crate) fn unlink_deleted(
        mut self,
        pager: &mut Pager,
        table: &str,
        deleted: &[u64],
        scans: &[MemoryRows<'_>],
        indexes: &[IndexImage],
    ) -> Result<Option<Prepared>, DbError> {
        if !self.snapshot.tables.contains_key(table) {
            return Err(DbError::Internal(format!(
                "deleted table '{table}' is not stored"
            )));
        }
        self.drop_absent_indexes(pager, indexes)?;
        let validate_mark = profile::start();
        let positions = if deleted.is_empty() {
            Vec::new()
        } else {
            let live = &self.snapshot.tables[table];
            deleted_positions(live, table, deleted)?
        };
        profile::add_validate(validate_mark);
        for scan in scans {
            self.sync_stored_rows(pager, scan)?;
        }
        if !positions.is_empty() {
            if per_row_unlink() || positions.len() == 1 {
                for index in positions {
                    self.unlink_row(pager, table, index)?;
                }
            } else {
                self.unlink_batch(pager, table, &positions)?;
            }
        }
        self.create_missing_indexes(pager, indexes)?;
        self.prepared(pager)
    }

    /// Decode every stored row and rewrite it when memory differs.
    ///
    /// A delete that matched nothing still runs this. The decode reports a
    /// corrupt heap record and a stored value that no longer matches memory.
    /// The in-memory audit does not read the page, so an empty id list is not
    /// a reason to skip it. A stored row that already matches does not dirty
    /// its page.
    fn sync_stored_rows(
        &mut self,
        pager: &mut Pager,
        scan: &MemoryRows<'_>,
    ) -> Result<(), DbError> {
        let count = {
            let live = self.snapshot.tables.get(scan.name).ok_or_else(|| {
                DbError::Internal(format!("stored table '{}' is missing", scan.name))
            })?;
            if live.row_ids.len() != scan.rows.len() || live.links.len() != scan.rows.len() {
                return Err(DbError::Internal(format!(
                    "stored table '{}' does not match memory",
                    scan.name
                )));
            }
            live.row_ids.len()
        };
        for index in 0..count {
            let row_id = self.snapshot.tables[scan.name].row_ids[index];
            let values = &scan.rows[index];
            self.rewrite_row(pager, scan.name, index, row_id, values)?;
        }
        Ok(())
    }

    fn prepared(mut self, pager: &mut Pager) -> Result<Option<Prepared>, DbError> {
        if self.dirty.is_empty() && !self.freelist_dirty {
            return Ok(None);
        }
        let gather_mark = profile::start();
        self.finish(pager)?;
        let dirty = std::mem::take(&mut self.dirty);
        let mut pages: Vec<(u32, Page)> = self
            .pages
            .into_iter()
            .filter(|(id, _)| dirty.contains(id))
            .collect();
        pages.sort_by_key(|(id, _)| *id);
        profile::add_page_gather(gather_mark);
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
        let index_positions: Vec<usize> = self
            .snapshot
            .indexes
            .iter()
            .enumerate()
            .filter(|(_, index)| index.table_id == live.table_id)
            .map(|(position, _)| position)
            .rev()
            .collect();
        for position in index_positions {
            self.drop_index_at(pager, position)?;
        }
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
                self.rewrite_row(pager, &desired.name, index, *row_id, values)?;
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
            self.touch(tail.page)?;
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
        let table_id = live.table_id;
        self.patch_catalog(pager, catalog_page, catalog_slot, next_row_id, head)?;
        self.maintain_indexes(pager, table_id, row_id, None, Some(values))?;
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
        let table_id = self.snapshot.tables[name].table_id;
        // Compare and retain the complete record before any edit. Nothing can
        // change this slot between this decode and its rewrite. A later row
        // still decodes its current image, including earlier neighbor patches.
        let mark = profile::start();
        self.ensure(pager, link.page)?;
        let bytes = page::cell(&self.pages[&link.page], link.slot)?.to_vec();
        let old = record::decode_record(&bytes).map_err(|_| page::corrupt(link.page))?;
        let same = old.values == values;
        profile::note_row_compare();
        profile::add_compare(mark);
        if same {
            return Ok(());
        }
        let (old_values, prev, next) = if repeat_update_decode() {
            drop(old);
            let values = self.row_values(pager, link)?;
            let (prev, next) = self.read_links(link)?;
            (values, prev, next)
        } else {
            (old.values, old.prev, old.next)
        };
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
            self.touch(link.page)?;
            self.maintain_indexes(pager, table_id, row_id, Some(&old_values), Some(values))?;
            return Ok(());
        }
        self.delete_cell(pager, link)?;
        self.ensure(pager, link.page)?;
        let relocated =
            match page::insert_cell(self.pages.get_mut(&link.page).expect("page"), &bytes) {
                Ok(slot) => {
                    self.touch(link.page)?;
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
        self.maintain_indexes(pager, table_id, row_id, Some(&old_values), Some(values))?;
        Ok(())
    }

    fn unlink_row(&mut self, pager: &mut Pager, name: &str, index: usize) -> Result<(), DbError> {
        let link = self.snapshot.tables[name].links[index];
        let table_id = self.snapshot.tables[name].table_id;
        let row_id = self.snapshot.tables[name].row_ids[index];
        let decode_mark = profile::start();
        let old_values = self.row_values(pager, link)?;
        profile::add_decode(decode_mark);
        let heap_mark = profile::start();
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
        profile::add_heap_edit(heap_mark);
        let index_mark = profile::start();
        self.maintain_indexes(pager, table_id, row_id, Some(&old_values), None)?;
        profile::add_index_edit(index_mark);
        Ok(())
    }

    /// Remove every listed slot, then its postings.
    ///
    /// `positions` is the descending stored indexes from [`deleted_positions`].
    /// Keys are decoded before any slot is cleared. The survivor vectors are
    /// built in one pass. Neighbor links are patched in that same descending
    /// order, so a dead cell's leftover bytes match the per-row unlink. Empty
    /// heap pages are released once, after the survivor list is installed.
    fn unlink_batch(
        &mut self,
        pager: &mut Pager,
        name: &str,
        positions: &[usize],
    ) -> Result<(), DbError> {
        let decode_mark = profile::start();
        let (table_id, links, row_ids, next_row_id, catalog_page, catalog_slot) = {
            let live = &self.snapshot.tables[name];
            (
                live.table_id,
                live.links.clone(),
                live.row_ids.clone(),
                live.next_row_id,
                live.catalog_page,
                live.catalog_slot,
            )
        };
        let mut doomed = vec![false; links.len()];
        for &index in positions {
            doomed[index] = true;
        }
        let mut next_alive = vec![links.len(); links.len()];
        let mut next_index = links.len();
        for index in (0..links.len()).rev() {
            next_alive[index] = next_index;
            if !doomed[index] {
                next_index = index;
            }
        }
        let ordinals: Vec<(usize, u16)> = self
            .snapshot
            .indexes
            .iter()
            .enumerate()
            .filter(|(_, index)| index.table_id == table_id)
            .map(|(position, index)| (position, index.column_ordinal))
            .collect();
        let mut postings = vec![Vec::new(); ordinals.len()];
        for &index in positions {
            let values = self.row_values(pager, links[index])?;
            for (slot, (_, ordinal)) in ordinals.iter().enumerate() {
                if let Some(key) = integer_at(&values, *ordinal)? {
                    postings[slot].push((key, row_ids[index]));
                }
            }
        }
        profile::add_decode(decode_mark);

        let heap_mark = profile::start();
        // A page that loses every row is replaced by a free-page image. Link
        // patches on that page do not survive the replacement. Pages that keep
        // a row, including a row of another table, still get every patch.
        let mut surviving_pages = HashSet::new();
        for (index, link) in links.iter().copied().enumerate() {
            if !doomed[index] {
                surviving_pages.insert(link.page);
            }
        }
        for (other, table) in &self.snapshot.tables {
            if other == name {
                continue;
            }
            for link in &table.links {
                surviving_pages.insert(link.page);
            }
        }
        let patch_doomed = patch_doomed_pages();
        for &index in positions {
            let prev = if index == 0 {
                NULL_LINK
            } else {
                links[index - 1]
            };
            let next = if next_alive[index] == links.len() {
                NULL_LINK
            } else {
                links[next_alive[index]]
            };
            if prev != NULL_LINK && (patch_doomed || surviving_pages.contains(&prev.page)) {
                self.ensure(pager, prev.page)?;
                let (prev_prev, _) = self.read_links(prev)?;
                self.patch(pager, prev, prev_prev, next)?;
            }
            if next != NULL_LINK && (patch_doomed || surviving_pages.contains(&next.page)) {
                self.ensure(pager, next.page)?;
                let (_, next_next) = self.read_links(next)?;
                self.patch(pager, next, prev, next_next)?;
            }
            if patch_doomed || surviving_pages.contains(&links[index].page) {
                self.delete_cell(pager, links[index])?;
            }
        }
        let mut kept_ids = Vec::with_capacity(links.len() - positions.len());
        let mut kept_links = Vec::with_capacity(links.len() - positions.len());
        for (index, link) in links.iter().copied().enumerate() {
            if doomed[index] {
                continue;
            }
            kept_ids.push(row_ids[index]);
            kept_links.push(link);
        }
        let head = kept_links.first().copied().unwrap_or(NULL_LINK);
        let tail = kept_links.last().copied().unwrap_or(NULL_LINK);
        {
            let live = self.snapshot.tables.get_mut(name).expect("table");
            live.row_ids = kept_ids;
            live.links = kept_links;
            live.head = head;
            live.tail = tail;
        }
        self.patch_catalog(pager, catalog_page, catalog_slot, next_row_id, head)?;
        let mut released = HashSet::new();
        for &index in positions {
            let page_id = links[index].page;
            if surviving_pages.contains(&page_id) || !released.insert(page_id) {
                continue;
            }
            // The free image does not depend on the deleted cells. Load the
            // page once so a non-heap page is left alone, then replace it.
            self.ensure(pager, page_id)?;
            self.maybe_free_heap(page_id)?;
        }
        profile::add_heap_edit(heap_mark);

        let index_mark = profile::start();
        for (slot, (position, _)) in ordinals.into_iter().enumerate() {
            self.delete_postings(pager, position, &postings[slot])?;
        }
        profile::add_index_edit(index_mark);
        Ok(())
    }

    fn delete_postings(
        &mut self,
        pager: &mut Pager,
        position: usize,
        entries: &[(i64, u64)],
    ) -> Result<(), DbError> {
        if entries.is_empty() {
            return Ok(());
        }
        profile::note_index_updated();
        let root = self.snapshot.indexes[position].root;
        let new_root = {
            let mut nodes = EditNodes {
                editor: self,
                pager,
            };
            btree::delete_many(&mut nodes, root, entries, btree::production())?
        };
        if new_root != root {
            self.snapshot.indexes[position].root = new_root;
            self.rewrite_index_cell(pager, position)?;
        }
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
                self.touch(tail)?;
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
                self.touch(page_id)?;
                self.ensure(pager, tail)?;
                page::set_next_page(self.pages.get_mut(&tail).expect("catalog"), page_id);
                self.touch(tail)?;
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
                self.touch(previous)?;
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
        self.touch(page_id)?;
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
        self.touch(page_id)?;
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
        self.touch(link.page)?;
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
        self.touch(link.page)?;
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
        self.touch(page_id)?;
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
        let capacity = inline_capacity(self.snapshot.version);
        if self.freelist_dirty || self.snapshot.inline_free.len() > capacity {
            self.relayout()?;
        }
        self.write_meta(pager)?;
        let ids: Vec<u32> = self.dirty.iter().copied().collect();
        for id in ids {
            let page = self
                .pages
                .get_mut(&id)
                .ok_or_else(|| DbError::Internal(format!("dirty page {id} is missing")))?;
            match page::page_type_of(page) {
                page::TYPE_HEAP => profile::note_heap_page(),
                page::TYPE_INDEX_LEAF | page::TYPE_INDEX_INTERNAL | page::TYPE_INDEX_CATALOG => {
                    profile::note_index_page()
                }
                _ => profile::note_other_page(),
            }
            {
                let _seal = profile::Timer::new(|profile| &mut profile.page_seal);
                page::seal(page);
            }
            // `page` is the only reference to this image. `seal` just stored
            // the checksum of these bytes. The structural checks below still
            // run. A test can force the old checksum between the two.
            if page::checksum_recheck_forced() {
                let _recheck = profile::Timer::new(|profile| &mut profile.page_recheck);
                page::recheck_checksum(page, id)?;
            }
            {
                let _verify = profile::Timer::new(|profile| &mut profile.page_verify);
                page::validate_fresh_seal(page, id)?;
            }
        }
        Ok(())
    }

    fn write_meta(&mut self, _pager: &mut Pager) -> Result<(), DbError> {
        let mut page = Page::new();
        page::write_meta(
            &mut page,
            page::Meta {
                version: self.snapshot.version,
                page_count: self.snapshot.page_count,
                catalog_root: self.snapshot.catalog_root,
                freelist_root: self.snapshot.freelist_root,
                nonce: self.snapshot.nonce,
                commit_id: self.snapshot.commit_id.checked_add(1).ok_or_else(|| {
                    DbError::Storage("database commit id space is exhausted".to_string())
                })?,
                next_table_id: self.snapshot.next_table_id,
                index_catalog_root: self.snapshot.index_catalog_root,
            },
            &self.snapshot.inline_free,
        )?;
        self.pages.insert(0, page);
        self.touch(0)?;
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
        let capacity = inline_capacity(self.snapshot.version);
        let need = directories_needed(pool.len(), capacity);
        if need > pool.len() {
            return Err(DbError::Internal(
                "cannot reserve freelist directory pages".to_string(),
            ));
        }
        let directories: Vec<u32> = pool.split_off(pool.len() - need);
        let data = pool;
        let capacity = inline_capacity(self.snapshot.version);
        let inline_len = data.len().min(capacity);
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
            self.touch(directory_id)?;
        }
        for id in &data {
            if old_directories.contains(id) {
                let mut page = Page::new();
                page::write_free_data(&mut page, *id);
                self.pages.insert(*id, page);
                self.touch(*id)?;
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

    fn touch(&mut self, id: u32) -> Result<(), DbError> {
        if self.dirty.contains(&id) {
            return Ok(());
        }
        let reserved = usize::from(id != 0 && !self.dirty.contains(&0));
        if self.dirty.len() + 1 + reserved > page::commit_frame_limit() {
            return Err(DbError::Storage(
                "statement changes more pages than one commit can record".to_string(),
            ));
        }
        self.dirty.insert(id);
        Ok(())
    }

    fn drop_absent_indexes(
        &mut self,
        pager: &mut Pager,
        desired: &[IndexImage],
    ) -> Result<(), DbError> {
        let names: HashSet<&str> = desired.iter().map(|index| index.name.as_str()).collect();
        let positions: Vec<usize> = self
            .snapshot
            .indexes
            .iter()
            .enumerate()
            .filter(|(_, index)| !names.contains(index.name.as_str()))
            .map(|(position, _)| position)
            .rev()
            .collect();
        for position in positions {
            self.drop_index_at(pager, position)?;
        }
        Ok(())
    }

    fn create_missing_indexes(
        &mut self,
        pager: &mut Pager,
        desired: &[IndexImage],
    ) -> Result<(), DbError> {
        let mut names = HashSet::new();
        for index in desired {
            if !names.insert(index.name.as_str()) {
                return Err(DbError::invalid(format!(
                    "index '{}' already exists",
                    index.name
                )));
            }
            if self
                .snapshot
                .indexes
                .iter()
                .any(|live| live.name == index.name)
            {
                continue;
            }
            self.create_index(pager, index)?;
        }
        Ok(())
    }

    fn create_index(&mut self, pager: &mut Pager, index: &IndexImage) -> Result<(), DbError> {
        if self.snapshot.version < page::FORMAT_VERSION_V3 {
            return Err(DbError::Storage(
                "database format version 2 requires an explicit migration before CREATE INDEX"
                    .to_string(),
            ));
        }
        let table = self
            .snapshot
            .tables
            .get(&index.table)
            .ok_or_else(|| DbError::TableNotFound(index.table.clone()))?;
        let catalog_page = table.catalog_page;
        let catalog_slot = table.catalog_slot;
        let table_id = table.table_id;
        self.ensure(pager, catalog_page)?;
        let bytes = page::cell(&self.pages[&catalog_page], catalog_slot)?.to_vec();
        let entry = record::decode_catalog(&bytes).map_err(|_| page::corrupt(catalog_page))?;
        let Some(position) = entry
            .columns
            .iter()
            .position(|column| column.name() == index.column)
        else {
            return Err(DbError::ColumnNotFound {
                table: index.table.clone(),
                column: index.column.clone(),
            });
        };
        if entry.columns[position].data_type() != DataType::Integer {
            return Err(DbError::unsupported(format!(
                "index column '{}' must be INTEGER",
                index.column
            )));
        }
        let ordinal = u16::try_from(position).map_err(|_| page::corrupt_file())?;
        let cell = record::encode_index(&IndexCatalog {
            name: index.name.clone(),
            table_id,
            ordinal,
            column: index.column.clone(),
            root: 0,
        })?;
        let (page_id, slot) = self.insert_index_catalog(pager, &cell)?;
        self.snapshot.indexes.push(LiveIndex {
            name: index.name.clone(),
            table_id,
            column_ordinal: ordinal,
            column_name: index.column.clone(),
            root: 0,
            catalog_page: page_id,
            catalog_slot: slot,
        });
        let index_pos = self.snapshot.indexes.len() - 1;
        let row_count = self.snapshot.tables[&index.table].links.len();
        for row_index in 0..row_count {
            let row_id = self.snapshot.tables[&index.table].row_ids[row_index];
            let link = self.snapshot.tables[&index.table].links[row_index];
            let values = self.row_values(pager, link)?;
            if let Some(key) = integer_at(&values, ordinal)? {
                self.mutate_index(pager, index_pos, IndexChange::Insert(key, row_id))?;
            }
        }
        Ok(())
    }

    fn drop_index_at(&mut self, pager: &mut Pager, position: usize) -> Result<(), DbError> {
        let index = self.snapshot.indexes[position].clone();
        let owned = {
            let mut nodes = EditNodes {
                editor: self,
                pager,
            };
            btree::owned_pages(&mut nodes, index.root)?
        };
        for page_id in owned {
            self.free_page(page_id)?;
        }
        self.delete_index_cell(pager, index.catalog_page, index.catalog_slot)?;
        self.snapshot.indexes.remove(position);
        Ok(())
    }

    fn maintain_indexes(
        &mut self,
        pager: &mut Pager,
        table_id: u32,
        row_id: u64,
        old: Option<&[Value]>,
        new: Option<&[Value]>,
    ) -> Result<(), DbError> {
        let positions: Vec<usize> = self
            .snapshot
            .indexes
            .iter()
            .enumerate()
            .filter(|(_, index)| index.table_id == table_id)
            .map(|(position, _)| position)
            .collect();
        for position in positions {
            let ordinal = self.snapshot.indexes[position].column_ordinal;
            let old_key = old
                .map(|values| integer_at(values, ordinal))
                .transpose()?
                .flatten();
            let new_key = new
                .map(|values| integer_at(values, ordinal))
                .transpose()?
                .flatten();
            if old_key == new_key {
                continue;
            }
            profile::note_index_updated();
            if let Some(key) = old_key {
                self.mutate_index(pager, position, IndexChange::Delete(key, row_id))?;
            }
            if let Some(key) = new_key {
                self.mutate_index(pager, position, IndexChange::Insert(key, row_id))?;
            }
        }
        Ok(())
    }

    fn mutate_index(
        &mut self,
        pager: &mut Pager,
        position: usize,
        change: IndexChange,
    ) -> Result<(), DbError> {
        let root = self.snapshot.indexes[position].root;
        let new_root = {
            let mut nodes = EditNodes {
                editor: self,
                pager,
            };
            match change {
                IndexChange::Insert(key, row_id) => {
                    btree::insert(&mut nodes, root, key, row_id, btree::production())?
                }
                IndexChange::Delete(key, row_id) => {
                    btree::delete(&mut nodes, root, key, row_id, btree::production())?
                }
            }
        };
        if new_root != root {
            self.snapshot.indexes[position].root = new_root;
            self.rewrite_index_cell(pager, position)?;
        }
        Ok(())
    }

    fn rewrite_index_cell(&mut self, pager: &mut Pager, position: usize) -> Result<(), DbError> {
        let index = self.snapshot.indexes[position].clone();
        let bytes = record::encode_index(&IndexCatalog {
            name: index.name,
            table_id: index.table_id,
            ordinal: index.column_ordinal,
            column: index.column_name,
            root: index.root,
        })?;
        self.ensure(pager, index.catalog_page)?;
        page::overwrite_cell(
            self.pages
                .get_mut(&index.catalog_page)
                .expect("index catalog"),
            index.catalog_slot,
            &bytes,
        )?;
        self.touch(index.catalog_page)?;
        Ok(())
    }

    fn row_values(&mut self, pager: &mut Pager, link: Link) -> Result<Vec<Value>, DbError> {
        self.ensure(pager, link.page)?;
        let bytes = page::cell(&self.pages[&link.page], link.slot)?.to_vec();
        Ok(record::decode_record(&bytes)
            .map_err(|_| page::corrupt(link.page))?
            .values)
    }

    fn insert_index_catalog(
        &mut self,
        pager: &mut Pager,
        bytes: &[u8],
    ) -> Result<(u32, u16), DbError> {
        if self.snapshot.index_catalog_root == 0 {
            let page_id = self.alloc_page()?;
            if page_id < self.base_count {
                self.ensure(pager, page_id)?;
            }
            let mut page = Page::new();
            page::init_slotted(&mut page, page::TYPE_INDEX_CATALOG, page_id);
            let slot = page::insert_cell(&mut page, bytes).map_err(|_| {
                DbError::Storage(format!(
                    "index definition exceeds the maximum record size of {} bytes",
                    page::MAX_RECORD_LEN
                ))
            })?;
            self.pages.insert(page_id, page);
            self.touch(page_id)?;
            self.snapshot.index_catalog_root = page_id;
            self.snapshot.index_catalog_tail = page_id;
            return Ok((page_id, slot));
        }
        let tail = self.snapshot.index_catalog_tail;
        self.ensure(pager, tail)?;
        match page::insert_cell(self.pages.get_mut(&tail).expect("index catalog"), bytes) {
            Ok(slot) => {
                self.touch(tail)?;
                Ok((tail, slot))
            }
            Err(page::InsertError::Full) => {
                let page_id = self.alloc_page()?;
                let mut page = Page::new();
                page::init_slotted(&mut page, page::TYPE_INDEX_CATALOG, page_id);
                let slot = page::insert_cell(&mut page, bytes).map_err(|_| {
                    DbError::Storage(format!(
                        "index definition exceeds the maximum record size of {} bytes",
                        page::MAX_RECORD_LEN
                    ))
                })?;
                self.pages.insert(page_id, page);
                self.touch(page_id)?;
                self.ensure(pager, tail)?;
                page::set_next_page(self.pages.get_mut(&tail).expect("index catalog"), page_id);
                self.touch(tail)?;
                self.snapshot.index_catalog_tail = page_id;
                Ok((page_id, slot))
            }
            Err(page::InsertError::TooBig) => Err(DbError::Storage(format!(
                "index definition exceeds the maximum record size of {} bytes",
                page::MAX_RECORD_LEN
            ))),
        }
    }

    fn delete_index_cell(
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
        if page::live_slots(&self.pages[&page_id])?.is_empty() {
            self.unlink_index_page(pager, page_id)?;
            self.free_page(page_id)?;
        }
        Ok(())
    }

    fn unlink_index_page(&mut self, pager: &mut Pager, page_id: u32) -> Result<(), DbError> {
        self.ensure(pager, page_id)?;
        let next = page::next_page_of(&self.pages[&page_id]);
        if self.snapshot.index_catalog_root == page_id {
            self.snapshot.index_catalog_root = next;
            if self.snapshot.index_catalog_tail == page_id {
                self.snapshot.index_catalog_tail = next;
            }
            return Ok(());
        }
        let mut previous = 0;
        let mut current = self.snapshot.index_catalog_root;
        while current != 0 {
            self.ensure(pager, current)?;
            let following = page::next_page_of(&self.pages[&current]);
            if current == page_id {
                self.ensure(pager, previous)?;
                page::set_next_page(self.pages.get_mut(&previous).expect("index catalog"), next);
                self.touch(previous)?;
                if self.snapshot.index_catalog_tail == page_id {
                    self.snapshot.index_catalog_tail = previous;
                }
                return Ok(());
            }
            previous = current;
            current = following;
        }
        Err(page::corrupt_file())
    }
}

enum IndexChange {
    Insert(i64, u64),
    Delete(i64, u64),
}

struct EditNodes<'a> {
    editor: &'a mut Editor,
    pager: &'a mut Pager,
}

impl Nodes for EditNodes<'_> {
    fn read_node(&mut self, id: u32) -> Result<Page, DbError> {
        self.editor.ensure(self.pager, id)?;
        self.editor
            .pages
            .get(&id)
            .cloned()
            .ok_or_else(|| page::corrupt(id))
    }

    fn write_node(&mut self, id: u32, page: Page) -> Result<(), DbError> {
        self.editor.touch(id)?;
        self.editor.pages.insert(id, page);
        Ok(())
    }

    fn alloc_node(&mut self) -> Result<u32, DbError> {
        let id = self.editor.alloc_page()?;
        if id < self.editor.base_count {
            self.editor.ensure(self.pager, id)?;
        }
        Ok(id)
    }

    fn free_node(&mut self, id: u32) -> Result<(), DbError> {
        self.editor.free_page(id)
    }
}

fn integer_at(values: &[Value], ordinal: u16) -> Result<Option<i64>, DbError> {
    let Some(value) = values.get(usize::from(ordinal)) else {
        return Err(DbError::Internal(
            "indexed column is missing from the row".to_string(),
        ));
    };
    match value {
        Value::Null => Ok(None),
        Value::Integer(key) => Ok(Some(*key)),
        _ => Err(DbError::Internal(
            "indexed value is not an integer".to_string(),
        )),
    }
}

fn inline_capacity(version: u32) -> usize {
    if version >= page::FORMAT_VERSION_V3 {
        page::META_FREE_CAPACITY_V3
    } else {
        page::META_FREE_CAPACITY
    }
}

fn directories_needed(unused: usize, capacity: usize) -> usize {
    if unused <= capacity {
        0
    } else {
        (unused - capacity).div_ceil(FREE_DIRECTORY_CAPACITY + 1)
    }
}

#[cfg(test)]
thread_local! {
    static FORCE_PER_ROW_UNLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Unlinks one stored index at a time.
///
/// The default path removes the same slots in one pass. This guard keeps the
/// Milestone 29 loop so a test can compare the pages.
#[cfg(test)]
pub(crate) struct ForcePerRowUnlink;

#[cfg(test)]
impl ForcePerRowUnlink {
    pub(crate) fn new() -> Self {
        FORCE_PER_ROW_UNLINK.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForcePerRowUnlink {
    fn drop(&mut self) {
        FORCE_PER_ROW_UNLINK.with(|flag| flag.set(false));
    }
}

fn per_row_unlink() -> bool {
    #[cfg(test)]
    {
        FORCE_PER_ROW_UNLINK.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
thread_local! {
    static FORCE_DOOMED_PATCHES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Patch every deleted neighbor, including a page that this batch frees.
///
/// The default batch skips those patches. The free-page image replaces them.
/// This guard is the Milestone 42 comparison.
#[cfg(test)]
pub(crate) struct ForceDoomedHeapPatches;

#[cfg(test)]
impl ForceDoomedHeapPatches {
    pub(crate) fn new() -> Self {
        FORCE_DOOMED_PATCHES.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForceDoomedHeapPatches {
    fn drop(&mut self) {
        FORCE_DOOMED_PATCHES.with(|flag| flag.set(false));
    }
}

fn patch_doomed_pages() -> bool {
    #[cfg(test)]
    {
        FORCE_DOOMED_PATCHES.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
thread_local! {
    static REPEAT_UPDATE_DECODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Restore the M43 comparison, old-value decode, and link decode.
#[cfg(test)]
pub(crate) struct ForceRepeatedUpdateDecode(bool);

#[cfg(test)]
impl ForceRepeatedUpdateDecode {
    pub(crate) fn new() -> Self {
        Self(REPEAT_UPDATE_DECODE.with(|flag| flag.replace(true)))
    }
}

#[cfg(test)]
impl Drop for ForceRepeatedUpdateDecode {
    fn drop(&mut self) {
        REPEAT_UPDATE_DECODE.with(|flag| flag.set(self.0));
    }
}

fn repeat_update_decode() -> bool {
    #[cfg(test)]
    {
        REPEAT_UPDATE_DECODE.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// Stored indexes of `deleted`, highest first.
///
/// Checking finishes before any unlink. The indexes are unique and sorted so
/// the per-row loop can remove them from the high end without disturbing an
/// earlier index. The batched path uses the same indexes and does not shift
/// the vector once per row.
fn deleted_positions(
    live: &LiveTable,
    table: &str,
    deleted: &[u64],
) -> Result<Vec<usize>, DbError> {
    let mut positions = HashMap::with_capacity(live.row_ids.len());
    for (index, row_id) in live.row_ids.iter().copied().enumerate() {
        if positions.insert(row_id, index).is_some() {
            return Err(DbError::Internal(format!(
                "stored table '{table}' repeats row {row_id}"
            )));
        }
    }
    let mut seen = HashSet::with_capacity(deleted.len());
    let mut indexes = Vec::with_capacity(deleted.len());
    for row_id in deleted {
        if *row_id == 0 || *row_id >= live.next_row_id {
            return Err(DbError::Internal(format!(
                "deleted row {row_id} is outside the counter for table '{table}'"
            )));
        }
        if !seen.insert(*row_id) {
            return Err(DbError::Internal(format!(
                "delete lists row {row_id} twice in '{table}'"
            )));
        }
        let Some(index) = positions.get(row_id).copied() else {
            return Err(DbError::Internal(format!(
                "deleted row {row_id} is not stored in '{table}'"
            )));
        };
        indexes.push(index);
    }
    indexes.sort_unstable_by(|left, right| right.cmp(left));
    Ok(indexes)
}

fn is_subsequence(needle: &[u64], haystack: &[u64]) -> bool {
    let mut hay = haystack.iter();
    needle.iter().all(|id| hay.any(|candidate| candidate == id))
}
