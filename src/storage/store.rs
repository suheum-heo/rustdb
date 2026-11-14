//! Catalog and heap images.
//!
//! Each publish builds new heap and catalog pages, then asks the pager to
//! replace the file. Pages that belonged to the previous image become free
//! data pages and can be reused by a later publish. A row id is assigned when
//! the row is inserted and is stored in the record; updates keep it.

use std::collections::HashSet;
use std::path::Path;

use crate::error::DbError;
use crate::schema::{Column, Schema};
use crate::storage::page::{self, InsertError, PAGE_SIZE, Page};
use crate::storage::pager::{Image, Pager};
use crate::storage::record::{
    self, CatalogEntry, Link, NULL_LINK, StoredRecord, ensure_record_fits,
};
use crate::value::Value;

const FREE_DIRECTORY_CAPACITY: usize = (PAGE_SIZE - 28) / 4;

/// One table, in the order its rows should be stored.
#[derive(Debug)]
pub(crate) struct TableImage {
    pub name: String,
    pub columns: Vec<Column>,
    pub next_row_id: u64,
    pub rows: Vec<(u64, Vec<Value>)>,
}

pub(crate) struct Store {
    pager: Pager,
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
        let images = load(&mut pager)?;
        Ok((Self { pager }, images))
    }

    pub(crate) fn publication_visible(&self) -> bool {
        self.pager.publication_visible()
    }

    pub(crate) fn publish(&mut self, tables: &[TableImage]) -> Result<(), DbError> {
        self.pager.begin_publish();
        let mut ordered: Vec<&TableImage> = tables.iter().collect();
        ordered.sort_by(|left, right| left.name.cmp(&right.name));
        validate(&ordered)?;

        let mut image = self.pager.begin_image();
        let heads = write_heaps(&mut image, &ordered)?;
        let catalog_root = write_catalog(&mut image, &ordered, &heads)?;
        let (freelist_root, inline_free, reusable) = write_freelist(&mut image)?;
        let mut meta = Page::new();
        page::write_meta(
            &mut meta,
            page::Meta {
                page_count: image.next_id(),
                catalog_root,
                freelist_root,
            },
            &inline_free,
        )?;
        image.write(0, meta)?;
        let pages = image.into_pages()?;
        self.pager.commit(pages, reusable)
    }
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
        let table_id = u32::try_from(index + 1).map_err(|_| page::corrupt_file())?;
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

fn load(pager: &mut Pager) -> Result<Vec<TableImage>, DbError> {
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
    let mut heap_pages = HashSet::new();
    let mut names = HashSet::with_capacity(entries.len());
    let mut table_ids = HashSet::with_capacity(entries.len());
    for entry in entries {
        if entry.name.is_empty()
            || !names.insert(entry.name.clone())
            || !table_ids.insert(entry.table_id)
        {
            return Err(page::corrupt_file());
        }
        let columns = Schema::try_new(&entry.name, entry.columns.clone())
            .map_err(|_| page::corrupt_file())?
            .columns()
            .to_vec();
        let rows = walk_rows(
            pager,
            entry.head,
            &columns,
            meta.page_count,
            &mut heap_pages,
        )?;
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
        images.push(TableImage {
            name: entry.name,
            columns,
            next_row_id: entry.next_row_id,
            rows,
        });
    }

    let mut live = HashSet::new();
    live.insert(0);
    live.extend(catalog_pages);
    live.extend(directory_pages.iter().copied());
    live.extend(heap_pages);
    let free_set: HashSet<u32> = free_ids.iter().copied().collect();
    if live.iter().any(|id| free_set.contains(id)) {
        return Err(page::corrupt_file());
    }
    let mut reusable = free_ids;
    for id in 1..meta.page_count {
        if !live.contains(&id) && !free_set.contains(&id) {
            reusable.push(id);
        }
    }
    pager.set_reusable(reusable);
    Ok(images)
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

fn read_catalog(
    pager: &mut Pager,
    root: u32,
    page_count: u32,
) -> Result<(Vec<u32>, Vec<CatalogEntry>), DbError> {
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
            entries.push(record::decode_catalog(&bytes).map_err(|_| page::corrupt(page_id))?);
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

fn walk_rows(
    pager: &mut Pager,
    head: Link,
    columns: &[Column],
    page_count: u32,
    heap_pages: &mut HashSet<u32>,
) -> Result<Vec<(u64, Vec<Value>)>, DbError> {
    let mut rows = Vec::new();
    let mut current = head;
    let mut previous = NULL_LINK;
    let mut seen = HashSet::new();
    while current.page != 0 {
        if current.page >= page_count || !seen.insert((current.page, current.slot)) {
            return Err(page::corrupt_file());
        }
        heap_pages.insert(current.page);
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
        previous = current;
        current = record.next;
        rows.push((record.row_id, record.values));
    }
    Ok(rows)
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
        bytes[32..36].copy_from_slice(&2u32.to_le_bytes());
        let mut page = Page::new();
        page.as_mut_slice().copy_from_slice(&bytes[..PAGE_SIZE]);
        page::seal(&mut page);
        bytes[..PAGE_SIZE].copy_from_slice(page.as_slice());
        fs::write(file.path(), &bytes).unwrap();
        let error = Store::open(file.path()).unwrap_err();
        assert_eq!(error.to_string(), "unsupported database format version 2");

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
        bytes[56..60].copy_from_slice(&2u32.to_le_bytes());
        bytes[60..64].copy_from_slice(&2u32.to_le_bytes());
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
}
