//! Optional phase timer for the update investigation.
//!
//! `start` returns `None` unless a test has enabled recording, so the commit
//! path does not sample the clock in ordinary runs.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Profile {
    pub compare: Duration,
    /// Building the table image, or auditing memory when that image is skipped.
    pub prepare: Duration,
    pub apply: Duration,
    /// Checking deleted ids against the stored row-id vector.
    pub validate_ids: Duration,
    /// Decoding deleted heap records, or the stored rows of a no-match.
    pub decode: Duration,
    /// Heap slot removal, link repair, catalog head, and empty-page release.
    pub heap_edit: Duration,
    /// Removing index postings for a delete.
    pub index_edit: Duration,
    pub wal_write: Duration,
    pub wal_sync: Duration,
    pub db_write: Duration,
    pub db_sync: Duration,
    pub wal_clear: Duration,
    #[cfg_attr(not(test), allow(dead_code))]
    pub frames: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub row_compares: u64,
    #[cfg_attr(not(test), allow(dead_code))]
    pub heap_pages: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub index_pages: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub other_pages: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    pub wal_bytes: u64,
    /// Postings removed from a leaf in one rewrite.
    #[cfg_attr(not(test), allow(dead_code))]
    pub leaf_batches: u64,
    /// Postings removed by the single-entry index delete.
    #[cfg_attr(not(test), allow(dead_code))]
    pub index_singles: u64,
    /// Root-to-leaf descents during index deletion.
    #[cfg_attr(not(test), allow(dead_code))]
    pub descents: u64,
    /// Leaf spans repacked by bulk deletion.
    #[cfg_attr(not(test), allow(dead_code))]
    pub leaf_redistributions: u64,
    /// Leaves removed by a bulk merge.
    #[cfg_attr(not(test), allow(dead_code))]
    pub leaf_merges: u64,
    /// Internal pages removed when a bulk rebuild shrinks that level.
    #[cfg_attr(not(test), allow(dead_code))]
    pub internal_merges: u64,
    /// Levels lost when a bulk rebuild shortens the tree.
    #[cfg_attr(not(test), allow(dead_code))]
    pub root_contractions: u64,
    /// Index pages allocated during a tree edit.
    #[cfg_attr(not(test), allow(dead_code))]
    pub pages_allocated: u64,
    /// Index pages freed during a tree edit.
    #[cfg_attr(not(test), allow(dead_code))]
    pub pages_freed: u64,
    /// Bulk span rebuilds.
    #[cfg_attr(not(test), allow(dead_code))]
    pub bulk_spans: u64,
    /// Bytes of the posting vectors built for one bulk rebuild.
    #[cfg_attr(not(test), allow(dead_code))]
    pub scratch_bytes: u64,
    /// Leaf pages decoded while checking or editing an index.
    #[cfg_attr(not(test), allow(dead_code))]
    pub leaf_decodes: u64,
    /// Right-sibling steps during ordered verification and span loading.
    #[cfg_attr(not(test), allow(dead_code))]
    pub sibling_steps: u64,
    /// Entry comparisons while matching a delete request to a leaf.
    #[cfg_attr(not(test), allow(dead_code))]
    pub posting_compares: u64,
    /// Checks that a request pair has not already been accepted.
    #[cfg_attr(not(test), allow(dead_code))]
    pub duplicate_checks: u64,
    /// Postings accepted by the ordered leaf-chain check.
    #[cfg_attr(not(test), allow(dead_code))]
    pub ordered_verified: u64,
    /// Postings accepted by a root descent per posting.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fallback_verified: u64,
    /// Validating the tree and building the posting presence map.
    #[cfg_attr(not(test), allow(dead_code))]
    pub presence_build: Duration,
    /// Looking up requested postings in a presence map.
    #[cfg_attr(not(test), allow(dead_code))]
    pub presence_member: Duration,
    /// Payload bytes of `(key, row id, leaf)` pairs stored in the map.
    /// This is not peak resident memory.
    #[cfg_attr(not(test), allow(dead_code))]
    pub map_bytes: u64,
    /// Requested pairs found in the map.
    #[cfg_attr(not(test), allow(dead_code))]
    pub map_hits: u64,
    /// Requested pairs absent from the map.
    #[cfg_attr(not(test), allow(dead_code))]
    pub map_misses: u64,
    /// Postings accepted after a presence-map check, including the seed leaf.
    #[cfg_attr(not(test), allow(dead_code))]
    pub presence_verified: u64,
    /// Successful presence maps built during deletion.
    #[cfg_attr(not(test), allow(dead_code))]
    pub presence_builds: u64,
    /// Pages the structure-aware estimate used for one statement.
    /// This is the sum across indexes, not a peak.
    #[cfg_attr(not(test), allow(dead_code))]
    pub estimated_pages: u64,
    /// Pages read while building an early presence map.
    #[cfg_attr(not(test), allow(dead_code))]
    pub scanned_pages: u64,
    /// Early presence plans that verified a request before the grouping loop.
    #[cfg_attr(not(test), allow(dead_code))]
    pub early_plans: u64,
    /// Rebuild-time validations skipped because this snapshot was already checked.
    #[cfg_attr(not(test), allow(dead_code))]
    pub validations_skipped: u64,
    /// Calls to the B+ tree structural validator.
    #[cfg_attr(not(test), allow(dead_code))]
    pub tree_validations: u64,
    /// Reserving the presence map and inserting pairs and parent paths.
    /// This sits inside `presence_build` and does not read pages.
    #[cfg_attr(not(test), allow(dead_code))]
    pub path_record: Duration,
    /// Non-underflow singles applied through a parent path already in hand.
    #[cfg_attr(not(test), allow(dead_code))]
    pub reused_paths: u64,
    /// Plans discarded because a rebuild, merge, or root change moved pages.
    #[cfg_attr(not(test), allow(dead_code))]
    pub path_invalidations: u64,
}

#[cfg(test)]
thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static DATA: std::cell::Cell<Profile> = const { std::cell::Cell::new(Profile {
        compare: Duration::ZERO,
        prepare: Duration::ZERO,
        apply: Duration::ZERO,
        validate_ids: Duration::ZERO,
        decode: Duration::ZERO,
        heap_edit: Duration::ZERO,
        index_edit: Duration::ZERO,
        wal_write: Duration::ZERO,
        wal_sync: Duration::ZERO,
        db_write: Duration::ZERO,
        db_sync: Duration::ZERO,
        wal_clear: Duration::ZERO,
        frames: 0,
        row_compares: 0,
        heap_pages: 0,
        index_pages: 0,
        other_pages: 0,
        wal_bytes: 0,
        leaf_batches: 0,
        index_singles: 0,
        descents: 0,
        leaf_redistributions: 0,
        leaf_merges: 0,
        internal_merges: 0,
        root_contractions: 0,
        pages_allocated: 0,
        pages_freed: 0,
        bulk_spans: 0,
        scratch_bytes: 0,
        leaf_decodes: 0,
        sibling_steps: 0,
        posting_compares: 0,
        duplicate_checks: 0,
        ordered_verified: 0,
        fallback_verified: 0,
        presence_build: Duration::ZERO,
        presence_member: Duration::ZERO,
        map_bytes: 0,
        map_hits: 0,
        map_misses: 0,
        presence_verified: 0,
        presence_builds: 0,
        estimated_pages: 0,
        scanned_pages: 0,
        early_plans: 0,
        validations_skipped: 0,
        tree_validations: 0,
        path_record: Duration::ZERO,
        reused_paths: 0,
        path_invalidations: 0,
    }) };
}

#[inline]
pub(crate) fn start() -> Option<Instant> {
    #[cfg(test)]
    {
        if ENABLED.with(|flag| flag.get()) {
            return Some(Instant::now());
        }
    }
    None
}

pub(crate) fn add_compare(mark: Option<Instant>) {
    add(|profile| &mut profile.compare, mark);
}

pub(crate) fn add_prepare(mark: Option<Instant>) {
    add(|profile| &mut profile.prepare, mark);
}

pub(crate) fn add_apply(mark: Option<Instant>) {
    add(|profile| &mut profile.apply, mark);
}

pub(crate) fn add_validate(mark: Option<Instant>) {
    add(|profile| &mut profile.validate_ids, mark);
}

pub(crate) fn add_decode(mark: Option<Instant>) {
    add(|profile| &mut profile.decode, mark);
}

pub(crate) fn add_heap_edit(mark: Option<Instant>) {
    add(|profile| &mut profile.heap_edit, mark);
}

pub(crate) fn add_index_edit(mark: Option<Instant>) {
    add(|profile| &mut profile.index_edit, mark);
}

pub(crate) fn note_leaf_batch(entries: u64) {
    #[cfg(test)]
    if ENABLED.with(std::cell::Cell::get) {
        DATA.with(|data| {
            let mut profile = data.get();
            profile.leaf_batches = profile.leaf_batches.saturating_add(entries);
            data.set(profile);
        });
    }
    #[cfg(not(test))]
    {
        let _ = entries;
    }
}

pub(crate) fn note_index_single() {
    #[cfg(test)]
    if ENABLED.with(std::cell::Cell::get) {
        DATA.with(|data| {
            let mut profile = data.get();
            profile.index_singles = profile.index_singles.saturating_add(1);
            data.set(profile);
        });
    }
}

pub(crate) fn note_reused_path() {
    note_u64(|profile| &mut profile.reused_paths, 1);
}

pub(crate) fn note_path_invalidation() {
    note_u64(|profile| &mut profile.path_invalidations, 1);
}

pub(crate) fn note_descent() {
    note_u64(|profile| &mut profile.descents, 1);
}

pub(crate) fn note_leaf_redistribution() {
    note_u64(|profile| &mut profile.leaf_redistributions, 1);
}

pub(crate) fn note_leaf_merges(count: u64) {
    note_u64(|profile| &mut profile.leaf_merges, count);
}

pub(crate) fn note_internal_merges(count: u64) {
    note_u64(|profile| &mut profile.internal_merges, count);
}

pub(crate) fn note_root_contractions(count: u64) {
    note_u64(|profile| &mut profile.root_contractions, count);
}

pub(crate) fn note_page_alloc() {
    note_u64(|profile| &mut profile.pages_allocated, 1);
}

pub(crate) fn note_page_free() {
    note_u64(|profile| &mut profile.pages_freed, 1);
}

pub(crate) fn note_bulk_span() {
    note_u64(|profile| &mut profile.bulk_spans, 1);
}

pub(crate) fn note_scratch(bytes: u64) {
    note_u64(|profile| &mut profile.scratch_bytes, bytes);
}

pub(crate) fn note_leaf_decode() {
    note_u64(|profile| &mut profile.leaf_decodes, 1);
}

pub(crate) fn note_sibling_step() {
    note_u64(|profile| &mut profile.sibling_steps, 1);
}

pub(crate) fn note_posting_compare_n(count: u64) {
    note_u64(|profile| &mut profile.posting_compares, count);
}

pub(crate) fn note_duplicate_check_n(count: u64) {
    note_u64(|profile| &mut profile.duplicate_checks, count);
}

pub(crate) fn note_ordered_verified(count: u64) {
    note_u64(|profile| &mut profile.ordered_verified, count);
}

pub(crate) fn note_fallback_verified(count: u64) {
    note_u64(|profile| &mut profile.fallback_verified, count);
}

pub(crate) fn add_presence_build(mark: Option<Instant>) {
    add(|profile| &mut profile.presence_build, mark);
}

pub(crate) fn add_presence_member(mark: Option<Instant>) {
    add(|profile| &mut profile.presence_member, mark);
}

pub(crate) fn note_map_bytes(bytes: u64) {
    note_u64(|profile| &mut profile.map_bytes, bytes);
}

pub(crate) fn note_map_hit(count: u64) {
    note_u64(|profile| &mut profile.map_hits, count);
}

pub(crate) fn note_map_miss() {
    note_u64(|profile| &mut profile.map_misses, 1);
}

pub(crate) fn note_presence_verified(count: u64) {
    note_u64(|profile| &mut profile.presence_verified, count);
}

pub(crate) fn note_presence_build_count() {
    note_u64(|profile| &mut profile.presence_builds, 1);
}

pub(crate) fn note_estimated_pages(pages: u64) {
    note_u64(|profile| &mut profile.estimated_pages, pages);
}

pub(crate) fn note_scanned_pages(pages: u64) {
    note_u64(|profile| &mut profile.scanned_pages, pages);
}

pub(crate) fn note_early_plan() {
    note_u64(|profile| &mut profile.early_plans, 1);
}

pub(crate) fn note_validation_skipped() {
    note_u64(|profile| &mut profile.validations_skipped, 1);
}

pub(crate) fn note_tree_validation() {
    note_u64(|profile| &mut profile.tree_validations, 1);
}

pub(crate) fn add_path_record(mark: Option<Instant>) {
    add(|profile| &mut profile.path_record, mark);
}

fn note_u64(slot: fn(&mut Profile) -> &mut u64, amount: u64) {
    #[cfg(test)]
    if ENABLED.with(std::cell::Cell::get) {
        DATA.with(|data| {
            let mut profile = data.get();
            let current = *slot(&mut profile);
            *slot(&mut profile) = current.saturating_add(amount);
            data.set(profile);
        });
    }
    #[cfg(not(test))]
    {
        let _ = (slot, amount);
    }
}

pub(crate) fn add_wal_write(mark: Option<Instant>) {
    add(|profile| &mut profile.wal_write, mark);
}

pub(crate) fn add_wal_sync(mark: Option<Instant>) {
    add(|profile| &mut profile.wal_sync, mark);
}

pub(crate) fn add_db_write(mark: Option<Instant>) {
    add(|profile| &mut profile.db_write, mark);
}

pub(crate) fn add_db_sync(mark: Option<Instant>) {
    add(|profile| &mut profile.db_sync, mark);
}

pub(crate) fn add_wal_clear(mark: Option<Instant>) {
    add(|profile| &mut profile.wal_clear, mark);
}

pub(crate) fn note_frames(count: u32) {
    #[cfg(test)]
    {
        if ENABLED.with(|flag| flag.get()) {
            DATA.with(|data| {
                let mut profile = data.get();
                profile.frames = count;
                data.set(profile);
            });
        }
    }
    #[cfg(not(test))]
    {
        let _ = count;
    }
}

pub(crate) fn note_heap_page() {
    note_counter(|profile| &mut profile.heap_pages);
}

pub(crate) fn note_index_page() {
    note_counter(|profile| &mut profile.index_pages);
}

pub(crate) fn note_other_page() {
    note_counter(|profile| &mut profile.other_pages);
}

pub(crate) fn note_wal_bytes(bytes: usize) {
    #[cfg(test)]
    if ENABLED.with(std::cell::Cell::get) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        DATA.with(|data| {
            let mut profile = data.get();
            profile.wal_bytes = profile.wal_bytes.saturating_add(bytes);
            data.set(profile);
        });
    }
    #[cfg(not(test))]
    {
        let _ = bytes;
    }
}

fn note_counter(slot: fn(&mut Profile) -> &mut u32) {
    #[cfg(test)]
    if ENABLED.with(std::cell::Cell::get) {
        DATA.with(|data| {
            let mut profile = data.get();
            let current = *slot(&mut profile);
            *slot(&mut profile) = current.saturating_add(1);
            data.set(profile);
        });
    }
    #[cfg(not(test))]
    {
        let _ = slot;
    }
}

pub(crate) fn note_row_compare() {
    #[cfg(test)]
    {
        if ENABLED.with(|flag| flag.get()) {
            DATA.with(|data| {
                let mut profile = data.get();
                profile.row_compares = profile.row_compares.saturating_add(1);
                data.set(profile);
            });
        }
    }
}

#[cfg(test)]
pub(crate) struct Guard;

#[cfg(test)]
impl Drop for Guard {
    fn drop(&mut self) {
        ENABLED.with(|flag| flag.set(false));
    }
}

#[cfg(test)]
pub(crate) fn enable() -> Guard {
    ENABLED.with(|flag| flag.set(true));
    reset();
    Guard
}

#[cfg(test)]
pub(crate) fn reset() {
    DATA.with(|data| data.set(Profile::default()));
}

#[cfg(test)]
pub(crate) fn take() -> Profile {
    DATA.with(|data| data.get())
}

fn add(slot: fn(&mut Profile) -> &mut Duration, mark: Option<Instant>) {
    #[cfg(test)]
    {
        let Some(mark) = mark else {
            return;
        };
        DATA.with(|data| {
            let mut profile = data.get();
            *slot(&mut profile) += mark.elapsed();
            data.set(profile);
        });
    }
    #[cfg(not(test))]
    {
        let _ = (slot, mark);
    }
}
