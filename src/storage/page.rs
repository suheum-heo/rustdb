//! Fixed-size pages and the slotted layout used by catalog and heap pages.
//!
//! Integer fields are little-endian. The checksum is CRC-32/ISO-HDLC over the
//! page with the checksum field treated as zero. Rust struct layout is not
//! written to disk.

use crate::error::DbError;

/// Bytes in every database page.
pub const PAGE_SIZE: usize = 4096;

/// Bytes occupied by the header shared by every page.
pub(crate) const HEADER_LEN: usize = 24;

/// Bytes in one slot-directory entry.
pub(crate) const SLOT_LEN: usize = 4;

/// Largest cell that fits on an empty slotted page, including its slot entry.
pub const MAX_RECORD_LEN: usize = PAGE_SIZE - HEADER_LEN - SLOT_LEN;

pub(crate) const TYPE_META: u16 = 1;
pub(crate) const TYPE_CATALOG: u16 = 2;
pub(crate) const TYPE_HEAP: u16 = 3;
pub(crate) const TYPE_FREE: u16 = 4;
pub(crate) const TYPE_INDEX_CATALOG: u16 = 5;
pub(crate) const TYPE_INDEX_INTERNAL: u16 = 6;
pub(crate) const TYPE_INDEX_LEAF: u16 = 7;

pub(crate) const MAGIC: [u8; 8] = *b"RUSTDB\0\0";
/// Version written by this process. Version 1 is readable only by migration.
pub(crate) const FORMAT_VERSION: u32 = 2;

pub(crate) const FORMAT_VERSION_V1: u32 = 1;
pub(crate) const FORMAT_VERSION_V3: u32 = 3;

pub(crate) const OFF_TYPE: usize = 0;
pub(crate) const OFF_SLOTS: usize = 2;
pub(crate) const OFF_ID: usize = 4;
pub(crate) const OFF_CHECKSUM: usize = 8;
pub(crate) const OFF_NEXT: usize = 12;
pub(crate) const OFF_LOWER: usize = 16;
pub(crate) const OFF_UPPER: usize = 18;
pub(crate) const OFF_FLAGS: usize = 20;
pub(crate) const OFF_RESERVED: usize = 22;

const META_MAGIC: usize = 24;
const META_VERSION: usize = 32;
const META_PAGE_SIZE: usize = 36;
const META_PAGE_COUNT: usize = 40;
const META_CATALOG: usize = 44;
const META_FREELIST: usize = 48;
const META_FREE_COUNT: usize = 52;
const META_FREE_IDS_V1: usize = 56;
const META_NONCE: usize = 56;
const META_COMMIT_ID: usize = 64;
const META_FREE_IDS: usize = 72;
/// Table-id high-water mark. A version 2 writer reserves these four bytes.
const META_NEXT_TABLE_ID: usize = PAGE_SIZE - 4;
/// Index-catalog root. Version 3 reserves these four bytes, just before the
/// table-id mark, so a version 3 meta page holds one fewer inline free id.
const META_INDEX_ROOT: usize = PAGE_SIZE - 8;

/// Free page ids stored directly in a version 1 meta page.
pub(crate) const META_FREE_CAPACITY_V1: usize = (PAGE_SIZE - META_FREE_IDS_V1) / 4;

/// Inline free ids a version 2 writer stores. The last four bytes of the page
/// are the table-id high-water mark, so this is one id smaller than the
/// legacy tail.
pub(crate) const META_FREE_CAPACITY: usize = (META_NEXT_TABLE_ID - META_FREE_IDS) / 4;

/// Inline free ids on a version 3 meta page. Offset 4088 is the index catalog.
pub(crate) const META_FREE_CAPACITY_V3: usize = (META_INDEX_ROOT - META_FREE_IDS) / 4;

/// One redo-log commit cannot carry more frames than this.
pub(crate) const MAX_COMMIT_FRAMES: usize = 8192;

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_FRAME_CAP: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn commit_frame_limit() -> usize {
    #[cfg(test)]
    {
        let custom = TEST_FRAME_CAP.with(std::cell::Cell::get);
        if custom != 0 {
            return custom;
        }
    }
    MAX_COMMIT_FRAMES
}

/// Inline free ids a version 2 reader still accepts. A file written before the
/// high-water mark used all 1006 slots, and those last four bytes are a page
/// id rather than a table id.
pub(crate) const META_FREE_CAPACITY_LEGACY: usize = (PAGE_SIZE - META_FREE_IDS) / 4;

/// One owned page. The byte length is always [`PAGE_SIZE`].
#[derive(Clone)]
pub(crate) struct Page {
    bytes: Vec<u8>,
}

impl Page {
    pub(crate) fn new() -> Self {
        Self {
            bytes: vec![0; PAGE_SIZE],
        }
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Meta {
    pub version: u32,
    pub page_count: u32,
    pub catalog_root: u32,
    pub freelist_root: u32,
    /// Zero on a version 1 page. Version 2 uses a non-zero nonce.
    pub nonce: u64,
    /// Zero until the first committed statement. Version 1 pages report 0.
    pub commit_id: u64,
    /// Next table id to assign. Zero means a version 2 file that did not
    /// record one, or a version 1 page. A writer stores this at the end of
    /// the meta page. A zero mark has no deleted-id history.
    pub next_table_id: u32,
    /// Page id of the index catalog, or zero when the database has no indexes.
    /// Version 1 and 2 pages report zero.
    pub index_catalog_root: u32,
}

/// Why a cell could not be placed on this page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InsertError {
    /// The cell is larger than [`MAX_RECORD_LEN`].
    TooBig,
    /// The page has no usable hole and no contiguous free space left.
    Full,
}

pub(crate) fn corrupt(page_id: u32) -> DbError {
    DbError::Storage(format!("database page {page_id} is corrupt"))
}

pub(crate) fn corrupt_file() -> DbError {
    DbError::Storage("database file is corrupt".to_string())
}

pub(crate) fn init_slotted(page: &mut Page, page_type: u16, page_id: u32) {
    let bytes = page.as_mut_slice();
    bytes.fill(0);
    write_u16(bytes, OFF_TYPE, page_type);
    write_u16(bytes, OFF_SLOTS, 0);
    write_u32(bytes, OFF_ID, page_id);
    write_u32(bytes, OFF_NEXT, 0);
    write_u16(bytes, OFF_LOWER, HEADER_LEN as u16);
    write_u16(bytes, OFF_UPPER, PAGE_SIZE as u16);
}

const FREE_COUNT: usize = 24;
const FREE_IDS: usize = 28;

pub(crate) fn write_meta(page: &mut Page, meta: Meta, free_ids: &[u32]) -> Result<(), DbError> {
    let capacity = match meta.version {
        FORMAT_VERSION => META_FREE_CAPACITY,
        FORMAT_VERSION_V3 => META_FREE_CAPACITY_V3,
        _ => {
            return Err(DbError::Internal(
                "meta page version is not writable".to_string(),
            ));
        }
    };
    if free_ids.len() > capacity {
        return Err(DbError::Internal(
            "meta page cannot store this many free page ids".to_string(),
        ));
    }
    init_slotted(page, TYPE_META, 0);
    let bytes = page.as_mut_slice();
    bytes[META_MAGIC..META_MAGIC + MAGIC.len()].copy_from_slice(&MAGIC);
    if meta.nonce == 0 {
        return Err(DbError::Internal(
            "database nonce must be non-zero".to_string(),
        ));
    }
    if meta.next_table_id == 0 {
        return Err(DbError::Internal(
            "table id counter was not initialized".to_string(),
        ));
    }
    write_u32(bytes, META_VERSION, meta.version);
    write_u32(bytes, META_PAGE_SIZE, PAGE_SIZE as u32);
    write_u32(bytes, META_PAGE_COUNT, meta.page_count);
    write_u32(bytes, META_CATALOG, meta.catalog_root);
    write_u32(bytes, META_FREELIST, meta.freelist_root);
    let count = u32::try_from(free_ids.len()).map_err(|_| {
        DbError::Internal("meta page cannot store this many free page ids".to_string())
    })?;
    write_u32(bytes, META_FREE_COUNT, count);
    write_u64(bytes, META_NONCE, meta.nonce);
    write_u64(bytes, META_COMMIT_ID, meta.commit_id);
    for (index, id) in free_ids.iter().copied().enumerate() {
        write_u32(bytes, META_FREE_IDS + index * 4, id);
    }
    write_u32(bytes, META_NEXT_TABLE_ID, meta.next_table_id);
    if meta.version == FORMAT_VERSION_V3 {
        write_u32(bytes, META_INDEX_ROOT, meta.index_catalog_root);
    }
    Ok(())
}

pub(crate) fn read_meta(page: &Page) -> Result<Meta, DbError> {
    verify(page, 0)?;
    let bytes = page.as_slice();
    if read_u16(bytes, OFF_TYPE) != TYPE_META || read_u16(bytes, OFF_FLAGS) != 0 {
        return Err(DbError::Storage("invalid database header".to_string()));
    }
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&bytes[META_MAGIC..META_MAGIC + 8]);
    if magic != MAGIC {
        return Err(DbError::Storage("invalid database header".to_string()));
    }
    let version = read_u32(bytes, META_VERSION);
    if version != FORMAT_VERSION && version != FORMAT_VERSION_V1 && version != FORMAT_VERSION_V3 {
        return Err(DbError::Storage(format!(
            "unsupported database format version {version}"
        )));
    }
    if read_u32(bytes, META_PAGE_SIZE) != PAGE_SIZE as u32 {
        return Err(DbError::Storage("invalid database header".to_string()));
    }
    let page_count = read_u32(bytes, META_PAGE_COUNT);
    if page_count < 2 {
        return Err(DbError::Storage("invalid database header".to_string()));
    }
    let (nonce, commit_id, next_table_id, index_catalog_root, capacity) =
        if version == FORMAT_VERSION_V1 {
            (0, 0, 0, 0, META_FREE_CAPACITY_V1)
        } else if version == FORMAT_VERSION_V3 {
            let nonce = read_u64(bytes, META_NONCE);
            if nonce == 0 {
                return Err(corrupt(0));
            }
            (
                nonce,
                read_u64(bytes, META_COMMIT_ID),
                read_u32(bytes, META_NEXT_TABLE_ID),
                read_u32(bytes, META_INDEX_ROOT),
                META_FREE_CAPACITY_V3,
            )
        } else {
            let nonce = read_u64(bytes, META_NONCE);
            if nonce == 0 {
                return Err(corrupt(0));
            }
            let free_count = read_u32(bytes, META_FREE_COUNT) as usize;
            let next_table_id = if free_count >= META_FREE_CAPACITY_LEGACY {
                0
            } else {
                read_u32(bytes, META_NEXT_TABLE_ID)
            };
            (
                nonce,
                read_u64(bytes, META_COMMIT_ID),
                next_table_id,
                0,
                META_FREE_CAPACITY_LEGACY,
            )
        };
    let free_count = read_u32(bytes, META_FREE_COUNT) as usize;
    if free_count > capacity {
        return Err(corrupt(0));
    }
    Ok(Meta {
        version,
        page_count,
        catalog_root: read_u32(bytes, META_CATALOG),
        freelist_root: read_u32(bytes, META_FREELIST),
        nonce,
        commit_id,
        next_table_id,
        index_catalog_root,
    })
}

pub(crate) fn inline_free_ids(page: &Page) -> Result<Vec<u32>, DbError> {
    verify(page, 0)?;
    let bytes = page.as_slice();
    let version = read_u32(bytes, META_VERSION);
    let (start, capacity) = match version {
        FORMAT_VERSION_V1 => (META_FREE_IDS_V1, META_FREE_CAPACITY_V1),
        FORMAT_VERSION => (META_FREE_IDS, META_FREE_CAPACITY_LEGACY),
        FORMAT_VERSION_V3 => (META_FREE_IDS, META_FREE_CAPACITY_V3),
        _ => return Err(corrupt(0)),
    };
    let count = read_u32(bytes, META_FREE_COUNT) as usize;
    if count > capacity {
        return Err(corrupt(0));
    }
    let mut ids = Vec::with_capacity(count);
    for index in 0..count {
        ids.push(read_u32(bytes, start + index * 4));
    }
    Ok(ids)
}

pub(crate) fn migration_required(path: &std::path::Path) -> DbError {
    DbError::Storage(format!(
        "database format version 1 requires an explicit migration: rustdb --migrate {}",
        path.display()
    ))
}

pub(crate) fn write_free_data(page: &mut Page, page_id: u32) {
    init_slotted(page, TYPE_FREE, page_id);
    write_u32(page.as_mut_slice(), FREE_COUNT, 0);
}

pub(crate) fn write_free_directory(
    page: &mut Page,
    page_id: u32,
    next: u32,
    ids: &[u32],
) -> Result<(), DbError> {
    let capacity = (PAGE_SIZE - FREE_IDS) / 4;
    if ids.len() > capacity {
        return Err(DbError::Internal(
            "free-page directory exceeds one page".to_string(),
        ));
    }
    init_slotted(page, TYPE_FREE, page_id);
    let bytes = page.as_mut_slice();
    write_u32(bytes, OFF_NEXT, next);
    let count = u32::try_from(ids.len())
        .map_err(|_| DbError::Internal("free-page directory exceeds one page".to_string()))?;
    write_u32(bytes, FREE_COUNT, count);
    for (index, id) in ids.iter().copied().enumerate() {
        write_u32(bytes, FREE_IDS + index * 4, id);
    }
    Ok(())
}

pub(crate) fn read_free_directory(page: &Page, page_id: u32) -> Result<(u32, Vec<u32>), DbError> {
    verify(page, page_id)?;
    let bytes = page.as_slice();
    if read_u16(bytes, OFF_TYPE) != TYPE_FREE {
        return Err(corrupt(page_id));
    }
    let next = read_u32(bytes, OFF_NEXT);
    let count = read_u32(bytes, FREE_COUNT) as usize;
    let end = FREE_IDS
        .checked_add(count.checked_mul(4).ok_or_else(|| corrupt(page_id))?)
        .ok_or_else(|| corrupt(page_id))?;
    if end > PAGE_SIZE {
        return Err(corrupt(page_id));
    }
    let mut ids = Vec::with_capacity(count);
    for offset in (FREE_IDS..end).step_by(4) {
        ids.push(read_u32(bytes, offset));
    }
    Ok((next, ids))
}

pub(crate) fn page_id_of(page: &Page) -> u32 {
    read_u32(page.as_slice(), OFF_ID)
}

pub(crate) fn page_type_of(page: &Page) -> u16 {
    read_u16(page.as_slice(), OFF_TYPE)
}

pub(crate) fn next_page_of(page: &Page) -> u32 {
    read_u32(page.as_slice(), OFF_NEXT)
}

pub(crate) fn set_next_page(page: &mut Page, next: u32) {
    write_u32(page.as_mut_slice(), OFF_NEXT, next);
}

/// Largest payload `insert_cell` can still place, after a compaction.
pub(crate) fn max_payload(page: &Page) -> usize {
    let bytes = page.as_slice();
    let lower = read_u16(bytes, OFF_LOWER) as usize;
    let upper = read_u16(bytes, OFF_UPPER) as usize;
    let contiguous = upper.saturating_sub(lower);
    if has_free_slot(bytes) {
        contiguous
    } else {
        contiguous.saturating_sub(SLOT_LEN)
    }
}

pub(crate) fn insert_cell(page: &mut Page, payload: &[u8]) -> Result<u16, InsertError> {
    if payload.len() > MAX_RECORD_LEN {
        return Err(InsertError::TooBig);
    }
    if max_payload(page) < payload.len() {
        compact(page).map_err(|_| InsertError::Full)?;
    }
    if max_payload(page) < payload.len() {
        return Err(InsertError::Full);
    }

    let bytes = page.as_slice();
    let upper = read_u16(bytes, OFF_UPPER) as usize;
    let Some(offset) = upper.checked_sub(payload.len()) else {
        return Err(InsertError::Full);
    };
    let Ok(offset_u16) = u16::try_from(offset) else {
        return Err(InsertError::Full);
    };
    let Ok(length_u16) = u16::try_from(payload.len()) else {
        return Err(InsertError::TooBig);
    };
    let appended = match lowest_free_slot(bytes) {
        Some(slot) => (slot, None),
        None => {
            let slot = read_u16(bytes, OFF_SLOTS);
            let Some(next) = slot.checked_add(1) else {
                return Err(InsertError::Full);
            };
            let Some(lower) = (read_u16(bytes, OFF_LOWER) as usize).checked_add(SLOT_LEN) else {
                return Err(InsertError::Full);
            };
            let Ok(lower_u16) = u16::try_from(lower) else {
                return Err(InsertError::Full);
            };
            if slot_directory(slot).is_none() {
                return Err(InsertError::Full);
            }
            (slot, Some((next, lower_u16)))
        }
    };
    let (slot, appended) = appended;

    let bytes = page.as_mut_slice();
    bytes[offset..offset + payload.len()].copy_from_slice(payload);
    write_u16(bytes, OFF_UPPER, offset_u16);
    if let Some((next, lower)) = appended {
        write_u16(bytes, OFF_SLOTS, next);
        write_u16(bytes, OFF_LOWER, lower);
    }
    write_slot(bytes, slot, offset_u16, length_u16);
    Ok(slot)
}

/// Mark a slot free. Its bytes are reclaimed the next time the page is compacted.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn delete_cell(page: &mut Page, slot: u16) -> Result<(), DbError> {
    let page_id = page_id_of(page);
    let bytes = page.as_mut_slice();
    let count = read_u16(bytes, OFF_SLOTS);
    if slot >= count {
        return Err(corrupt(page_id));
    }
    write_slot(bytes, slot, 0, 0);
    Ok(())
}

/// Replace a cell with a payload that fits in its current slot.
pub(crate) fn overwrite_cell(page: &mut Page, slot: u16, payload: &[u8]) -> Result<(), DbError> {
    let page_id = page_id_of(page);
    let (offset, length) = occupied_slot(page.as_slice(), page_id, slot)?;
    if payload.len() > length {
        return Err(DbError::Internal(
            "replacement cell is larger than the slot".to_string(),
        ));
    }
    page.as_mut_slice()[offset..offset + payload.len()].copy_from_slice(payload);
    if payload.len() < length {
        let Ok(offset_u16) = u16::try_from(offset) else {
            return Err(corrupt(page_id));
        };
        let Ok(length_u16) = u16::try_from(payload.len()) else {
            return Err(corrupt(page_id));
        };
        write_slot(page.as_mut_slice(), slot, offset_u16, length_u16);
    }
    Ok(())
}

pub(crate) fn cell(page: &Page, slot: u16) -> Result<&[u8], DbError> {
    let page_id = page_id_of(page);
    let (offset, length) = occupied_slot(page.as_slice(), page_id, slot)?;
    Ok(&page.as_slice()[offset..offset + length])
}

pub(crate) fn cell_mut(page: &mut Page, slot: u16) -> Result<&mut [u8], DbError> {
    let page_id = page_id_of(page);
    let (offset, length) = occupied_slot(page.as_slice(), page_id, slot)?;
    Ok(&mut page.as_mut_slice()[offset..offset + length])
}

pub(crate) fn live_slots(page: &Page) -> Result<Vec<u16>, DbError> {
    let page_id = page_id_of(page);
    let bytes = page.as_slice();
    let count = read_u16(bytes, OFF_SLOTS) as usize;
    let mut slots = Vec::new();
    for slot in 0..count {
        let slot = u16::try_from(slot).map_err(|_| corrupt(page_id))?;
        if slot_entry(bytes, page_id, slot)?.is_some() {
            slots.push(slot);
        }
    }
    Ok(slots)
}

pub(crate) fn seal(page: &mut Page) {
    let bytes = page.as_mut_slice();
    write_u32(bytes, OFF_CHECKSUM, 0);
    let checksum = crc32(bytes);
    write_u32(bytes, OFF_CHECKSUM, checksum);
}

/// Recompute the CRC of a page that did not just come from [`seal`].
///
/// The CRC covers all [`PAGE_SIZE`] bytes. Bytes `8..12`, the stored
/// checksum, are treated as zero. File reads, redo-log replay, open, and
/// migration use this. A page the editor just sealed does not.
pub(crate) fn recheck_checksum(page: &Page, expected_id: u32) -> Result<(), DbError> {
    let bytes = page.as_slice();
    if bytes.len() != PAGE_SIZE {
        return Err(corrupt(expected_id));
    }
    let stored = read_u32(bytes, OFF_CHECKSUM);
    let mut cleared = bytes.to_vec();
    write_u32(&mut cleared, OFF_CHECKSUM, 0);
    if crc32(&cleared) != stored {
        return Err(corrupt(expected_id));
    }
    Ok(())
}

/// Header, page-type, slotted-directory, and index-node checks.
///
/// This does not recompute the checksum. [`verify`] runs it after
/// [`recheck_checksum`]. The editor's commit runs it on a page whose
/// checksum [`seal`] just wrote, while that editor still owns the page.
pub(crate) fn validate_fresh_seal(page: &Page, expected_id: u32) -> Result<(), DbError> {
    let bytes = page.as_slice();
    if bytes.len() != PAGE_SIZE {
        return Err(corrupt(expected_id));
    }
    if read_u32(bytes, OFF_ID) != expected_id
        || read_u16(bytes, OFF_FLAGS) != 0
        || read_u16(bytes, OFF_RESERVED) != 0
    {
        return Err(corrupt(expected_id));
    }
    let page_type = read_u16(bytes, OFF_TYPE);
    if !matches!(
        page_type,
        TYPE_META
            | TYPE_CATALOG
            | TYPE_HEAP
            | TYPE_FREE
            | TYPE_INDEX_CATALOG
            | TYPE_INDEX_INTERNAL
            | TYPE_INDEX_LEAF
    ) {
        return Err(corrupt(expected_id));
    }
    if matches!(page_type, TYPE_CATALOG | TYPE_HEAP | TYPE_INDEX_CATALOG) {
        verify_slotted(bytes, expected_id)?;
    }
    if matches!(page_type, TYPE_INDEX_INTERNAL | TYPE_INDEX_LEAF) {
        verify_index_node(bytes, expected_id, page_type)?;
    }
    Ok(())
}

pub(crate) fn verify(page: &Page, expected_id: u32) -> Result<(), DbError> {
    recheck_checksum(page, expected_id)?;
    validate_fresh_seal(page, expected_id)
}

#[cfg(test)]
thread_local! {
    static FORCE_CHECKSUM_RECHECK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Recompute the page checksum at commit, after `seal`, as Milestone 39 did.
///
/// The default commit checks page structure and keeps the checksum `seal`
/// just stored. This guard is the reference path for that comparison.
#[cfg(test)]
pub(crate) struct ForceChecksumRecheck;

#[cfg(test)]
impl ForceChecksumRecheck {
    pub(crate) fn new() -> Self {
        FORCE_CHECKSUM_RECHECK.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForceChecksumRecheck {
    fn drop(&mut self) {
        FORCE_CHECKSUM_RECHECK.with(|flag| flag.set(false));
    }
}

pub(crate) fn checksum_recheck_forced() -> bool {
    #[cfg(test)]
    {
        FORCE_CHECKSUM_RECHECK.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

fn verify_index_node(bytes: &[u8], page_id: u32, page_type: u16) -> Result<(), DbError> {
    if read_u16(bytes, OFF_SLOTS) != 0
        || read_u16(bytes, OFF_LOWER) != HEADER_LEN as u16
        || read_u16(bytes, OFF_UPPER) != HEADER_LEN as u16
        || read_u16(bytes, HEADER_LEN + 2) != 0
    {
        return Err(corrupt(page_id));
    }
    let count = usize::from(read_u16(bytes, HEADER_LEN));
    if page_type == TYPE_INDEX_LEAF {
        if count == 0 || count > 254 || page_id == 0 {
            return Err(corrupt(page_id));
        }
    } else if count == 0
        || count > 203
        || read_u32(bytes, OFF_NEXT) != 0
        || read_u32(bytes, PAGE_SIZE - 4) != 0
    {
        return Err(corrupt(page_id));
    }
    Ok(())
}

fn verify_slotted(bytes: &[u8], page_id: u32) -> Result<(), DbError> {
    let count = usize::from(read_u16(bytes, OFF_SLOTS));
    let lower = usize::from(read_u16(bytes, OFF_LOWER));
    let upper = usize::from(read_u16(bytes, OFF_UPPER));
    let directory_bytes = count
        .checked_mul(SLOT_LEN)
        .ok_or_else(|| corrupt(page_id))?;
    let directory_end = HEADER_LEN
        .checked_add(directory_bytes)
        .ok_or_else(|| corrupt(page_id))?;
    if lower != directory_end || lower > upper || upper > PAGE_SIZE {
        return Err(corrupt(page_id));
    }
    let mut ranges = Vec::new();
    for slot in 0..count {
        let directory = HEADER_LEN
            .checked_add(slot.checked_mul(SLOT_LEN).ok_or_else(|| corrupt(page_id))?)
            .ok_or_else(|| corrupt(page_id))?;
        if directory
            .checked_add(SLOT_LEN)
            .ok_or_else(|| corrupt(page_id))?
            > bytes.len()
        {
            return Err(corrupt(page_id));
        }
        let offset = usize::from(read_u16(bytes, directory));
        let length = usize::from(read_u16(bytes, directory + 2));
        if offset == 0 && length == 0 {
            continue;
        }
        let end = offset.checked_add(length).ok_or_else(|| corrupt(page_id))?;
        if length == 0 || offset < upper || end > PAGE_SIZE {
            return Err(corrupt(page_id));
        }
        ranges.push((offset, end));
    }
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(corrupt(page_id));
        }
    }
    Ok(())
}

fn compact(page: &mut Page) -> Result<(), DbError> {
    let page_id = page_id_of(page);
    let count = read_u16(page.as_slice(), OFF_SLOTS);
    let mut live = Vec::new();
    for slot in 0..count {
        if let Some((offset, length)) = slot_entry(page.as_slice(), page_id, slot)? {
            live.push((slot, page.as_slice()[offset..offset + length].to_vec()));
        }
    }
    let mut upper = PAGE_SIZE;
    for (slot, payload) in live.iter().rev() {
        let Some(next_upper) = upper.checked_sub(payload.len()) else {
            return Err(corrupt(page_id));
        };
        upper = next_upper;
        let Ok(offset_u16) = u16::try_from(upper) else {
            return Err(corrupt(page_id));
        };
        let Ok(length_u16) = u16::try_from(payload.len()) else {
            return Err(corrupt(page_id));
        };
        page.as_mut_slice()[upper..upper + payload.len()].copy_from_slice(payload);
        write_slot(page.as_mut_slice(), *slot, offset_u16, length_u16);
    }
    let lower = HEADER_LEN + usize::from(count) * SLOT_LEN;
    if lower > upper {
        return Err(corrupt(page_id));
    }
    let Ok(lower_u16) = u16::try_from(lower) else {
        return Err(corrupt(page_id));
    };
    write_u16(page.as_mut_slice(), OFF_LOWER, lower_u16);
    let Ok(upper_u16) = u16::try_from(upper) else {
        return Err(corrupt(page_id));
    };
    write_u16(page.as_mut_slice(), OFF_UPPER, upper_u16);
    Ok(())
}

fn has_free_slot(bytes: &[u8]) -> bool {
    lowest_free_slot(bytes).is_some()
}

fn lowest_free_slot(bytes: &[u8]) -> Option<u16> {
    let count = read_u16(bytes, OFF_SLOTS);
    (0..count).find(|slot| {
        let Some(directory) = slot_directory(*slot) else {
            return false;
        };
        read_u16(bytes, directory) == 0 && read_u16(bytes, directory + 2) == 0
    })
}

fn slot_directory(slot: u16) -> Option<usize> {
    let directory = HEADER_LEN.checked_add(usize::from(slot).checked_mul(SLOT_LEN)?)?;
    if directory.checked_add(SLOT_LEN)? > PAGE_SIZE {
        None
    } else {
        Some(directory)
    }
}

/// `Ok(None)` is a free slot. A free slot is offset 0 and length 0.
fn slot_entry(bytes: &[u8], page_id: u32, slot: u16) -> Result<Option<(usize, usize)>, DbError> {
    let count = read_u16(bytes, OFF_SLOTS);
    if slot >= count {
        return Err(corrupt(page_id));
    }
    let directory = match usize::from(slot)
        .checked_mul(SLOT_LEN)
        .and_then(|offset| HEADER_LEN.checked_add(offset))
        .and_then(|offset| offset.checked_add(SLOT_LEN))
    {
        Some(end) if end <= bytes.len() => end - SLOT_LEN,
        _ => return Err(corrupt(page_id)),
    };
    let offset = read_u16(bytes, directory) as usize;
    let length = read_u16(bytes, directory + 2) as usize;
    if offset == 0 && length == 0 {
        return Ok(None);
    }
    let end = offset.checked_add(length).ok_or_else(|| corrupt(page_id))?;
    if offset == 0 || length == 0 || end > PAGE_SIZE {
        return Err(corrupt(page_id));
    }
    Ok(Some((offset, length)))
}

fn occupied_slot(bytes: &[u8], page_id: u32, slot: u16) -> Result<(usize, usize), DbError> {
    slot_entry(bytes, page_id, slot)?.ok_or_else(|| corrupt(page_id))
}

fn write_slot(bytes: &mut [u8], slot: u16, offset: u16, length: u16) {
    let directory = HEADER_LEN + slot as usize * SLOT_LEN;
    write_u16(bytes, directory, offset);
    write_u16(bytes, directory + 2, length);
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    let mut array = [0u8; 2];
    array.copy_from_slice(&bytes[offset..offset + 2]);
    u16::from_le_bytes(array)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut array = [0u8; 4];
    array.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(array)
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut array = [0u8; 8];
    array.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(array)
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn checksum(bytes: &[u8]) -> u32 {
    crc32(bytes)
}

/// Reflected CRC-32 polynomial. ISO-HDLC, PNG, and Ethernet use this value.
const CRC_POLY: u32 = 0xEDB8_8320;

/// Eight slices of reflected CRC-32/ISO-HDLC, generated from [`CRC_POLY`].
/// Slice 0 is the one-byte table.
const CRC_SLICE_TABLES: [[u32; 256]; 8] = crc_slice_tables();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ CRC_POLY;
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

/// CRC-32/ISO-HDLC. The state starts at `0xFFFFFFFF` and the result is inverted.
///
/// Eight input bytes are folded through [`CRC_SLICE_TABLES`]. A remainder uses
/// the one-byte table. ARM CRC32 and SSE4.2 CRC32 instructions use the
/// Castagnoli polynomial, so they are not this checksum.
fn crc32(bytes: &[u8]) -> u32 {
    #[cfg(test)]
    if bitwise_crc_forced() {
        return crc32_bitwise(bytes);
    }
    crc32_slice8(bytes)
}

#[cfg(test)]
fn crc32_table(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = CRC_SLICE_TABLES[0][index] ^ (crc >> 8);
    }
    !crc
}

/// Bit-by-bit CRC-32/ISO-HDLC. Tests use this as the reference implementation.
#[cfg(test)]
fn crc32_bitwise(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (CRC_POLY & mask);
        }
    }
    !crc
}

fn crc32_slice8(bytes: &[u8]) -> u32 {
    let tables = &CRC_SLICE_TABLES;
    let mut crc = 0xFFFF_FFFFu32;
    let mut rest = bytes;
    while rest.len() >= 8 {
        let one = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) ^ crc;
        let two = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]);
        crc = tables[7][(one & 0xFF) as usize]
            ^ tables[6][((one >> 8) & 0xFF) as usize]
            ^ tables[5][((one >> 16) & 0xFF) as usize]
            ^ tables[4][(one >> 24) as usize]
            ^ tables[3][(two & 0xFF) as usize]
            ^ tables[2][((two >> 8) & 0xFF) as usize]
            ^ tables[1][((two >> 16) & 0xFF) as usize]
            ^ tables[0][(two >> 24) as usize];
        rest = &rest[8..];
    }
    for &byte in rest {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = tables[0][index] ^ (crc >> 8);
    }
    !crc
}

const fn crc_slice_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let base = crc_table();
    let mut index = 0;
    while index < 256 {
        tables[0][index] = base[index];
        index += 1;
    }
    let mut slice = 1;
    while slice < 8 {
        let mut index = 0;
        while index < 256 {
            let previous = tables[slice - 1][index];
            tables[slice][index] = (previous >> 8) ^ tables[0][(previous & 0xFF) as usize];
            index += 1;
        }
        slice += 1;
    }
    tables
}

#[cfg(test)]
thread_local! {
    static FORCE_BITWISE_CRC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Checksum with the original bit loop.
///
/// A commit uses the lookup table. This guard is the reference path.
#[cfg(test)]
pub(crate) struct ForceBitwiseCrc;

#[cfg(test)]
impl ForceBitwiseCrc {
    pub(crate) fn new() -> Self {
        FORCE_BITWISE_CRC.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForceBitwiseCrc {
    fn drop(&mut self) {
        FORCE_BITWISE_CRC.with(|flag| flag.set(false));
    }
}

#[cfg(test)]
fn bitwise_crc_forced() -> bool {
    FORCE_BITWISE_CRC.with(|flag| flag.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_fill_rejects_one_more_byte_and_reuses_a_deleted_slot() {
        let mut page = Page::new();
        init_slotted(&mut page, TYPE_HEAP, 3);
        let mut stored = 0u16;
        loop {
            let room = max_payload(&page);
            if room < 32 {
                break;
            }
            let payload = vec![0xAB; 32];
            stored = insert_cell(&mut page, &payload).unwrap();
        }
        let leftover = max_payload(&page);
        if leftover > 0 {
            assert!(insert_cell(&mut page, &vec![1; leftover]).is_ok());
        }
        assert_eq!(insert_cell(&mut page, &[2]), Err(InsertError::Full));
        assert!(insert_cell(&mut page, &vec![0; MAX_RECORD_LEN + 1]).is_err());

        delete_cell(&mut page, 0).unwrap();
        let reused = insert_cell(&mut page, &[9; 32]).unwrap();
        assert_eq!(reused, 0);
        assert_eq!(cell(&page, 0).unwrap(), &vec![9; 32]);
        assert_eq!(cell(&page, stored).unwrap().len(), 32);
        seal(&mut page);
        verify(&page, 3).unwrap();
    }

    #[test]
    fn checksum_detects_a_flipped_byte() {
        let mut page = Page::new();
        write_meta(
            &mut page,
            Meta {
                version: FORMAT_VERSION,
                page_count: 2,
                catalog_root: 1,
                freelist_root: 0,
                nonce: 1,
                commit_id: 0,
                next_table_id: 1,
                index_catalog_root: 0,
            },
            &[],
        )
        .unwrap();
        seal(&mut page);
        verify(&page, 0).unwrap();
        page.as_mut_slice()[META_CATALOG] ^= 0x01;
        assert_eq!(
            verify(&page, 0).unwrap_err().to_string(),
            "database page 0 is corrupt"
        );
    }

    #[test]
    fn a_full_legacy_free_list_is_not_read_as_the_table_id_counter() {
        let mut page = Page::new();
        write_meta(
            &mut page,
            Meta {
                version: FORMAT_VERSION,
                page_count: 2,
                catalog_root: 1,
                freelist_root: 0,
                nonce: 1,
                commit_id: 0,
                next_table_id: 9,
                index_catalog_root: 0,
            },
            &[],
        )
        .unwrap();
        let bytes = page.as_mut_slice();
        write_u32(bytes, META_FREE_COUNT, META_FREE_CAPACITY_LEGACY as u32);
        for index in 0..META_FREE_CAPACITY_LEGACY {
            write_u32(
                bytes,
                META_FREE_IDS + index * 4,
                u32::try_from(index + 1).unwrap(),
            );
        }
        seal(&mut page);
        let meta = read_meta(&page).unwrap();
        assert_eq!(meta.next_table_id, 0);
        let ids = inline_free_ids(&page).unwrap();
        assert_eq!(ids.len(), META_FREE_CAPACITY_LEGACY);
        assert_eq!(
            ids[META_FREE_CAPACITY_LEGACY - 1],
            META_FREE_CAPACITY_LEGACY as u32
        );
        assert_ne!(meta.next_table_id, ids[META_FREE_CAPACITY_LEGACY - 1]);
    }

    #[test]
    fn a_resealed_page_rejects_a_slot_past_the_page_or_overlapping_another() {
        let mut page = Page::new();
        init_slotted(&mut page, TYPE_HEAP, 4);
        insert_cell(&mut page, &[1, 2, 3, 4]).unwrap();
        insert_cell(&mut page, &[5, 6, 7, 8]).unwrap();
        seal(&mut page);
        verify(&page, 4).unwrap();
        assert_eq!(
            verify(&page, 9).unwrap_err().to_string(),
            "database page 9 is corrupt"
        );

        write_u16(page.as_mut_slice(), HEADER_LEN + 2, 5000);
        seal(&mut page);
        assert_eq!(
            verify(&page, 4).unwrap_err().to_string(),
            "database page 4 is corrupt"
        );

        let mut page = Page::new();
        init_slotted(&mut page, TYPE_HEAP, 4);
        insert_cell(&mut page, &[1, 2, 3, 4]).unwrap();
        insert_cell(&mut page, &[5, 6, 7, 8]).unwrap();
        let first = read_u16(page.as_slice(), HEADER_LEN);
        write_u16(page.as_mut_slice(), HEADER_LEN + SLOT_LEN, first);
        write_u16(page.as_mut_slice(), HEADER_LEN + SLOT_LEN + 2, 4);
        seal(&mut page);
        assert_eq!(
            verify(&page, 4).unwrap_err().to_string(),
            "database page 4 is corrupt"
        );
    }

    fn sealed_heap() -> Page {
        let mut page = Page::new();
        init_slotted(&mut page, TYPE_HEAP, 4);
        insert_cell(&mut page, &[1, 2, 3, 4]).unwrap();
        insert_cell(&mut page, &[5, 6, 7, 8]).unwrap();
        seal(&mut page);
        page
    }

    fn seal_index(page_type: u16, id: u32, count: u16) -> Page {
        let mut page = Page::new();
        init_slotted(&mut page, page_type, id);
        write_u16(page.as_mut_slice(), OFF_UPPER, HEADER_LEN as u16);
        write_u16(page.as_mut_slice(), HEADER_LEN, count);
        seal(&mut page);
        page
    }

    #[test]
    fn a_fresh_seal_matches_verify_on_valid_pages_and_keeps_its_checksum() {
        for (page_type, id) in [(TYPE_HEAP, 4), (TYPE_CATALOG, 2), (TYPE_INDEX_CATALOG, 6)] {
            let mut page = Page::new();
            init_slotted(&mut page, page_type, id);
            insert_cell(&mut page, b"cell").unwrap();
            seal(&mut page);
            let checksum = read_u32(page.as_slice(), OFF_CHECKSUM);
            verify(&page, id).unwrap();
            validate_fresh_seal(&page, id).unwrap();
            assert_eq!(read_u32(page.as_slice(), OFF_CHECKSUM), checksum);
        }
        let leaf = seal_index(TYPE_INDEX_LEAF, 8, 1);
        verify(&leaf, 8).unwrap();
        validate_fresh_seal(&leaf, 8).unwrap();
        let internal = seal_index(TYPE_INDEX_INTERNAL, 9, 1);
        verify(&internal, 9).unwrap();
        validate_fresh_seal(&internal, 9).unwrap();
        let mut free = Page::new();
        write_free_data(&mut free, 5);
        seal(&mut free);
        verify(&free, 5).unwrap();
        validate_fresh_seal(&free, 5).unwrap();
    }

    #[test]
    fn structural_damage_is_rejected_with_and_without_a_second_checksum() {
        let cases: &[fn(&mut Page)] = &[
            |page| write_u16(page.as_mut_slice(), OFF_TYPE, 99),
            |page| write_u32(page.as_mut_slice(), OFF_ID, 1),
            |page| write_u16(page.as_mut_slice(), OFF_FLAGS, 1),
            |page| write_u16(page.as_mut_slice(), OFF_RESERVED, 1),
            |page| write_u16(page.as_mut_slice(), HEADER_LEN + 2, 5000),
            |page| {
                let first = read_u16(page.as_slice(), HEADER_LEN);
                write_u16(page.as_mut_slice(), HEADER_LEN + SLOT_LEN, first);
                write_u16(page.as_mut_slice(), HEADER_LEN + SLOT_LEN + 2, 4);
            },
        ];
        for damage in cases {
            let mut page = sealed_heap();
            damage(&mut page);
            seal(&mut page);
            let full = verify(&page, 4).unwrap_err().to_string();
            let fresh = validate_fresh_seal(&page, 4).unwrap_err().to_string();
            assert_eq!(full, "database page 4 is corrupt");
            assert_eq!(fresh, full);
        }
        let leaf = seal_index(TYPE_INDEX_LEAF, 8, 0);
        assert_eq!(
            verify(&leaf, 8).unwrap_err().to_string(),
            validate_fresh_seal(&leaf, 8).unwrap_err().to_string()
        );
        let mut internal = seal_index(TYPE_INDEX_INTERNAL, 9, 1);
        write_u16(internal.as_mut_slice(), HEADER_LEN + 2, 1);
        seal(&mut internal);
        assert_eq!(
            verify(&internal, 9).unwrap_err().to_string(),
            validate_fresh_seal(&internal, 9).unwrap_err().to_string()
        );
        write_u32(internal.as_mut_slice(), PAGE_SIZE - 4, 1);
        write_u16(internal.as_mut_slice(), HEADER_LEN + 2, 0);
        seal(&mut internal);
        assert_eq!(
            verify(&internal, 9).unwrap_err().to_string(),
            validate_fresh_seal(&internal, 9).unwrap_err().to_string()
        );
    }

    #[test]
    fn a_bad_checksum_fails_verify_and_is_not_what_a_fresh_seal_checks() {
        let mut page = sealed_heap();
        page.as_mut_slice()[OFF_CHECKSUM] ^= 0xff;
        assert_eq!(
            verify(&page, 4).unwrap_err().to_string(),
            "database page 4 is corrupt"
        );
        validate_fresh_seal(&page, 4).unwrap();
        page.as_mut_slice()[OFF_CHECKSUM] ^= 0xff;
        page.as_mut_slice()[HEADER_LEN] ^= 0xff;
        assert!(recheck_checksum(&page, 4).is_err());
        assert!(verify(&page, 4).is_err());
    }

    #[test]
    fn a_payload_changed_after_seal_fails_the_untrusted_checksum() {
        let mut page = sealed_heap();
        let sealed = page.as_slice().to_vec();
        page.as_mut_slice()[PAGE_SIZE - 1] ^= 0xff;
        assert!(recheck_checksum(&page, 4).is_err());
        assert!(verify(&page, 4).is_err());
        page.as_mut_slice().copy_from_slice(&sealed);
        verify(&page, 4).unwrap();
    }

    #[test]
    fn crc32_matches_published_answers_and_the_bitwise_reference() {
        assert_eq!(checksum(&[]), 0x0000_0000);
        assert_eq!(crc32_bitwise(&[]), 0x0000_0000);
        assert_eq!(checksum(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_bitwise(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_slice8(b"123456789"), 0xCBF4_3926);
        let samples = crc_samples();
        for sample in &samples {
            let reference = crc32_bitwise(sample);
            assert_eq!(checksum(sample), reference, "len {}", sample.len());
            assert_eq!(crc32_slice8(sample), reference, "slice8 {}", sample.len());
            for align in 0..8 {
                let mut storage = vec![0xA5u8; sample.len() + align];
                storage[align..].copy_from_slice(sample);
                assert_eq!(checksum(&storage[align..]), reference, "align {align}");
                assert_eq!(crc32_slice8(&storage[align..]), reference);
            }
        }
    }

    #[test]
    fn a_sealed_page_keeps_the_bitwise_checksum_bytes() {
        let mut fast = sealed_heap();
        let slow = {
            let _guard = ForceBitwiseCrc::new();
            sealed_heap()
        };
        assert_eq!(fast.as_slice(), slow.as_slice());
        let stored = u32::from_le_bytes(
            fast.as_slice()[OFF_CHECKSUM..OFF_CHECKSUM + 4]
                .try_into()
                .unwrap(),
        );
        fast.as_mut_slice()[OFF_CHECKSUM..OFF_CHECKSUM + 4].fill(0);
        assert_eq!(crc32_bitwise(fast.as_slice()), stored);
        fast.as_mut_slice()[PAGE_SIZE - 1] ^= 0xFF;
        assert!(recheck_checksum(&fast, 4).is_err());
        assert!(verify(&fast, 4).is_err());
    }

    fn crc_samples() -> Vec<Vec<u8>> {
        let mut samples = Vec::new();
        for length in [
            0, 1, 2, 3, 4, 7, 8, 9, 15, 16, 31, 32, 63, 64, 4095, 4096, 4097, 4100, 8192, 12_288,
        ] {
            samples.push(vec![0u8; length]);
            samples.push(vec![0xFF; length]);
        }
        samples.push(b"123456789".to_vec());
        samples.push("café — Δ".as_bytes().to_vec());
        let mut mixed = Vec::new();
        let mut state = 0xC0FF_EE42u64;
        for _ in 0..20_000 {
            state = state
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(0x6A09_E667);
            mixed.push(state as u8);
        }
        samples.push(mixed);
        let mut frames = Vec::new();
        for id in 0..3u32 {
            frames.extend(id.to_le_bytes());
            frames.extend(std::iter::repeat_n(id as u8, 4096));
        }
        samples.push(frames);
        samples.push({
            let mut odd = vec![0u8; 4100 * 2 + 17];
            for (index, byte) in odd.iter_mut().enumerate() {
                *byte = (index as u8).wrapping_mul(3);
            }
            odd
        });
        let mut wide = Vec::with_capacity(1 << 20);
        let mut state = 0x1234_5678_9ABCu64;
        while wide.len() < (1 << 20) {
            state = state.wrapping_mul(0x5851_F42D).wrapping_add(1);
            wide.push((state >> 33) as u8);
        }
        samples.push(wide);
        samples
    }

    #[test]
    #[ignore = "release comparison of CRC-32 implementations"]
    fn profile_crc32_implementations() {
        let lengths = [24usize, 64, 4096, 64 * 1024, 1024 * 1024, 12 * 1024 * 1024];
        println!("crc32 cpu=apple-m5 poly=0xEDB88320 init=0xFFFFFFFF final_xor=0xFFFFFFFF");
        for length in lengths {
            let mut bytes = vec![0u8; length];
            let mut state = 0xA5A5_5A5A_0123_4567u64;
            for byte in &mut bytes {
                state = state.wrapping_mul(0x9E37_79B9).wrapping_add(0x7F4A_7C15);
                *byte = (state >> 24) as u8;
            }
            let reference = crc32_bitwise(&bytes);
            assert_eq!(crc32_table(&bytes), reference);
            assert_eq!(crc32_slice8(&bytes), reference);
            let repeats = if length <= 64 {
                200_000
            } else if length <= 4096 {
                20_000
            } else if length <= 64 * 1024 {
                2_000
            } else if length <= 1024 * 1024 {
                80
            } else {
                8
            };
            for (name, function) in [
                ("bitwise", crc32_bitwise as fn(&[u8]) -> u32),
                ("table", crc32_table as fn(&[u8]) -> u32),
                ("slice8", crc32_slice8 as fn(&[u8]) -> u32),
            ] {
                let cold = std::time::Instant::now();
                let cold_sum = std::hint::black_box(function(std::hint::black_box(&bytes)));
                let cold_ns = cold.elapsed().as_nanos();
                let mut warm = Vec::with_capacity(5);
                let mut total = 0u64;
                for _ in 0..5 {
                    let started = std::time::Instant::now();
                    let mut mixed = 0u32;
                    for _ in 0..repeats {
                        mixed ^= function(std::hint::black_box(&bytes));
                    }
                    let elapsed = started.elapsed();
                    total = total.wrapping_add(u64::from(std::hint::black_box(mixed)));
                    warm.push(elapsed.as_nanos() / repeats as u128);
                }
                warm.sort_unstable();
                let median = warm[warm.len() / 2];
                let mib = length as f64 / (1024.0 * 1024.0);
                let per_sec = if median == 0 {
                    0.0
                } else {
                    mib / (median as f64 / 1_000_000_000.0)
                };
                println!(
                    "crc32 impl={name} bytes={length} cold_ns={cold_ns} warm_ns={median} warm_min_ns={} warm_max_ns={} mib_per_s={per_sec:.1} repeats={repeats} checksum={cold_sum:#x} sink={total}",
                    warm[0], warm[4]
                );
            }
        }
    }
}
