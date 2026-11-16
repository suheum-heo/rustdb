//! Integer B+ tree stored in 4096-byte pages.
//!
//! Leaves hold sorted `(key, row_id)` pairs. Internal nodes hold copies of
//! the minimum pair in each child after the leftmost. A missing key is the
//! absence of a pair; SQL `NULL` is never stored here.

use super::page::{
    HEADER_LEN, OFF_ID, OFF_LOWER, OFF_NEXT, OFF_SLOTS, OFF_TYPE, OFF_UPPER, PAGE_SIZE, Page,
    TYPE_INDEX_INTERNAL, TYPE_INDEX_LEAF,
};
use crate::error::DbError;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

pub(crate) type Entry = (i64, u64);

/// Production occupancy. The byte layout fixes the maxima.
pub(crate) const LEAF_MAX: usize = 254;
pub(crate) const LEAF_MIN: usize = 127;
pub(crate) const INTERNAL_MAX: usize = 203;
pub(crate) const INTERNAL_MIN: usize = 101;

/// A child chain taller than this is corrupt. A real tree stays far below it.
const MAX_DEPTH: u32 = 64;

const ENTRY_LEN: usize = 16;
const SEPARATOR_LEN: usize = 20;
const LEAF_COUNT: usize = HEADER_LEN;
const LEAF_BODY: usize = HEADER_LEN + 4;
const LEAF_LEFT: usize = PAGE_SIZE - 4;
const INTERNAL_COUNT: usize = HEADER_LEN;
const INTERNAL_LEFTMOST: usize = HEADER_LEN + 4;

pub(crate) const fn production() -> Limits {
    Limits {
        leaf_max: LEAF_MAX,
        leaf_min: LEAF_MIN,
        internal_max: INTERNAL_MAX,
        internal_min: INTERNAL_MIN,
    }
}

/// Occupancy used by one tree. Tests use a smaller fanout with the same
/// split and merge equations as the on-disk pages.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub leaf_max: usize,
    pub leaf_min: usize,
    pub internal_max: usize,
    pub internal_min: usize,
}

pub(crate) trait Nodes {
    fn read_node(&mut self, id: u32) -> Result<Page, DbError>;
    fn write_node(&mut self, id: u32, page: Page) -> Result<(), DbError>;
    fn alloc_node(&mut self) -> Result<u32, DbError>;
    fn free_node(&mut self, id: u32) -> Result<(), DbError>;
}

fn alloc_page(store: &mut dyn Nodes) -> Result<u32, DbError> {
    let id = store.alloc_node()?;
    super::profile::note_page_alloc();
    Ok(id)
}

fn free_page(store: &mut dyn Nodes, id: u32) -> Result<(), DbError> {
    store.free_node(id)?;
    super::profile::note_page_free();
    Ok(())
}

#[derive(Clone)]
struct Leaf {
    entries: Vec<Entry>,
    left: u32,
    right: u32,
}

#[derive(Clone)]
struct Internal {
    keys: Vec<Entry>,
    children: Vec<u32>,
}

enum Inserted {
    Fit {
        minimum: Entry,
    },
    Split {
        minimum: Entry,
        separator: Entry,
        right: u32,
    },
}

enum Removed {
    Present { minimum: Entry },
    Underfull { minimum: Entry },
    Collapsed { child: u32 },
    Empty,
}

/// Insert `(key, row_id)`. `root == 0` creates the first leaf.
pub(crate) fn insert(
    store: &mut dyn Nodes,
    root: u32,
    key: i64,
    row_id: u64,
    limits: Limits,
) -> Result<u32, DbError> {
    if row_id == 0 {
        return Err(DbError::Internal(
            "index row id must be nonzero".to_string(),
        ));
    }
    let entry = (key, row_id);
    if root == 0 {
        let id = alloc_page(store)?;
        let leaf = Leaf {
            entries: vec![entry],
            left: 0,
            right: 0,
        };
        store.write_node(id, encode_leaf(id, &leaf)?)?;
        return Ok(id);
    }
    match insert_at(store, root, entry, limits, 0)? {
        Inserted::Fit { .. } => Ok(root),
        Inserted::Split {
            separator, right, ..
        } => {
            let id = alloc_page(store)?;
            let node = Internal {
                keys: vec![separator],
                children: vec![root, right],
            };
            store.write_node(id, encode_internal(id, &node)?)?;
            Ok(id)
        }
    }
}

/// Delete one pair. Returns the root, which is `0` when the tree is empty.
pub(crate) fn delete(
    store: &mut dyn Nodes,
    root: u32,
    key: i64,
    row_id: u64,
    limits: Limits,
) -> Result<u32, DbError> {
    if root == 0 {
        return Err(missing());
    }
    super::profile::note_descent();
    super::profile::note_index_single();
    match delete_at(store, root, (key, row_id), true, limits, 0)? {
        Removed::Empty => {
            free_page(store, root)?;
            Ok(0)
        }
        Removed::Present { .. } | Removed::Underfull { .. } => Ok(root),
        Removed::Collapsed { child, .. } => {
            free_page(store, root)?;
            Ok(child)
        }
    }
}

/// Delete `entries` in order.
///
/// A run that sits in one freshly decoded leaf and leaves that leaf at or
/// above its minimum is removed in one rewrite. One posting, and any run that
/// would not underflow, still uses [`delete`]. A run that would drop the leaf
/// below its minimum is removed by repacking the sibling span that holds the
/// verified prefix, then rebuilding internal pages over the surviving leaves.
/// The prefix stops at the first missing or repeated pair, and that error is
/// the same as [`delete`]. A run that moves only forward or only backward in
/// `(key, row id)` order is checked along the leaf chain after the first root
/// descent. A run that changes direction may be checked against a presence
/// map before this loop. The map is a snapshot of the current root. It is
/// dropped when a rebuild frees pages. A map that cannot be built, a monotonic
/// run, a request whose descents would read fewer pages than the estimate, and
/// a test that forces the older check still descend once per posting. A test
/// can also force the grouped path for every run.
pub(crate) fn delete_many(
    store: &mut dyn Nodes,
    root: u32,
    entries: &[Entry],
    limits: Limits,
) -> Result<u32, DbError> {
    if grouped_only() {
        return delete_grouped(store, root, entries, limits);
    }
    let plan = try_early_presence(store, root, entries, limits)?;
    delete_with_plan(store, root, entries, limits, plan)
}

fn delete_with_plan(
    store: &mut dyn Nodes,
    mut root: u32,
    entries: &[Entry],
    limits: Limits,
    mut plan: Option<PresencePlan>,
) -> Result<u32, DbError> {
    let mut cursor = 0;
    // Grouped rewrites and single deletes that stay above the minimum do not
    // free a leaf or change another leaf's child index. A span rebuild does.
    let mut rebuilt = false;
    let mut mutated = false;
    while cursor < entries.len() {
        if root == 0 {
            return Err(missing());
        }
        let cached = if rebuilt {
            None
        } else {
            plan.as_ref()
                .filter(|plan| plan.live)
                .and_then(|plan| plan.place(entries[cursor]))
        };
        let (path, leaf_id) = if let Some(place) = cached {
            (place.path, place.leaf)
        } else {
            path_to_leaf(store, root, entries[cursor])?
        };
        let leaf = decode_leaf(&store.read_node(leaf_id)?)?;
        // Count a slot once. A second copy of the same pair ends the run, so
        // the next pass reports it missing after the first copy is removed.
        // The length used below is this leaf's, just decoded.
        let mut claimed = vec![false; leaf.entries.len()];
        let mut run = 0;
        while cursor + run < entries.len() {
            let wanted = entries[cursor + run];
            let Some(slot) = unclaimed_slot(&leaf.entries, &claimed, wanted) else {
                break;
            };
            claimed[slot] = true;
            run += 1;
        }
        if run == 0 {
            return Err(missing());
        }
        let is_root = leaf_id == root;
        let keep = if is_root { 1 } else { limits.leaf_min.max(1) };
        let room = leaf.entries.len().saturating_sub(keep);
        let take = run.min(room);
        if take >= 2 {
            let old_first = leaf.entries[0];
            let mut removed = leaf;
            remove_leaf_entries(&mut removed, &entries[cursor..cursor + take])?;
            let minimum_changed = removed.entries[0] != old_first;
            super::profile::note_leaf_batch(u64::try_from(take).unwrap_or(u64::MAX));
            rewrite_leaf_path(store, &path, leaf_id, &removed, minimum_changed)?;
            mutated = true;
            cursor += take;
        } else if run > room {
            let prepared = if rebuilt {
                None
            } else {
                plan.as_ref()
                    .and_then(|plan| plan.hits(&entries[cursor..], leaf_id, run))
            };
            let skip_validate = prepared.is_some() && !mutated;
            let (new_root, taken) = bulk_rebalance(
                store,
                root,
                BulkRequest {
                    entries: &entries[cursor..],
                    seed_leaf: leaf_id,
                    seed_count: run,
                    path: &path,
                    prepared,
                    skip_validate,
                },
                limits,
            )?;
            rebuilt = true;
            mutated = true;
            if let Some(plan) = plan.as_mut() {
                plan.live = false;
            }
            if taken == 0 {
                root = delete(store, root, entries[cursor].0, entries[cursor].1, limits)?;
                cursor += 1;
            } else {
                root = new_root;
                cursor += taken;
            }
        } else {
            let next = delete(store, root, entries[cursor].0, entries[cursor].1, limits)?;
            // A leaf that stays at its minimum is not merged, so the other
            // leaves keep their page ids. A root change means this snapshot
            // no longer describes the tree.
            if next != root {
                rebuilt = true;
                if let Some(plan) = plan.as_mut() {
                    plan.live = false;
                }
            }
            root = next;
            mutated = true;
            cursor += 1;
        }
    }
    Ok(root)
}

/// Milestone 30 deletion. A safe shared-leaf run is rewritten once. Every
/// underflow still calls [`delete`].
fn delete_grouped(
    store: &mut dyn Nodes,
    mut root: u32,
    entries: &[Entry],
    limits: Limits,
) -> Result<u32, DbError> {
    let mut cursor = 0;
    while cursor < entries.len() {
        if root == 0 {
            return Err(missing());
        }
        let (path, leaf_id) = path_to_leaf(store, root, entries[cursor])?;
        let leaf = decode_leaf(&store.read_node(leaf_id)?)?;
        let mut claimed = vec![false; leaf.entries.len()];
        let mut run = 0;
        while cursor + run < entries.len() {
            let wanted = entries[cursor + run];
            let Some(slot) = unclaimed_slot(&leaf.entries, &claimed, wanted) else {
                break;
            };
            claimed[slot] = true;
            run += 1;
        }
        if run == 0 {
            return Err(missing());
        }
        let is_root = leaf_id == root;
        let keep = if is_root { 1 } else { limits.leaf_min.max(1) };
        let room = leaf.entries.len().saturating_sub(keep);
        let take = run.min(room);
        if take >= 2 {
            let old_first = leaf.entries[0];
            let mut removed = leaf;
            remove_leaf_entries(&mut removed, &entries[cursor..cursor + take])?;
            let minimum_changed = removed.entries[0] != old_first;
            super::profile::note_leaf_batch(u64::try_from(take).unwrap_or(u64::MAX));
            rewrite_leaf_path(store, &path, leaf_id, &removed, minimum_changed)?;
            cursor += take;
        } else {
            root = delete(store, root, entries[cursor].0, entries[cursor].1, limits)?;
            cursor += 1;
        }
    }
    Ok(root)
}

#[cfg(test)]
thread_local! {
    static FORCE_GROUPED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Use the Milestone 30 grouped delete for every run in this thread.
#[cfg(test)]
pub(crate) struct ForceGroupedDelete;

#[cfg(test)]
impl ForceGroupedDelete {
    pub(crate) fn new() -> Self {
        FORCE_GROUPED.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForceGroupedDelete {
    fn drop(&mut self) {
        FORCE_GROUPED.with(|flag| flag.set(false));
    }
}

fn grouped_only() -> bool {
    #[cfg(test)]
    {
        FORCE_GROUPED.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
thread_local! {
    static FORCE_DESCENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Check every posting with a root descent, as Milestone 31 did.
#[cfg(test)]
pub(crate) struct ForceDescentVerify;

#[cfg(test)]
impl ForceDescentVerify {
    pub(crate) fn new() -> Self {
        FORCE_DESCENT.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForceDescentVerify {
    fn drop(&mut self) {
        FORCE_DESCENT.with(|flag| flag.set(false));
    }
}

fn descent_only() -> bool {
    #[cfg(test)]
    {
        FORCE_DESCENT.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
thread_local! {
    static FORCE_CHAIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_PRESENCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_LATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Keep Milestone 32's leaf-chain check and skip the presence map.
#[cfg(test)]
pub(crate) struct ForceChainVerify;

#[cfg(test)]
impl ForceChainVerify {
    pub(crate) fn new() -> Self {
        FORCE_CHAIN.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForceChainVerify {
    fn drop(&mut self) {
        FORCE_CHAIN.with(|flag| flag.set(false));
    }
}

/// Keep Milestone 33's late map and skip the early plan.
#[cfg(test)]
pub(crate) struct ForceLatePresence;

#[cfg(test)]
impl ForceLatePresence {
    pub(crate) fn new() -> Self {
        FORCE_LATE.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForceLatePresence {
    fn drop(&mut self) {
        FORCE_LATE.with(|flag| flag.set(false));
    }
}

/// Build a presence map for a non-monotonic run even when the cost bound says
/// the descents are cheaper.
#[cfg(test)]
pub(crate) struct ForcePresenceMap;

#[cfg(test)]
impl ForcePresenceMap {
    pub(crate) fn new() -> Self {
        FORCE_PRESENCE.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForcePresenceMap {
    fn drop(&mut self) {
        FORCE_PRESENCE.with(|flag| flag.set(false));
    }
}

fn chain_only() -> bool {
    #[cfg(test)]
    {
        FORCE_CHAIN.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

fn presence_forced() -> bool {
    #[cfg(test)]
    {
        FORCE_PRESENCE.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

fn late_only() -> bool {
    #[cfg(test)]
    {
        FORCE_LATE.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

struct Hit {
    entry: Entry,
    leaf: u32,
}

struct Loaded {
    id: u32,
    entries: Vec<Entry>,
    left: u32,
    right: u32,
}

/// Remove a verified prefix that would underflow, by repacking its leaf span.
///
/// `taken == 0` means the prefix has one posting. The caller uses [`delete`]
/// and the tree is unchanged. A longer prefix is checked in request order and
/// stops before a missing or repeated pair. The existing tree is validated
/// before any page is written. A corrupt page, including one outside the
/// deleted key range, returns the corrupt error and leaves the store as it
/// was. The rebuilt tree uses the same leaf and internal page encodings.
struct BulkRequest<'a> {
    entries: &'a [Entry],
    seed_leaf: u32,
    seed_count: usize,
    path: &'a [(u32, usize)],
    prepared: Option<Vec<Hit>>,
    skip_validate: bool,
}

fn bulk_rebalance(
    store: &mut dyn Nodes,
    root: u32,
    request: BulkRequest<'_>,
    limits: Limits,
) -> Result<(u32, usize), DbError> {
    let BulkRequest {
        entries,
        seed_leaf,
        seed_count,
        path,
        prepared,
        skip_validate,
    } = request;
    let hits = if let Some(hits) = prepared {
        hits
    } else {
        collect_hits(store, root, entries, seed_leaf, seed_count, path, limits)?
    };
    if hits.len() < 2 {
        return Ok((root, 0));
    }
    let mut span = load_span(store, &hits)?;
    let min_leaf = limits.leaf_min.max(1);
    let mut took_left = false;
    let mut took_right = false;
    let survivors = loop {
        let survivors = survivor_entries(&span, &hits)?;
        let whole = span_covers_tree(&span);
        if whole || survivors.len() >= min_leaf {
            break survivors;
        }
        if !took_left && span[0].left != 0 {
            absorb_neighbor(store, &mut span, true)?;
            took_left = true;
            continue;
        }
        if !took_right && span.last().is_some_and(|leaf| leaf.right != 0) {
            absorb_neighbor(store, &mut span, false)?;
            took_right = true;
            continue;
        }
        return Err(corrupt(span[0].id));
    };
    let whole = span_covers_tree(&span);
    // Reject a tree that is already corrupt before this rebuild writes a
    // page. The walk visits every leaf and internal page, so a damaged page
    // outside the deleted keys fails here. A one-posting delete does not.
    // An early plan that validated this same root, and has not written since,
    // already established those invariants.
    if skip_validate {
        super::profile::note_validation_skipped();
    } else {
        validate(store, root, limits)?;
    }
    let old_height = height(store, root)?;
    let old_internals = internal_ids(store, root)?;
    let old_chain = leaf_chain(store, root)?;
    let packed = pack_entries(&survivors, limits, whole)?;
    let span_ids: HashSet<u32> = span.iter().map(|leaf| leaf.id).collect();
    let mut prefix = Vec::new();
    let mut suffix = Vec::new();
    let mut seen_span = false;
    let mut passed_span = false;
    for leaf in &old_chain {
        if span_ids.contains(&leaf.id) {
            if passed_span {
                return Err(corrupt(leaf.id));
            }
            seen_span = true;
        } else if seen_span {
            passed_span = true;
            suffix.push((leaf.id, leaf.entries[0]));
        } else {
            prefix.push((leaf.id, leaf.entries[0]));
        }
    }
    if !seen_span {
        return Err(corrupt(root));
    }
    if packed.is_empty() && (!prefix.is_empty() || !suffix.is_empty()) {
        return Err(DbError::Internal(
            "bulk deletion emptied a span that is not the whole tree".to_string(),
        ));
    }
    let old_ids: Vec<u32> = span.iter().map(|leaf| leaf.id).collect();
    // Every replacement page is reserved before a leaf is rewritten. An id
    // that is already in this tree, including a live page left on the free
    // list, fails here and no leaf image is replaced.
    let mut reserved: HashSet<u32> = old_chain.iter().map(|leaf| leaf.id).collect();
    reserved.extend(old_internals.iter().copied());
    let mut new_ids = Vec::with_capacity(packed.len());
    for index in 0..packed.len() {
        if index < old_ids.len() {
            new_ids.push(old_ids[index]);
        } else {
            new_ids.push(alloc_fresh(store, &mut reserved)?);
        }
    }
    let chain_len = prefix.len() + packed.len() + suffix.len();
    let internal_count = internal_page_count(chain_len, limits)?;
    let mut internal_pool = Vec::with_capacity(internal_count);
    for _ in 0..internal_count {
        internal_pool.push(alloc_fresh(store, &mut reserved)?);
    }
    let scratch = u64::try_from(hits.len() + survivors.len())
        .unwrap_or(u64::MAX)
        .saturating_mul(16);
    let merged = old_ids.len().saturating_sub(new_ids.len());
    for (index, entries) in packed.iter().enumerate() {
        let id = new_ids[index];
        let left = if index == 0 {
            prefix.last().map(|(id, _)| *id).unwrap_or(0)
        } else {
            new_ids[index - 1]
        };
        let right = if index + 1 == new_ids.len() {
            suffix.first().map(|(id, _)| *id).unwrap_or(0)
        } else {
            new_ids[index + 1]
        };
        let leaf = Leaf {
            entries: entries.clone(),
            left,
            right,
        };
        store.write_node(id, encode_leaf(id, &leaf)?)?;
    }
    if let Some(&(id, _)) = prefix.last() {
        let mut leaf = decode_leaf(&store.read_node(id)?)?;
        let right = new_ids
            .first()
            .copied()
            .or_else(|| suffix.first().map(|(id, _)| *id))
            .unwrap_or(0);
        if leaf.right != right {
            leaf.right = right;
            store.write_node(id, encode_leaf(id, &leaf)?)?;
        }
    }
    if let Some(&(id, _)) = suffix.first() {
        let mut leaf = decode_leaf(&store.read_node(id)?)?;
        let left = new_ids
            .last()
            .copied()
            .or_else(|| prefix.last().map(|(id, _)| *id))
            .unwrap_or(0);
        if leaf.left != left {
            leaf.left = left;
            store.write_node(id, encode_leaf(id, &leaf)?)?;
        }
    }
    let mut chain = prefix;
    for (index, entries) in packed.iter().enumerate() {
        chain.push((new_ids[index], entries[0]));
    }
    chain.extend(suffix);
    let new_root = build_levels(store, &chain, limits, &internal_pool)?;
    let new_height = height(store, new_root)?;
    let new_internals = if new_root == 0 {
        Vec::new()
    } else {
        internal_ids(store, new_root)?
    };
    let removed_internal = old_internals.len().saturating_sub(new_internals.len());
    let mut live: HashSet<u32> = chain.iter().map(|(id, _)| *id).collect();
    live.extend(new_internals.iter().copied());
    let mut doomed = HashSet::new();
    for id in old_ids.into_iter().skip(new_ids.len()).chain(old_internals) {
        if live.contains(&id) || !doomed.insert(id) {
            return Err(DbError::Internal(format!(
                "bulk deletion tried to free page {id} twice"
            )));
        }
        free_page(store, id)?;
    }
    validate(store, new_root, limits)
        .map_err(|error| DbError::Internal(format!("rebuilt index failed validation: {error}")))?;
    super::profile::note_scratch(scratch);
    super::profile::note_bulk_span();
    super::profile::note_leaf_redistribution();
    if merged > 0 {
        super::profile::note_leaf_merges(u64::try_from(merged).unwrap_or(u64::MAX));
    }
    if removed_internal > 0 {
        super::profile::note_internal_merges(u64::try_from(removed_internal).unwrap_or(u64::MAX));
    }
    if new_height < old_height {
        super::profile::note_root_contractions(u64::from(old_height - new_height));
    }
    Ok((new_root, hits.len()))
}

fn collect_hits(
    store: &mut dyn Nodes,
    root: u32,
    entries: &[Entry],
    seed_leaf: u32,
    seed_count: usize,
    path: &[(u32, usize)],
    limits: Limits,
) -> Result<Vec<Hit>, DbError> {
    let mut hits = Vec::new();
    let mut claimed = HashSet::new();
    for entry in entries.iter().copied().take(seed_count) {
        if !claim_posting(&mut claimed, entry) {
            super::profile::note_fallback_verified(count_u64(hits.len()));
            return Ok(hits);
        }
        hits.push(Hit {
            entry,
            leaf: seed_leaf,
        });
    }
    // The ordered walk accepts a request that only moves forward or only
    // backward. A repeated pair or a change of direction keeps the per-posting
    // descent, so the first rejected request stays the one descent would reject.
    let direction = monotonic_direction(entries);
    if seed_count > 0
        && !descent_only()
        && let Some(direction) = direction
        && let Some(extra) = verify_ordered_suffix(
            store,
            entries,
            seed_leaf,
            seed_count,
            path,
            direction == Direction::Forward,
        )?
    {
        for hit in extra {
            if !claim_posting(&mut claimed, hit.entry) {
                break;
            }
            hits.push(hit);
        }
        super::profile::note_ordered_verified(count_u64(hits.len()));
        return Ok(hits);
    }
    // A direction change is not eligible for the leaf chain. The map is a
    // local snapshot of this root. It is dropped before the rebuild writes,
    // and a later batch starts from the root that rebuild produced.
    let height = u32::try_from(path.len())
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    if !descent_only()
        && direction.is_none()
        && let Some(extra) =
            verify_presence_suffix(store, root, entries, seed_count, &claimed, height, limits)?
    {
        for hit in extra {
            if !claim_posting(&mut claimed, hit.entry) {
                break;
            }
            hits.push(hit);
        }
        super::profile::note_presence_verified(count_u64(hits.len()));
        return Ok(hits);
    }
    let mut index = hits.len();
    while index < entries.len() {
        let entry = entries[index];
        if !claim_posting(&mut claimed, entry) {
            break;
        }
        let (_path, leaf_id) = path_to_leaf(store, root, entry)?;
        let leaf = decode_leaf(&store.read_node(leaf_id)?)?;
        if !leaf_contains(&leaf.entries, entry) {
            break;
        }
        hits.push(Hit {
            entry,
            leaf: leaf_id,
        });
        index += 1;
    }
    super::profile::note_fallback_verified(count_u64(hits.len()));
    Ok(hits)
}

fn count_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

fn claim_posting(claimed: &mut HashSet<Entry>, entry: Entry) -> bool {
    super::profile::note_duplicate_check_n(1);
    claimed.insert(entry)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

/// `Some` when every adjacent pair moves the same way. Equal pairs and a
/// change of direction are `None`, and those requests keep the per-posting
/// descent. A one-entry slice is forward because it has no later posting.
fn monotonic_direction(entries: &[Entry]) -> Option<Direction> {
    let mut direction = None;
    let mut checks = 0_u64;
    for pair in entries.windows(2) {
        checks += 1;
        let step = match cmp_entry(pair[0], pair[1]) {
            Ordering::Less => Direction::Forward,
            Ordering::Greater => Direction::Backward,
            Ordering::Equal => return None,
        };
        match direction {
            None => direction = Some(step),
            Some(current) if current != step => return None,
            Some(_) => {}
        }
    }
    super::profile::note_duplicate_check_n(checks);
    super::profile::note_posting_compare_n(checks);
    Some(direction.unwrap_or(Direction::Forward))
}

fn leaf_contains(entries: &[Entry], wanted: Entry) -> bool {
    let mut compares = 0_u64;
    let found = entries.iter().any(|entry| {
        compares += 1;
        *entry == wanted
    });
    super::profile::note_posting_compare_n(compares);
    found
}

fn unclaimed_slot(entries: &[Entry], claimed: &[bool], wanted: Entry) -> Option<usize> {
    let mut compares = 0_u64;
    let slot = entries.iter().enumerate().position(|(index, entry)| {
        compares += 1;
        !claimed[index] && *entry == wanted
    });
    super::profile::note_posting_compare_n(compares);
    super::profile::note_duplicate_check_n(compares);
    slot
}

fn leaf_is_strict(entries: &[Entry]) -> bool {
    entries
        .windows(2)
        .all(|pair| cmp_entry(pair[0], pair[1]) == Ordering::Less)
}

struct PostedCompares(u64);

impl Drop for PostedCompares {
    fn drop(&mut self) {
        super::profile::note_posting_compare_n(self.0);
    }
}

/// Largest page count a tree of `height` can hold.
///
/// Each internal page has at most [`INTERNAL_MAX`] separators, so it has at
/// most that many children plus one. The sum is every level from the root
/// down to the leaves. A request whose descents would read more pages than
/// that maximum cannot be cheaper as a walk from the root.
fn max_index_pages(height: u32) -> u64 {
    let fanout = u64::try_from(INTERNAL_MAX)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut pages = 1_u64;
    let mut level = 1_u64;
    for _ in 1..height.max(1) {
        level = level.saturating_mul(fanout);
        pages = pages.saturating_add(level);
    }
    pages
}

/// `suffix` is the number of postings that would otherwise each descend from
/// the root. The comparison uses page reads, not a measured latency.
fn presence_reads_exceed_tree(suffix: usize, height: u32) -> bool {
    let height = height.max(1);
    let reads = u64::try_from(suffix)
        .unwrap_or(u64::MAX)
        .saturating_mul(u64::from(height));
    reads > max_index_pages(height)
}

fn presence_map_eligible(suffix: usize, height: u32) -> bool {
    if chain_only() {
        return false;
    }
    if presence_forced() {
        return suffix > 0;
    }
    presence_reads_exceed_tree(suffix, height)
}

/// Pages a descent-cost estimate attributes to the current tree.
///
/// `exact` is the page count read from the root and, at height 3, from each
/// of the root's children. Taller trees use that root shape as a ceiling and
/// fill every unseen level to this tree's own internal fanout. The probe does
/// not read leaves.
#[derive(Clone, Copy, Debug)]
struct ScanEstimate {
    height: u32,
    pages: u64,
    exact: bool,
}

struct LeafPlace {
    leaf: u32,
    path: Vec<(u32, usize)>,
}

/// Statement-local snapshot of one index. `live` is false after a rebuild
/// frees or reallocates pages. Leaf ids in `leaves` are then unusable.
struct PresencePlan {
    leaves: HashMap<Entry, u32>,
    paths: HashMap<u32, Vec<(u32, usize)>>,
    live: bool,
}

impl PresencePlan {
    fn place(&self, entry: Entry) -> Option<LeafPlace> {
        if !self.live {
            return None;
        }
        let leaf = *self.leaves.get(&entry)?;
        let path = self.paths.get(&leaf)?.clone();
        Some(LeafPlace { leaf, path })
    }

    /// Hits for a bulk suffix, or `None` when this snapshot does not name the
    /// seed leaf. The caller then uses the older per-posting check.
    fn hits(&self, entries: &[Entry], seed_leaf: u32, seed_count: usize) -> Option<Vec<Hit>> {
        if !self.live || seed_count > entries.len() {
            return None;
        }
        let mut hits = Vec::new();
        let mut claimed = HashSet::new();
        for entry in entries.iter().copied().take(seed_count) {
            if self.leaves.get(&entry).copied() != Some(seed_leaf) {
                return None;
            }
            if !claimed.insert(entry) {
                return Some(hits);
            }
            hits.push(Hit {
                entry,
                leaf: seed_leaf,
            });
        }
        for entry in entries.iter().copied().skip(seed_count) {
            if !claimed.insert(entry) {
                break;
            }
            match self.leaves.get(&entry).copied() {
                Some(leaf) => hits.push(Hit { entry, leaf }),
                None => break,
            }
        }
        if hits.len() < 2 { None } else { Some(hits) }
    }
}

fn direction_of(entries: &[Entry]) -> Option<Direction> {
    let mut direction = None;
    for pair in entries.windows(2) {
        let step = match cmp_entry(pair[0], pair[1]) {
            Ordering::Less => Direction::Forward,
            Ordering::Greater => Direction::Backward,
            Ordering::Equal => return None,
        };
        match direction {
            None => direction = Some(step),
            Some(current) if current != step => return None,
            Some(_) => {}
        }
    }
    Some(direction.unwrap_or(Direction::Forward))
}

/// `None` keeps the Milestone 33 decision inside the first underflow.
///
/// A monotonic run stays on the leaf-chain check. A height-1 tree is one
/// leaf, so the grouping loop already sees every posting after one descent.
/// The map is also skipped when the request has no more postings than the
/// estimate has pages: hashing every live posting would dominate that delete.
fn try_early_presence(
    store: &mut dyn Nodes,
    root: u32,
    entries: &[Entry],
    limits: Limits,
) -> Result<Option<PresencePlan>, DbError> {
    if late_only() || descent_only() || chain_only() || root == 0 || entries.is_empty() {
        return Ok(None);
    }
    if direction_of(entries).is_some() {
        return Ok(None);
    }
    let estimate = match estimate_scan(store, root, limits) {
        Ok(estimate) => estimate,
        Err(_) => return Ok(None),
    };
    super::profile::note_estimated_pages(estimate.pages);
    if !early_map_eligible(entries.len(), &estimate) {
        return Ok(None);
    }
    let built = super::profile::start();
    let mut hints = Vec::new();
    let (flat, scanned) = match validate_tree(store, root, limits, Some(&mut hints)) {
        Ok(tree) => tree,
        Err(_) => {
            super::profile::add_presence_build(built);
            return Ok(None);
        }
    };
    let Some(plan) = assemble_presence(&flat, hints) else {
        super::profile::add_presence_build(built);
        return Ok(None);
    };
    super::profile::note_scanned_pages(scanned);
    super::profile::add_presence_build(built);
    let payload = count_u64(plan.leaves.len()).saturating_mul(
        u64::try_from(std::mem::size_of::<Entry>() + std::mem::size_of::<u32>())
            .unwrap_or(u64::MAX),
    );
    let leaves = plan.leaves;
    let paths = plan.paths;
    let member = super::profile::start();
    let mut seen = HashSet::new();
    if seen.try_reserve(entries.len()).is_err() {
        super::profile::add_presence_member(member);
        return Ok(None);
    }
    let mut verified = 0_usize;
    for entry in entries.iter().copied() {
        super::profile::note_duplicate_check_n(1);
        if !seen.insert(entry) {
            break;
        }
        if leaves.contains_key(&entry) {
            super::profile::note_map_hit(1);
            verified += 1;
        } else {
            super::profile::note_map_miss();
            break;
        }
    }
    super::profile::add_presence_member(member);
    super::profile::note_presence_verified(count_u64(verified));
    super::profile::note_map_bytes(payload);
    super::profile::note_presence_build_count();
    super::profile::note_early_plan();
    Ok(Some(PresencePlan {
        leaves,
        paths,
        live: true,
    }))
}

/// Fill the map from the entries [`validate_tree`] already accepted.
///
/// `try_reserve` failure drops the hints and leaves the request unconsumed.
/// A duplicate pair or a count that does not match the validated entries
/// means the hints are not a snapshot of that walk.
fn assemble_presence(flat: &[Entry], hints: Vec<LeafHint>) -> Option<PresencePlan> {
    let recorded = super::profile::start();
    let mut leaves = HashMap::new();
    let mut paths = HashMap::new();
    if leaves.try_reserve(flat.len()).is_err() || paths.try_reserve(hints.len()).is_err() {
        super::profile::add_path_record(recorded);
        return None;
    }
    let mut cursor = 0_usize;
    for hint in hints {
        let Some(end) = cursor.checked_add(hint.count) else {
            super::profile::add_path_record(recorded);
            return None;
        };
        if end > flat.len() || paths.insert(hint.id, hint.path).is_some() {
            super::profile::add_path_record(recorded);
            return None;
        }
        for entry in &flat[cursor..end] {
            if leaves.insert(*entry, hint.id).is_some() {
                super::profile::add_path_record(recorded);
                return None;
            }
        }
        cursor = end;
    }
    super::profile::add_path_record(recorded);
    if cursor != flat.len() || leaves.len() != flat.len() {
        return None;
    }
    Some(PresencePlan {
        leaves,
        paths,
        live: true,
    })
}

fn early_map_eligible(suffix: usize, estimate: &ScanEstimate) -> bool {
    if chain_only() || descent_only() || suffix == 0 || estimate.height <= 1 {
        return false;
    }
    if presence_forced() {
        return true;
    }
    let pages = estimate.pages.max(1);
    let reads = u64::try_from(suffix)
        .unwrap_or(u64::MAX)
        .saturating_mul(u64::from(estimate.height));
    if reads <= pages {
        return false;
    }
    // An exact page count is also the tree's size. A shorter request stays on
    // the descent path so the delete does not hash every live posting. An
    // inexact count is already a ceiling, so the read comparison is enough.
    !estimate.exact || count_u64(suffix) > pages
}

fn estimate_scan(
    store: &mut dyn Nodes,
    root: u32,
    limits: Limits,
) -> Result<ScanEstimate, DbError> {
    if root == 0 {
        return Ok(ScanEstimate {
            height: 0,
            pages: 0,
            exact: true,
        });
    }
    let page = store.read_node(root)?;
    let kind = page_type(&page)?;
    if kind == TYPE_INDEX_LEAF {
        return Ok(ScanEstimate {
            height: 1,
            pages: 1,
            exact: true,
        });
    }
    if kind != TYPE_INDEX_INTERNAL {
        return Err(corrupt(root));
    }
    let root_node = decode_internal(&page)?;
    let children = root_node.children.len();
    let child_count = u64::try_from(children).unwrap_or(u64::MAX);
    let Some(first) = root_node.children.first().copied() else {
        return Err(corrupt(root));
    };
    let first_page = store.read_node(first)?;
    let first_kind = page_type(&first_page)?;
    if first_kind == TYPE_INDEX_LEAF {
        return Ok(ScanEstimate {
            height: 2,
            pages: child_count.saturating_add(1),
            exact: true,
        });
    }
    if first_kind != TYPE_INDEX_INTERNAL {
        return Err(corrupt(first));
    }
    let height = height(store, root)?;
    let fanout = u64::try_from(limits.internal_max)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let ceiling = |height| fanout_pages(fanout, height);
    if height == 3 {
        let mut leaves = 0_u64;
        for child in root_node.children {
            let Some(node) = read_internal(store, child) else {
                return Ok(ScanEstimate {
                    height,
                    pages: ceiling(height),
                    exact: false,
                });
            };
            leaves = leaves.saturating_add(u64::try_from(node.children.len()).unwrap_or(u64::MAX));
        }
        return Ok(ScanEstimate {
            height,
            pages: child_count.saturating_add(1).saturating_add(leaves),
            exact: true,
        });
    }
    // Read the root's children and stop. Each of those nodes roots a subtree
    // whose remaining levels are filled to this tree's own fanout. Counting
    // every lower internal page would be the scan the estimate is avoiding.
    let mut level_two = 0_u64;
    for child in root_node.children {
        let Some(node) = read_internal(store, child) else {
            return Ok(ScanEstimate {
                height,
                pages: ceiling(height),
                exact: false,
            });
        };
        level_two =
            level_two.saturating_add(u64::try_from(node.children.len()).unwrap_or(u64::MAX));
    }
    let below = fanout_pages(fanout, height.saturating_sub(2));
    Ok(ScanEstimate {
        height,
        pages: child_count
            .saturating_add(1)
            .saturating_add(level_two.saturating_mul(below)),
        exact: false,
    })
}

fn fanout_pages(fanout: u64, height: u32) -> u64 {
    let mut pages = 1_u64;
    let mut level = 1_u64;
    for _ in 1..height.max(1) {
        level = level.saturating_mul(fanout);
        pages = pages.saturating_add(level);
    }
    pages
}

fn read_internal(store: &mut dyn Nodes, id: u32) -> Option<Internal> {
    let page = store.read_node(id).ok()?;
    decode_internal(&page).ok()
}

/// `None` means the map is not a safe substitute. The caller repeats the
/// check with one descent per posting, so a missing pair stays missing when
/// that older check would not yet report a damaged page.
///
/// A `Some` prefix stops before the first missing or repeated pair. The map
/// is discarded by the caller. It is not used again after a rebuild.
fn verify_presence_suffix(
    store: &mut dyn Nodes,
    root: u32,
    entries: &[Entry],
    seed_count: usize,
    claimed: &HashSet<Entry>,
    height: u32,
    limits: Limits,
) -> Result<Option<Vec<Hit>>, DbError> {
    let suffix = entries.len().saturating_sub(seed_count);
    if !presence_map_eligible(suffix, height) {
        return Ok(None);
    }
    let built = super::profile::start();
    // The same validator the rebuild runs before it writes. A failure here
    // is not returned: the descent path decides whether the first request
    // fails as missing or the later rebuild reports the damaged page.
    let flat = match validate(store, root, limits) {
        Ok(flat) => flat,
        Err(_) => {
            super::profile::add_presence_build(built);
            return Ok(None);
        }
    };
    let chain = match leaf_chain(store, root) {
        Ok(chain) => chain,
        Err(_) => {
            super::profile::add_presence_build(built);
            return Ok(None);
        }
    };
    let mut count = 0_usize;
    for leaf in &chain {
        count = match count.checked_add(leaf.entries.len()) {
            Some(count) => count,
            None => {
                super::profile::add_presence_build(built);
                return Ok(None);
            }
        };
    }
    if count != flat.len() {
        super::profile::add_presence_build(built);
        return Ok(None);
    }
    let mut map = HashMap::new();
    if map.try_reserve(count).is_err() {
        super::profile::add_presence_build(built);
        return Ok(None);
    }
    let mut cursor = 0_usize;
    for (index, leaf) in chain.iter().enumerate() {
        if index > 0 {
            super::profile::note_sibling_step();
        }
        for entry in &leaf.entries {
            if flat.get(cursor) != Some(entry) || map.insert(*entry, leaf.id).is_some() {
                super::profile::add_presence_build(built);
                return Ok(None);
            }
            cursor += 1;
        }
    }
    if cursor != flat.len() {
        super::profile::add_presence_build(built);
        return Ok(None);
    }
    let payload = count_u64(count).saturating_mul(
        u64::try_from(std::mem::size_of::<Entry>() + std::mem::size_of::<u32>())
            .unwrap_or(u64::MAX),
    );
    super::profile::note_map_bytes(payload);
    super::profile::note_presence_build_count();
    super::profile::add_presence_build(built);
    let member = super::profile::start();
    let mut extra = Vec::new();
    let mut seen = HashSet::new();
    if seen.try_reserve(suffix).is_err() {
        super::profile::add_presence_member(member);
        return Ok(None);
    }
    for entry in entries.iter().copied().skip(seed_count) {
        super::profile::note_duplicate_check_n(1);
        if claimed.contains(&entry) || !seen.insert(entry) {
            break;
        }
        match map.get(&entry) {
            Some(leaf) => {
                super::profile::note_map_hit(1);
                extra.push(Hit { entry, leaf: *leaf });
            }
            None => {
                super::profile::note_map_miss();
                break;
            }
        }
    }
    super::profile::add_presence_member(member);
    Ok(Some(extra))
}

/// `None` means the chain walk is not a safe substitute, and the caller must
/// use a root descent per posting. A `Some` prefix stops before the first
/// missing pair and does not include it.
fn verify_ordered_suffix(
    store: &mut dyn Nodes,
    entries: &[Entry],
    seed_leaf: u32,
    seed_count: usize,
    path: &[(u32, usize)],
    forward: bool,
) -> Result<Option<Vec<Hit>>, DbError> {
    let loaded = match load_leaf(store, seed_leaf) {
        Ok(loaded) => loaded,
        Err(_) => return Ok(None),
    };
    if !leaf_is_strict(&loaded.entries) {
        return Ok(None);
    }
    let last_seed = entries[seed_count - 1];
    let Some(position) = loaded.entries.iter().position(|entry| *entry == last_seed) else {
        return Ok(None);
    };
    let (low, high) = match leaf_bounds(store, path) {
        Ok(bounds) => bounds,
        Err(_) => return Ok(None),
    };
    if !in_bounds(last_seed, low, high) {
        return Ok(None);
    }
    let mut cursor = LeafCursor {
        id: loaded.id,
        entries: loaded.entries,
        left: loaded.left,
        right: loaded.right,
        low,
        high,
        path: path.to_vec(),
        // Forward scans the next larger slot. Backward starts on the last
        // seed and steps to the previous slot before comparing.
        slot: if forward { position + 1 } else { position },
    };
    let mut seen = HashSet::new();
    seen.insert(cursor.id);
    let mut extra = Vec::new();
    let mut index = seed_count;
    let mut compares = PostedCompares(0);
    while index < entries.len() {
        let wanted = entries[index];
        let moved = if forward {
            while !below_high(wanted, cursor.high) {
                match step_to_next_leaf(store, &mut cursor, &mut seen) {
                    ChainStep::Moved => {}
                    ChainStep::End => return Ok(Some(extra)),
                    ChainStep::Fallback => return Ok(None),
                }
                if !leaf_is_strict(&cursor.entries) {
                    return Ok(None);
                }
            }
            !at_or_above_low(wanted, cursor.low)
        } else {
            while !at_or_above_low(wanted, cursor.low) {
                match step_to_previous_leaf(store, &mut cursor, &mut seen) {
                    ChainStep::Moved => {}
                    ChainStep::End => return Ok(Some(extra)),
                    ChainStep::Fallback => return Ok(None),
                }
                if !leaf_is_strict(&cursor.entries) {
                    return Ok(None);
                }
            }
            !below_high(wanted, cursor.high)
        };
        if moved {
            // The separator range and the leaf just entered do not agree.
            // Descent decides whether that key is missing or the page is corrupt.
            return Ok(None);
        }
        let found = if forward {
            let mut found = false;
            while cursor.slot < cursor.entries.len() {
                compares.0 += 1;
                match cmp_entry(wanted, cursor.entries[cursor.slot]) {
                    Ordering::Equal => {
                        found = true;
                        break;
                    }
                    Ordering::Less => break,
                    Ordering::Greater => cursor.slot += 1,
                }
            }
            found
        } else {
            let mut found = false;
            while cursor.slot > 0 {
                cursor.slot -= 1;
                compares.0 += 1;
                match cmp_entry(wanted, cursor.entries[cursor.slot]) {
                    Ordering::Equal => {
                        found = true;
                        break;
                    }
                    Ordering::Greater => {
                        cursor.slot += 1;
                        break;
                    }
                    Ordering::Less => {}
                }
            }
            found
        };
        if !found {
            return Ok(Some(extra));
        }
        extra.push(Hit {
            entry: wanted,
            leaf: cursor.id,
        });
        if forward {
            cursor.slot += 1;
        }
        index += 1;
    }
    Ok(Some(extra))
}

struct LeafCursor {
    id: u32,
    entries: Vec<Entry>,
    left: u32,
    right: u32,
    low: Option<Entry>,
    high: Option<Entry>,
    path: Vec<(u32, usize)>,
    slot: usize,
}

enum ChainStep {
    Moved,
    End,
    Fallback,
}

fn in_bounds(key: Entry, low: Option<Entry>, high: Option<Entry>) -> bool {
    at_or_above_low(key, low) && below_high(key, high)
}

fn at_or_above_low(key: Entry, low: Option<Entry>) -> bool {
    match low {
        None => true,
        Some(low) => cmp_entry(key, low) != Ordering::Less,
    }
}

fn below_high(key: Entry, high: Option<Entry>) -> bool {
    match high {
        None => true,
        Some(high) => cmp_entry(key, high) == Ordering::Less,
    }
}

fn leaf_bounds(
    store: &mut dyn Nodes,
    path: &[(u32, usize)],
) -> Result<(Option<Entry>, Option<Entry>), DbError> {
    let mut low = None;
    let mut high = None;
    for &(id, child_index) in path {
        let node = decode_internal(&store.read_node(id)?)?;
        if child_index >= node.children.len() {
            return Err(corrupt(id));
        }
        if child_index > 0 {
            let separator = node.keys[child_index - 1];
            low = Some(match low {
                Some(current) if cmp_entry(current, separator) == Ordering::Greater => current,
                _ => separator,
            });
        }
        if child_index < node.keys.len() {
            let separator = node.keys[child_index];
            high = Some(match high {
                Some(current) if cmp_entry(current, separator) == Ordering::Less => current,
                _ => separator,
            });
        }
    }
    Ok((low, high))
}

/// Move to the next leaf using the ancestor path. The right-sibling pointer
/// must name that same leaf and point back. A disagreement, a cycle, or a
/// page that will not decode returns [`ChainStep::Fallback`] so the caller
/// repeats the check with one descent per posting.
fn step_to_next_leaf(
    store: &mut dyn Nodes,
    cursor: &mut LeafCursor,
    seen: &mut HashSet<u32>,
) -> ChainStep {
    if cursor.path.is_empty() {
        if cursor.right != 0 {
            return ChainStep::Fallback;
        }
        return ChainStep::End;
    }
    if seen.len() > 1_000_000 {
        return ChainStep::Fallback;
    }
    let mut path = cursor.path.clone();
    let mut branched: Option<(u32, usize)> = None;
    while let Some((id, child_index)) = path.pop() {
        let page = match store.read_node(id) {
            Ok(page) => page,
            Err(_) => return ChainStep::Fallback,
        };
        let node = match decode_internal(&page) {
            Ok(node) => node,
            Err(_) => return ChainStep::Fallback,
        };
        if child_index >= node.children.len() {
            return ChainStep::Fallback;
        }
        if child_index + 1 < node.children.len() {
            branched = Some((id, child_index + 1));
            break;
        }
    }
    let Some((parent_id, child_index)) = branched else {
        if cursor.right != 0 {
            return ChainStep::Fallback;
        }
        return ChainStep::End;
    };
    path.push((parent_id, child_index));
    let parent_page = match store.read_node(parent_id) {
        Ok(page) => page,
        Err(_) => return ChainStep::Fallback,
    };
    let parent = match decode_internal(&parent_page) {
        Ok(node) => node,
        Err(_) => return ChainStep::Fallback,
    };
    if child_index >= parent.children.len() {
        return ChainStep::Fallback;
    }
    let mut id = parent.children[child_index];
    if id == 0 {
        return ChainStep::Fallback;
    }
    for _ in 0..MAX_DEPTH {
        let page = match store.read_node(id) {
            Ok(page) => page,
            Err(_) => return ChainStep::Fallback,
        };
        let kind = match page_type(&page) {
            Ok(kind) => kind,
            Err(_) => return ChainStep::Fallback,
        };
        if kind == TYPE_INDEX_LEAF {
            if id != cursor.right || !seen.insert(id) {
                return ChainStep::Fallback;
            }
            let loaded = match decode_leaf(&page) {
                Ok(leaf) => leaf,
                Err(_) => return ChainStep::Fallback,
            };
            if loaded.left != cursor.id {
                return ChainStep::Fallback;
            }
            let previous = cursor.entries.last().copied();
            if let Some(previous) = previous
                && loaded
                    .entries
                    .first()
                    .is_none_or(|entry| cmp_entry(*entry, previous) != Ordering::Greater)
            {
                return ChainStep::Fallback;
            }
            let (low, high) = match leaf_bounds(store, &path) {
                Ok(bounds) => bounds,
                Err(_) => return ChainStep::Fallback,
            };
            super::profile::note_sibling_step();
            cursor.id = id;
            cursor.left = loaded.left;
            cursor.right = loaded.right;
            cursor.entries = loaded.entries;
            cursor.low = low;
            cursor.high = high;
            cursor.path = path;
            cursor.slot = 0;
            return ChainStep::Moved;
        }
        let node = match decode_internal(&page) {
            Ok(node) => node,
            Err(_) => return ChainStep::Fallback,
        };
        let Some(child) = node.children.first().copied() else {
            return ChainStep::Fallback;
        };
        if child == 0 {
            return ChainStep::Fallback;
        }
        path.push((id, 0));
        id = child;
    }
    ChainStep::Fallback
}

/// Move to the previous leaf. The left-sibling pointer must name that leaf,
/// and that leaf's right pointer must point back.
fn step_to_previous_leaf(
    store: &mut dyn Nodes,
    cursor: &mut LeafCursor,
    seen: &mut HashSet<u32>,
) -> ChainStep {
    if cursor.path.is_empty() {
        if cursor.left != 0 {
            return ChainStep::Fallback;
        }
        return ChainStep::End;
    }
    if seen.len() > 1_000_000 {
        return ChainStep::Fallback;
    }
    let mut path = cursor.path.clone();
    let mut branched: Option<(u32, usize)> = None;
    while let Some((id, child_index)) = path.pop() {
        let page = match store.read_node(id) {
            Ok(page) => page,
            Err(_) => return ChainStep::Fallback,
        };
        let node = match decode_internal(&page) {
            Ok(node) => node,
            Err(_) => return ChainStep::Fallback,
        };
        if child_index >= node.children.len() {
            return ChainStep::Fallback;
        }
        if child_index > 0 {
            branched = Some((id, child_index - 1));
            break;
        }
    }
    let Some((parent_id, child_index)) = branched else {
        if cursor.left != 0 {
            return ChainStep::Fallback;
        }
        return ChainStep::End;
    };
    path.push((parent_id, child_index));
    let parent_page = match store.read_node(parent_id) {
        Ok(page) => page,
        Err(_) => return ChainStep::Fallback,
    };
    let parent = match decode_internal(&parent_page) {
        Ok(node) => node,
        Err(_) => return ChainStep::Fallback,
    };
    if child_index >= parent.children.len() {
        return ChainStep::Fallback;
    }
    let mut id = parent.children[child_index];
    if id == 0 {
        return ChainStep::Fallback;
    }
    for _ in 0..MAX_DEPTH {
        let page = match store.read_node(id) {
            Ok(page) => page,
            Err(_) => return ChainStep::Fallback,
        };
        let kind = match page_type(&page) {
            Ok(kind) => kind,
            Err(_) => return ChainStep::Fallback,
        };
        if kind == TYPE_INDEX_LEAF {
            if id != cursor.left || !seen.insert(id) {
                return ChainStep::Fallback;
            }
            let loaded = match decode_leaf(&page) {
                Ok(leaf) => leaf,
                Err(_) => return ChainStep::Fallback,
            };
            if loaded.right != cursor.id {
                return ChainStep::Fallback;
            }
            let current_first = cursor.entries.first().copied();
            if let Some(current_first) = current_first
                && loaded
                    .entries
                    .last()
                    .is_none_or(|entry| cmp_entry(*entry, current_first) != Ordering::Less)
            {
                return ChainStep::Fallback;
            }
            let (low, high) = match leaf_bounds(store, &path) {
                Ok(bounds) => bounds,
                Err(_) => return ChainStep::Fallback,
            };
            super::profile::note_sibling_step();
            cursor.id = id;
            cursor.left = loaded.left;
            cursor.right = loaded.right;
            cursor.entries = loaded.entries;
            cursor.low = low;
            cursor.high = high;
            cursor.path = path;
            cursor.slot = cursor.entries.len();
            return ChainStep::Moved;
        }
        let node = match decode_internal(&page) {
            Ok(node) => node,
            Err(_) => return ChainStep::Fallback,
        };
        let Some(&child) = node.children.last() else {
            return ChainStep::Fallback;
        };
        if child == 0 || node.children.is_empty() {
            return ChainStep::Fallback;
        }
        path.push((id, node.children.len() - 1));
        id = child;
    }
    ChainStep::Fallback
}

fn load_span(store: &mut dyn Nodes, hits: &[Hit]) -> Result<Vec<Loaded>, DbError> {
    let min = hits
        .iter()
        .min_by(|left, right| cmp_entry(left.entry, right.entry))
        .ok_or_else(|| corrupt(0))?;
    let max = hits
        .iter()
        .max_by(|left, right| cmp_entry(left.entry, right.entry))
        .ok_or_else(|| corrupt(0))?;
    let mut span = Vec::new();
    let mut seen = HashSet::new();
    let mut id = min.leaf;
    loop {
        if !seen.insert(id) || span.len() > 1_000_000 {
            return Err(corrupt(id));
        }
        let leaf = load_leaf(store, id)?;
        let right = leaf.right;
        span.push(leaf);
        if id == max.leaf {
            break;
        }
        if right == 0 {
            return Err(corrupt(id));
        }
        super::profile::note_sibling_step();
        id = right;
    }
    for hit in hits {
        if !seen.contains(&hit.leaf) {
            return Err(corrupt(hit.leaf));
        }
    }
    for pair in span.windows(2) {
        if pair[0].right != pair[1].id || pair[1].left != pair[0].id {
            return Err(corrupt(pair[1].id));
        }
    }
    Ok(span)
}

fn absorb_neighbor(
    store: &mut dyn Nodes,
    span: &mut Vec<Loaded>,
    left_side: bool,
) -> Result<(), DbError> {
    let (id, edge) = if left_side {
        (span[0].left, span[0].id)
    } else {
        let edge = span.last().ok_or_else(|| corrupt(0))?;
        (edge.right, edge.id)
    };
    if id == 0 || span.iter().any(|leaf| leaf.id == id) {
        return Err(corrupt(edge));
    }
    let leaf = load_leaf(store, id)?;
    if left_side {
        if leaf.right != edge {
            return Err(corrupt(id));
        }
        span.insert(0, leaf);
    } else {
        if leaf.left != edge {
            return Err(corrupt(id));
        }
        span.push(leaf);
    }
    Ok(())
}

fn span_covers_tree(span: &[Loaded]) -> bool {
    span.first().is_some_and(|leaf| leaf.left == 0)
        && span.last().is_some_and(|leaf| leaf.right == 0)
}

fn survivor_entries(span: &[Loaded], hits: &[Hit]) -> Result<Vec<Entry>, DbError> {
    let mut need: HashSet<Entry> = hits.iter().map(|hit| hit.entry).collect();
    let mut out = Vec::new();
    for leaf in span {
        for entry in &leaf.entries {
            if need.remove(entry) {
                continue;
            }
            out.push(*entry);
        }
    }
    if need.is_empty() {
        Ok(out)
    } else {
        Err(corrupt(span.first().map(|leaf| leaf.id).unwrap_or(0)))
    }
}

fn pack_entries(
    entries: &[Entry],
    limits: Limits,
    root_may_be_short: bool,
) -> Result<Vec<Vec<Entry>>, DbError> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let max = limits.leaf_max;
    let min = limits.leaf_min.max(1);
    if max < min {
        return Err(DbError::Internal(
            "leaf occupancy limits are inverted".to_string(),
        ));
    }
    if entries.len() <= max && (root_may_be_short || entries.len() >= min) {
        return Ok(vec![entries.to_vec()]);
    }
    if entries.len() < min {
        return Err(DbError::Internal(
            "bulk deletion left a non-root leaf short of its minimum".to_string(),
        ));
    }
    let mut count = entries.len().div_ceil(max);
    let max_count = entries.len() / min;
    if max_count == 0 {
        return Err(DbError::Internal(
            "bulk deletion cannot pack the surviving leaves".to_string(),
        ));
    }
    count = count.min(max_count).max(1);
    let base = entries.len() / count;
    let extra = entries.len() % count;
    if base < min || base > max || (extra > 0 && base + 1 > max) {
        return Err(DbError::Internal(
            "bulk deletion cannot pack the surviving leaves".to_string(),
        ));
    }
    let mut packed = Vec::with_capacity(count);
    let mut cursor = 0;
    for index in 0..count {
        let len = if index < extra { base + 1 } else { base };
        packed.push(entries[cursor..cursor + len].to_vec());
        cursor += len;
    }
    Ok(packed)
}

fn alloc_fresh(store: &mut dyn Nodes, reserved: &mut HashSet<u32>) -> Result<u32, DbError> {
    let id = alloc_page(store)?;
    if id == 0 || !reserved.insert(id) {
        return Err(corrupt(id));
    }
    Ok(id)
}

fn build_levels(
    store: &mut dyn Nodes,
    leaves: &[(u32, Entry)],
    limits: Limits,
    pool: &[u32],
) -> Result<u32, DbError> {
    if leaves.is_empty() {
        if pool.is_empty() {
            return Ok(0);
        }
        return Err(DbError::Internal(
            "bulk deletion reserved internal pages for an empty tree".to_string(),
        ));
    }
    if leaves.len() == 1 {
        if pool.is_empty() {
            return Ok(leaves[0].0);
        }
        return Err(DbError::Internal(
            "bulk deletion reserved internal pages for a root leaf".to_string(),
        ));
    }
    let mut level = leaves.to_vec();
    let mut cursor = 0;
    loop {
        let groups = pack_children(&level, limits)?;
        let mut next = Vec::with_capacity(groups.len());
        for group in groups {
            let Some(id) = pool.get(cursor).copied() else {
                return Err(DbError::Internal(
                    "bulk deletion ran out of internal pages".to_string(),
                ));
            };
            cursor += 1;
            let keys: Vec<Entry> = group.iter().skip(1).map(|(_, minimum)| *minimum).collect();
            let children: Vec<u32> = group.iter().map(|(id, _)| *id).collect();
            let node = Internal { keys, children };
            store.write_node(id, encode_internal(id, &node)?)?;
            next.push((id, group[0].1));
        }
        if next.len() == 1 {
            if cursor != pool.len() {
                return Err(DbError::Internal(
                    "bulk deletion reserved unused internal pages".to_string(),
                ));
            }
            return Ok(next[0].0);
        }
        level = next;
    }
}

fn internal_page_count(leaves: usize, limits: Limits) -> Result<usize, DbError> {
    if leaves <= 1 {
        return Ok(0);
    }
    let mut level = leaves;
    let mut pages = 0;
    loop {
        let groups = child_group_count(level, limits)?;
        pages += groups;
        if groups == 1 {
            return Ok(pages);
        }
        level = groups;
        if pages > 1_000_000 {
            return Err(DbError::Internal(
                "bulk deletion internal fanout did not shrink".to_string(),
            ));
        }
    }
}

fn child_group_count(children: usize, limits: Limits) -> Result<usize, DbError> {
    let max_children = limits.internal_max.saturating_add(1);
    let min_children = limits.internal_min.max(1).saturating_add(1);
    if children < 2 || max_children < min_children {
        return Err(DbError::Internal(
            "bulk deletion cannot pack the internal level".to_string(),
        ));
    }
    if children <= max_children {
        return Ok(1);
    }
    let mut count = children.div_ceil(max_children);
    let max_count = children / min_children;
    if max_count == 0 {
        return Err(DbError::Internal(
            "bulk deletion cannot pack the internal level".to_string(),
        ));
    }
    count = count.min(max_count).max(1);
    let base = children / count;
    let extra = children % count;
    if base < min_children || base > max_children || (extra > 0 && base + 1 > max_children) {
        return Err(DbError::Internal(
            "bulk deletion cannot pack the internal level".to_string(),
        ));
    }
    Ok(count)
}

fn pack_children(
    children: &[(u32, Entry)],
    limits: Limits,
) -> Result<Vec<Vec<(u32, Entry)>>, DbError> {
    let max_children = limits.internal_max.saturating_add(1);
    let min_children = limits.internal_min.max(1).saturating_add(1);
    if children.len() < 2 || max_children < min_children {
        return Err(DbError::Internal(
            "bulk deletion cannot pack the internal level".to_string(),
        ));
    }
    if children.len() <= max_children {
        return Ok(vec![children.to_vec()]);
    }
    let mut count = children.len().div_ceil(max_children);
    let max_count = children.len() / min_children;
    if max_count == 0 {
        return Err(DbError::Internal(
            "bulk deletion cannot pack the internal level".to_string(),
        ));
    }
    count = count.min(max_count).max(1);
    let base = children.len() / count;
    let extra = children.len() % count;
    if base < min_children || base > max_children || (extra > 0 && base + 1 > max_children) {
        return Err(DbError::Internal(
            "bulk deletion cannot pack the internal level".to_string(),
        ));
    }
    let mut packed = Vec::with_capacity(count);
    let mut cursor = 0;
    for index in 0..count {
        let len = if index < extra { base + 1 } else { base };
        packed.push(children[cursor..cursor + len].to_vec());
        cursor += len;
    }
    Ok(packed)
}

fn load_leaf(store: &mut dyn Nodes, id: u32) -> Result<Loaded, DbError> {
    let leaf = decode_leaf(&store.read_node(id)?)?;
    Ok(Loaded {
        id,
        entries: leaf.entries,
        left: leaf.left,
        right: leaf.right,
    })
}

fn leaf_chain(store: &mut dyn Nodes, root: u32) -> Result<Vec<Loaded>, DbError> {
    if root == 0 {
        return Ok(Vec::new());
    }
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut id = leftmost_leaf(store, root)?;
    let mut previous = 0;
    while id != 0 {
        if !seen.insert(id) || chain.len() > 1_000_000 {
            return Err(corrupt(id));
        }
        let leaf = load_leaf(store, id)?;
        if leaf.left != previous {
            return Err(corrupt(id));
        }
        previous = id;
        id = leaf.right;
        chain.push(leaf);
    }
    Ok(chain)
}

fn internal_ids(store: &mut dyn Nodes, root: u32) -> Result<Vec<u32>, DbError> {
    let mut ids = Vec::new();
    if root != 0 {
        collect_internal(store, root, &mut ids, 0)?;
    }
    Ok(ids)
}

fn collect_internal(
    store: &mut dyn Nodes,
    id: u32,
    ids: &mut Vec<u32>,
    depth: u32,
) -> Result<(), DbError> {
    if depth >= MAX_DEPTH || ids.contains(&id) {
        return Err(corrupt(id));
    }
    let page = store.read_node(id)?;
    if page_type(&page)? == TYPE_INDEX_LEAF {
        return Ok(());
    }
    ids.push(id);
    let node = decode_internal(&page)?;
    for child in node.children {
        collect_internal(store, child, ids, depth + 1)?;
    }
    Ok(())
}

/// Row ids for one integer key, in row-id order.
///
/// A right sibling that this scan follows must point back, and the keys must
/// stay strictly ordered across that boundary. A chain that ends before the
/// tree's rightmost leaf is corrupt, so a broken link cannot truncate the
/// result. Leaves this scan does not visit are not checked here. Open walks
/// the whole tree.
pub(crate) fn find_equal(store: &mut dyn Nodes, root: u32, key: i64) -> Result<Vec<u64>, DbError> {
    let mut out = Vec::new();
    if root == 0 {
        return Ok(out);
    }
    let mut seen = HashSet::new();
    let mut leaf_id = find_leaf(store, root, (key, 0))?;
    loop {
        if !seen.insert(leaf_id) {
            return Err(corrupt(leaf_id));
        }
        let leaf = decode_leaf(&store.read_node(leaf_id)?)?;
        require_strict_order(&leaf, leaf_id)?;
        let mut passed = false;
        for entry in &leaf.entries {
            if entry.0 == key {
                out.push(entry.1);
            } else if entry.0 > key {
                passed = true;
                break;
            }
        }
        match advance_leaf(store, root, leaf_id, &leaf, passed)? {
            Some(next) => leaf_id = next,
            None => break,
        }
    }
    Ok(out)
}

/// Inclusive integer range in key order, then row id.
pub(crate) fn find_range(
    store: &mut dyn Nodes,
    root: u32,
    low: Option<i64>,
    high: Option<i64>,
) -> Result<Vec<Entry>, DbError> {
    let mut out = Vec::new();
    if root == 0 {
        return Ok(out);
    }
    let mut leaf_id = match low {
        Some(key) => find_leaf(store, root, (key, 0))?,
        None => leftmost_leaf(store, root)?,
    };
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(leaf_id) {
            return Err(corrupt(leaf_id));
        }
        let leaf = decode_leaf(&store.read_node(leaf_id)?)?;
        require_strict_order(&leaf, leaf_id)?;
        let mut passed = false;
        for entry in &leaf.entries {
            if low.is_some_and(|key| entry.0 < key) {
                continue;
            }
            if high.is_some_and(|key| entry.0 > key) {
                passed = true;
                break;
            }
            out.push(*entry);
        }
        match advance_leaf(store, root, leaf_id, &leaf, passed)? {
            Some(next) => leaf_id = next,
            None => break,
        }
    }
    Ok(out)
}

/// The next leaf, or `None` when this leaf finishes the scan.
///
/// `passed` means the caller already saw a key past the requested range, so
/// the right sibling is not part of the result and is not inspected.
fn advance_leaf(
    store: &mut dyn Nodes,
    root: u32,
    leaf_id: u32,
    leaf: &Leaf,
    passed: bool,
) -> Result<Option<u32>, DbError> {
    if passed {
        return Ok(None);
    }
    if leaf.right == 0 {
        if rightmost_leaf(store, root)? != leaf_id {
            return Err(corrupt(leaf_id));
        }
        return Ok(None);
    }
    let next_id = leaf.right;
    let next = decode_leaf(&store.read_node(next_id)?)?;
    require_strict_order(&next, next_id)?;
    if next.left != leaf_id {
        return Err(corrupt(next_id));
    }
    match (leaf.entries.last(), next.entries.first()) {
        (Some(last), Some(first)) if cmp_entry(*last, *first) == Ordering::Less => {}
        _ => return Err(corrupt(next_id)),
    }
    Ok(Some(next_id))
}

/// Minimum and maximum integer keys, or `None` when the tree is empty.
///
/// The two lookups follow the edges of the tree. A leaf that is not strictly
/// ordered is corrupt. This does not count entries or assume how they are
/// distributed.
pub(crate) fn stored_key_span(
    store: &mut dyn Nodes,
    root: u32,
) -> Result<Option<(i64, i64)>, DbError> {
    if root == 0 {
        return Ok(None);
    }
    let min = leftmost(store, root)?.0;
    let leaf_id = rightmost_leaf(store, root)?;
    let leaf = decode_leaf(&store.read_node(leaf_id)?)?;
    require_strict_order(&leaf, leaf_id)?;
    let Some(max) = leaf.entries.last().map(|entry| entry.0) else {
        return Err(corrupt(leaf_id));
    };
    if min > max {
        return Err(corrupt(leaf_id));
    }
    Ok(Some((min, max)))
}

/// Every pair in ascending order.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn iter_asc(store: &mut dyn Nodes, root: u32) -> Result<Vec<Entry>, DbError> {
    find_range(store, root, None, None)
}

/// Every pair in descending order, walking left sibling pointers.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn iter_desc(store: &mut dyn Nodes, root: u32) -> Result<Vec<Entry>, DbError> {
    let mut out = Vec::new();
    if root == 0 {
        return Ok(out);
    }
    let mut seen = HashSet::new();
    let mut leaf_id = rightmost_leaf(store, root)?;
    loop {
        if !seen.insert(leaf_id) {
            return Err(corrupt(leaf_id));
        }
        let leaf = decode_leaf(&store.read_node(leaf_id)?)?;
        for entry in leaf.entries.iter().rev() {
            out.push(*entry);
        }
        if leaf.left == 0 {
            break;
        }
        leaf_id = leaf.left;
    }
    Ok(out)
}

/// Levels from the root through a leaf. An empty tree has height 0.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn height(store: &mut dyn Nodes, root: u32) -> Result<u32, DbError> {
    if root == 0 {
        return Ok(0);
    }
    let mut id = root;
    let mut levels = 1;
    loop {
        let page = store.read_node(id)?;
        if page_type(&page)? == TYPE_INDEX_LEAF {
            return Ok(levels);
        }
        let node = decode_internal(&page)?;
        id = node.children[0];
        levels = levels.saturating_add(1);
        if levels > 64 {
            return Err(corrupt(id));
        }
    }
}

/// Structural check. `root == 0` is an empty tree.
pub(crate) fn validate(
    store: &mut dyn Nodes,
    root: u32,
    limits: Limits,
) -> Result<Vec<Entry>, DbError> {
    validate_tree(store, root, limits, None).map(|(flat, _)| flat)
}

/// One structural walk. `hints` records each leaf's parent path and entry
/// count for the caller that is about to build a presence map. The checks are
/// the same whether or not that record is requested.
fn validate_tree(
    store: &mut dyn Nodes,
    root: u32,
    limits: Limits,
    hints: Option<&mut Vec<LeafHint>>,
) -> Result<(Vec<Entry>, u64), DbError> {
    super::profile::note_tree_validation();
    if root == 0 {
        return Ok((Vec::new(), 0));
    }
    let mut seen = Vec::new();
    let mut leaves = Vec::new();
    {
        let mut walk = Walk {
            seen: &mut seen,
            leaves: &mut leaves,
            hints,
            path: Vec::new(),
        };
        let _ = check_node(store, root, true, limits, &mut walk, 0)?;
    }
    let scanned = count_u64(seen.len());
    let mut previous = 0;
    let mut flat = Vec::new();
    for (index, id) in leaves.iter().copied().enumerate() {
        if seen.iter().filter(|page| **page == id).count() != 1 {
            return Err(corrupt(id));
        }
        let leaf = decode_leaf(&store.read_node(id)?)?;
        if leaf.left != previous {
            return Err(corrupt(id));
        }
        if index + 1 == leaves.len() {
            if leaf.right != 0 {
                return Err(corrupt(id));
            }
        } else if leaf.right != leaves[index + 1] {
            return Err(corrupt(id));
        }
        flat.extend(leaf.entries);
        previous = id;
    }
    for pair in flat.windows(2) {
        if cmp_entry(pair[0], pair[1]) != Ordering::Less {
            return Err(corrupt(root));
        }
    }
    Ok((flat, scanned))
}

/// Parent path and entry count of one leaf, captured during [`validate_tree`].
struct LeafHint {
    id: u32,
    path: Vec<(u32, usize)>,
    count: usize,
}

struct Walk<'a> {
    seen: &'a mut Vec<u32>,
    leaves: &'a mut Vec<u32>,
    hints: Option<&'a mut Vec<LeafHint>>,
    path: Vec<(u32, usize)>,
}

/// Every page the tree owns, including the root.
pub(crate) fn owned_pages(store: &mut dyn Nodes, root: u32) -> Result<Vec<u32>, DbError> {
    let mut pages = Vec::new();
    if root != 0 {
        collect(store, root, &mut pages, 0)?;
    }
    Ok(pages)
}

fn collect(
    store: &mut dyn Nodes,
    id: u32,
    pages: &mut Vec<u32>,
    depth: u32,
) -> Result<(), DbError> {
    if depth >= MAX_DEPTH || pages.contains(&id) {
        return Err(corrupt(id));
    }
    pages.push(id);
    let page = store.read_node(id)?;
    if page_type(&page)? == TYPE_INDEX_LEAF {
        return Ok(());
    }
    let node = decode_internal(&page)?;
    for child in node.children {
        collect(store, child, pages, depth + 1)?;
    }
    Ok(())
}

fn check_node(
    store: &mut dyn Nodes,
    id: u32,
    is_root: bool,
    limits: Limits,
    walk: &mut Walk<'_>,
    depth: u32,
) -> Result<Entry, DbError> {
    if depth >= MAX_DEPTH || !walk.seen.iter().all(|page| *page != id) {
        return Err(corrupt(id));
    }
    walk.seen.push(id);
    let page = store.read_node(id)?;
    match page_type(&page)? {
        TYPE_INDEX_LEAF => {
            let leaf = decode_leaf(&page)?;
            if leaf.entries.is_empty() || leaf.entries.len() > limits.leaf_max {
                return Err(corrupt(id));
            }
            if !is_root && leaf.entries.len() < limits.leaf_min {
                return Err(corrupt(id));
            }
            for pair in leaf.entries.windows(2) {
                if cmp_entry(pair[0], pair[1]) != Ordering::Less {
                    return Err(corrupt(id));
                }
            }
            if let Some(hints) = walk.hints.as_mut() {
                hints.push(LeafHint {
                    id,
                    path: walk.path.clone(),
                    count: leaf.entries.len(),
                });
            }
            walk.leaves.push(id);
            Ok(leaf.entries[0])
        }
        TYPE_INDEX_INTERNAL => {
            let node = decode_internal(&page)?;
            if node.keys.is_empty()
                || node.keys.len() > limits.internal_max
                || node.children.len() != node.keys.len() + 1
            {
                return Err(corrupt(id));
            }
            if !is_root && node.keys.len() < limits.internal_min {
                return Err(corrupt(id));
            }
            for pair in node.keys.windows(2) {
                if cmp_entry(pair[0], pair[1]) != Ordering::Less {
                    return Err(corrupt(id));
                }
            }
            let mut child_min = None;
            for (index, child) in node.children.iter().copied().enumerate() {
                walk.path.push((id, index));
                let minimum = check_node(store, child, false, limits, walk, depth + 1)?;
                walk.path.pop();
                if index == 0 {
                    child_min = Some(minimum);
                } else if node.keys[index - 1] != minimum {
                    return Err(corrupt(id));
                }
            }
            child_min.ok_or_else(|| corrupt(id))
        }
        _ => Err(corrupt(id)),
    }
}

fn insert_at(
    store: &mut dyn Nodes,
    id: u32,
    entry: Entry,
    limits: Limits,
    depth: u32,
) -> Result<Inserted, DbError> {
    if depth >= MAX_DEPTH {
        return Err(corrupt(id));
    }
    let page = store.read_node(id)?;
    if page_type(&page)? == TYPE_INDEX_LEAF {
        let mut leaf = decode_leaf(&page)?;
        match leaf
            .entries
            .binary_search_by(|item| cmp_entry(*item, entry))
        {
            Ok(_) => {
                return Err(DbError::Internal(format!(
                    "duplicate index entry ({}, {})",
                    entry.0, entry.1
                )));
            }
            Err(index) => leaf.entries.insert(index, entry),
        }
        if leaf.entries.len() <= limits.leaf_max {
            let minimum = leaf.entries[0];
            store.write_node(id, encode_leaf(id, &leaf)?)?;
            return Ok(Inserted::Fit { minimum });
        }
        let right_id = alloc_page(store)?;
        let split_at = limits.leaf_max / 2;
        let right_entries = leaf.entries.split_off(split_at);
        let separator = right_entries[0];
        let right = Leaf {
            entries: right_entries,
            left: id,
            right: leaf.right,
        };
        if leaf.right != 0 {
            let mut neighbor = decode_leaf(&store.read_node(leaf.right)?)?;
            neighbor.left = right_id;
            let neighbor_id = leaf.right;
            store.write_node(neighbor_id, encode_leaf(neighbor_id, &neighbor)?)?;
        }
        leaf.right = right_id;
        let minimum = leaf.entries[0];
        store.write_node(id, encode_leaf(id, &leaf)?)?;
        store.write_node(right_id, encode_leaf(right_id, &right)?)?;
        return Ok(Inserted::Split {
            minimum,
            separator,
            right: right_id,
        });
    }

    let mut node = decode_internal(&page)?;
    let child_index = child_slot(&node.keys, entry);
    let child = node.children[child_index];
    match insert_at(store, child, entry, limits, depth + 1)? {
        Inserted::Fit { minimum } => {
            if child_index > 0 {
                node.keys[child_index - 1] = minimum;
            }
            let own = subtree_min(store, node.children[0])?;
            store.write_node(id, encode_internal(id, &node)?)?;
            Ok(Inserted::Fit { minimum: own })
        }
        Inserted::Split {
            minimum,
            separator,
            right,
        } => {
            if child_index > 0 {
                node.keys[child_index - 1] = minimum;
            }
            node.keys.insert(child_index, separator);
            node.children.insert(child_index + 1, right);
            if node.keys.len() <= limits.internal_max {
                let own = subtree_min(store, node.children[0])?;
                store.write_node(id, encode_internal(id, &node)?)?;
                return Ok(Inserted::Fit { minimum: own });
            }
            split_internal(store, id, node, limits)
        }
    }
}

fn split_internal(
    store: &mut dyn Nodes,
    id: u32,
    mut node: Internal,
    limits: Limits,
) -> Result<Inserted, DbError> {
    let promote = limits.internal_max / 2;
    let separator = node.keys[promote];
    let right_keys = node.keys.split_off(promote + 1);
    let right_children = node.children.split_off(promote + 1);
    node.keys.pop();
    let left_min = subtree_min(store, node.children[0])?;
    let right_id = alloc_page(store)?;
    let right = Internal {
        keys: right_keys,
        children: right_children,
    };
    store.write_node(id, encode_internal(id, &node)?)?;
    store.write_node(right_id, encode_internal(right_id, &right)?)?;
    Ok(Inserted::Split {
        minimum: left_min,
        separator,
        right: right_id,
    })
}

fn delete_at(
    store: &mut dyn Nodes,
    id: u32,
    entry: Entry,
    is_root: bool,
    limits: Limits,
    depth: u32,
) -> Result<Removed, DbError> {
    if depth >= MAX_DEPTH {
        return Err(corrupt(id));
    }
    let page = store.read_node(id)?;
    if page_type(&page)? == TYPE_INDEX_LEAF {
        let mut leaf = decode_leaf(&page)?;
        let Some(index) = leaf.entries.iter().position(|item| *item == entry) else {
            return Err(missing());
        };
        leaf.entries.remove(index);
        if leaf.entries.is_empty() {
            if !is_root {
                return Err(DbError::Internal(
                    "a non-root leaf became empty before it was merged".to_string(),
                ));
            }
            return Ok(Removed::Empty);
        }
        store.write_node(id, encode_leaf(id, &leaf)?)?;
        let minimum = leaf.entries[0];
        if !is_root && leaf.entries.len() < limits.leaf_min {
            return Ok(Removed::Underfull { minimum });
        }
        return Ok(Removed::Present { minimum });
    }

    let mut node = decode_internal(&page)?;
    let child_index = child_slot(&node.keys, entry);
    let child = node.children[child_index];
    match delete_at(store, child, entry, false, limits, depth + 1)? {
        Removed::Empty => Err(DbError::Internal(
            "an internal child became empty".to_string(),
        )),
        Removed::Collapsed { .. } => Err(DbError::Internal(
            "a non-root index node lost every child".to_string(),
        )),
        Removed::Present { minimum } => {
            if child_index > 0 {
                node.keys[child_index - 1] = minimum;
            }
            let own = subtree_min(store, node.children[0])?;
            store.write_node(id, encode_internal(id, &node)?)?;
            Ok(Removed::Present { minimum: own })
        }
        Removed::Underfull { minimum } => {
            if child_index > 0 {
                node.keys[child_index - 1] = minimum;
            }
            rebalance(store, &mut node, child_index, limits)?;
            if node.keys.is_empty() && is_root {
                return Ok(Removed::Collapsed {
                    child: node.children[0],
                });
            }
            let own = subtree_min(store, node.children[0])?;
            let under = !is_root && node.keys.len() < limits.internal_min;
            store.write_node(id, encode_internal(id, &node)?)?;
            if under {
                Ok(Removed::Underfull { minimum: own })
            } else {
                Ok(Removed::Present { minimum: own })
            }
        }
    }
}

fn rebalance(
    store: &mut dyn Nodes,
    parent: &mut Internal,
    index: usize,
    limits: Limits,
) -> Result<(), DbError> {
    let id = parent.children[index];
    let page = store.read_node(id)?;
    if page_type(&page)? == TYPE_INDEX_LEAF {
        rebalance_leaf(store, parent, index, limits)
    } else {
        rebalance_internal(store, parent, index, limits)
    }
}

fn rebalance_leaf(
    store: &mut dyn Nodes,
    parent: &mut Internal,
    index: usize,
    limits: Limits,
) -> Result<(), DbError> {
    let id = parent.children[index];
    let mut leaf = decode_leaf(&store.read_node(id)?)?;
    if index + 1 < parent.children.len() {
        let right_id = parent.children[index + 1];
        let mut right = decode_leaf(&store.read_node(right_id)?)?;
        if right.entries.len() > limits.leaf_min {
            leaf.entries.push(right.entries.remove(0));
            parent.keys[index] = right.entries[0];
            store.write_node(id, encode_leaf(id, &leaf)?)?;
            store.write_node(right_id, encode_leaf(right_id, &right)?)?;
            return Ok(());
        }
    }
    if index > 0 {
        let left_id = parent.children[index - 1];
        let mut left = decode_leaf(&store.read_node(left_id)?)?;
        if left.entries.len() > limits.leaf_min {
            let moved = left.entries.pop().expect("donor leaf");
            leaf.entries.insert(0, moved);
            parent.keys[index - 1] = leaf.entries[0];
            store.write_node(id, encode_leaf(id, &leaf)?)?;
            store.write_node(left_id, encode_leaf(left_id, &left)?)?;
            return Ok(());
        }
    }
    if index + 1 < parent.children.len() {
        let right_id = parent.children[index + 1];
        let right = decode_leaf(&store.read_node(right_id)?)?;
        leaf.entries.extend(right.entries);
        leaf.right = right.right;
        if right.right != 0 {
            let mut neighbor = decode_leaf(&store.read_node(right.right)?)?;
            neighbor.left = id;
            let neighbor_id = right.right;
            store.write_node(neighbor_id, encode_leaf(neighbor_id, &neighbor)?)?;
        }
        parent.keys.remove(index);
        parent.children.remove(index + 1);
        store.write_node(id, encode_leaf(id, &leaf)?)?;
        free_page(store, right_id)?;
        return Ok(());
    }
    let left_id = parent.children[index - 1];
    let mut left = decode_leaf(&store.read_node(left_id)?)?;
    left.entries.extend(leaf.entries);
    left.right = leaf.right;
    if leaf.right != 0 {
        let mut neighbor = decode_leaf(&store.read_node(leaf.right)?)?;
        neighbor.left = left_id;
        let neighbor_id = leaf.right;
        store.write_node(neighbor_id, encode_leaf(neighbor_id, &neighbor)?)?;
    }
    parent.keys.remove(index - 1);
    parent.children.remove(index);
    store.write_node(left_id, encode_leaf(left_id, &left)?)?;
    free_page(store, id)?;
    Ok(())
}

fn rebalance_internal(
    store: &mut dyn Nodes,
    parent: &mut Internal,
    index: usize,
    limits: Limits,
) -> Result<(), DbError> {
    let id = parent.children[index];
    let mut node = decode_internal(&store.read_node(id)?)?;
    if index + 1 < parent.children.len() {
        let right_id = parent.children[index + 1];
        let mut right = decode_internal(&store.read_node(right_id)?)?;
        if right.keys.len() > limits.internal_min {
            node.keys.push(parent.keys[index]);
            node.children.push(right.children.remove(0));
            parent.keys[index] = right.keys.remove(0);
            store.write_node(id, encode_internal(id, &node)?)?;
            store.write_node(right_id, encode_internal(right_id, &right)?)?;
            return Ok(());
        }
    }
    if index > 0 {
        let left_id = parent.children[index - 1];
        let mut left = decode_internal(&store.read_node(left_id)?)?;
        if left.keys.len() > limits.internal_min {
            let moved = left.children.pop().expect("donor child");
            let moved_key = left.keys.pop().expect("donor key");
            node.children.insert(0, moved);
            node.keys.insert(0, parent.keys[index - 1]);
            parent.keys[index - 1] = moved_key;
            store.write_node(id, encode_internal(id, &node)?)?;
            store.write_node(left_id, encode_internal(left_id, &left)?)?;
            return Ok(());
        }
    }
    if index + 1 < parent.children.len() {
        let right_id = parent.children[index + 1];
        let right = decode_internal(&store.read_node(right_id)?)?;
        node.keys.push(parent.keys[index]);
        node.keys.extend(right.keys);
        node.children.extend(right.children);
        parent.keys.remove(index);
        parent.children.remove(index + 1);
        store.write_node(id, encode_internal(id, &node)?)?;
        free_page(store, right_id)?;
        return Ok(());
    }
    let left_id = parent.children[index - 1];
    let mut left = decode_internal(&store.read_node(left_id)?)?;
    left.keys.push(parent.keys[index - 1]);
    left.keys.extend(node.keys);
    left.children.extend(node.children);
    parent.keys.remove(index - 1);
    parent.children.remove(index);
    store.write_node(left_id, encode_internal(left_id, &left)?)?;
    free_page(store, id)?;
    Ok(())
}

fn path_to_leaf(
    store: &mut dyn Nodes,
    root: u32,
    probe: Entry,
) -> Result<(Vec<(u32, usize)>, u32), DbError> {
    super::profile::note_descent();
    let mut path = Vec::new();
    let mut id = root;
    for _ in 0..MAX_DEPTH {
        let page = store.read_node(id)?;
        if page_type(&page)? == TYPE_INDEX_LEAF {
            return Ok((path, id));
        }
        let node = decode_internal(&page)?;
        let child_index = child_slot(&node.keys, probe);
        let child = node
            .children
            .get(child_index)
            .copied()
            .ok_or_else(|| corrupt(id))?;
        path.push((id, child_index));
        id = child;
    }
    Err(corrupt(root))
}

fn remove_leaf_entries(leaf: &mut Leaf, entries: &[Entry]) -> Result<(), DbError> {
    let mut used = vec![false; leaf.entries.len()];
    let mut positions = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(index) = leaf
            .entries
            .iter()
            .enumerate()
            .position(|(index, item)| !used[index] && item == entry)
        else {
            return Err(missing());
        };
        used[index] = true;
        positions.push(index);
    }
    positions.sort_unstable_by(|left, right| right.cmp(left));
    for index in positions {
        leaf.entries.remove(index);
    }
    if leaf.entries.is_empty() {
        return Err(DbError::Internal(
            "a batched leaf removal emptied the leaf".to_string(),
        ));
    }
    Ok(())
}

/// Write the leaf and re-encode every ancestor on `path`.
///
/// Only the separator that covers this leaf changes, and only when its minimum
/// entry changed. Higher separators stay put. Rewriting the unchanged
/// ancestors matches the pages a series of single deletes would seal.
///
/// Each ancestor reads its leftmost child, which is the read a single delete
/// uses to recompute that node's minimum. A corrupt page on that walk fails
/// the batch before the leaf is written.
fn rewrite_leaf_path(
    store: &mut dyn Nodes,
    path: &[(u32, usize)],
    leaf_id: u32,
    leaf: &Leaf,
    minimum_changed: bool,
) -> Result<(), DbError> {
    let minimum = leaf
        .entries
        .first()
        .copied()
        .ok_or_else(|| corrupt(leaf_id))?;
    for &(id, _) in path.iter().rev() {
        let node = decode_internal(&store.read_node(id)?)?;
        let child = node.children.first().copied().ok_or_else(|| corrupt(id))?;
        let _ = subtree_min(store, child)?;
    }
    store.write_node(leaf_id, encode_leaf(leaf_id, leaf)?)?;
    let mut branched = false;
    for &(id, child_index) in path.iter().rev() {
        let mut node = decode_internal(&store.read_node(id)?)?;
        if child_index >= node.children.len() {
            return Err(corrupt(id));
        }
        if !branched && child_index > 0 {
            if minimum_changed {
                node.keys[child_index - 1] = minimum;
            }
            branched = true;
        }
        store.write_node(id, encode_internal(id, &node)?)?;
    }
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
fn find_leaf(store: &mut dyn Nodes, root: u32, probe: Entry) -> Result<u32, DbError> {
    let mut id = root;
    for _ in 0..64 {
        let page = store.read_node(id)?;
        if page_type(&page)? == TYPE_INDEX_LEAF {
            return Ok(id);
        }
        let node = decode_internal(&page)?;
        id = node.children[child_slot(&node.keys, probe)];
    }
    Err(corrupt(root))
}

fn rightmost_leaf(store: &mut dyn Nodes, root: u32) -> Result<u32, DbError> {
    let mut id = root;
    for _ in 0..64 {
        let page = store.read_node(id)?;
        if page_type(&page)? == TYPE_INDEX_LEAF {
            return Ok(id);
        }
        let node = decode_internal(&page)?;
        id = node.children[node.children.len() - 1];
    }
    Err(corrupt(root))
}

fn leftmost_leaf(store: &mut dyn Nodes, root: u32) -> Result<u32, DbError> {
    let mut id = root;
    for _ in 0..64 {
        let page = store.read_node(id)?;
        if page_type(&page)? == TYPE_INDEX_LEAF {
            return Ok(id);
        }
        id = decode_internal(&page)?.children[0];
    }
    Err(corrupt(root))
}

#[cfg(test)]
pub(crate) fn leftmost_leaf_id(store: &mut dyn Nodes, root: u32) -> Result<u32, DbError> {
    leftmost_leaf(store, root)
}

fn require_strict_order(leaf: &Leaf, leaf_id: u32) -> Result<(), DbError> {
    let mut previous = None;
    for entry in &leaf.entries {
        if let Some(prev) = previous
            && cmp_entry(*entry, prev) != Ordering::Greater
        {
            return Err(corrupt(leaf_id));
        }
        previous = Some(*entry);
    }
    Ok(())
}

fn leftmost(store: &mut dyn Nodes, root: u32) -> Result<Entry, DbError> {
    let id = leftmost_leaf(store, root)?;
    let leaf = decode_leaf(&store.read_node(id)?)?;
    leaf.entries.first().copied().ok_or_else(|| corrupt(id))
}

fn subtree_min(store: &mut dyn Nodes, id: u32) -> Result<Entry, DbError> {
    leftmost(store, id)
}

fn child_slot(keys: &[Entry], probe: Entry) -> usize {
    keys.iter()
        .position(|separator| cmp_entry(probe, *separator) == Ordering::Less)
        .unwrap_or(keys.len())
}

fn cmp_entry(left: Entry, right: Entry) -> Ordering {
    left.0.cmp(&right.0).then(left.1.cmp(&right.1))
}

fn encode_leaf(id: u32, leaf: &Leaf) -> Result<Page, DbError> {
    if leaf.entries.len() > LEAF_MAX {
        return Err(DbError::Internal(
            "leaf exceeds the page capacity".to_string(),
        ));
    }
    let mut page = blank(TYPE_INDEX_LEAF, id);
    let bytes = page.as_mut_slice();
    write_u16(bytes, LEAF_COUNT, leaf.entries.len() as u16);
    for (index, entry) in leaf.entries.iter().enumerate() {
        let offset = LEAF_BODY + index * ENTRY_LEN;
        write_i64(bytes, offset, entry.0);
        write_u64(bytes, offset + 8, entry.1);
    }
    write_u32(bytes, OFF_NEXT, leaf.right);
    write_u32(bytes, LEAF_LEFT, leaf.left);
    Ok(page)
}

fn decode_leaf(page: &Page) -> Result<Leaf, DbError> {
    super::profile::note_leaf_decode();
    let bytes = page.as_slice();
    let id = read_u32(bytes, OFF_ID);
    if page_type(page)? != TYPE_INDEX_LEAF || read_u16(bytes, OFF_SLOTS) != 0 {
        return Err(corrupt(id));
    }
    let count = usize::from(read_u16(bytes, LEAF_COUNT));
    if count == 0 || count > LEAF_MAX || read_u16(bytes, LEAF_COUNT + 2) != 0 {
        return Err(corrupt(id));
    }
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let offset = LEAF_BODY + index * ENTRY_LEN;
        entries.push((read_i64(bytes, offset), read_u64(bytes, offset + 8)));
    }
    Ok(Leaf {
        entries,
        left: read_u32(bytes, LEAF_LEFT),
        right: read_u32(bytes, OFF_NEXT),
    })
}

fn encode_internal(id: u32, node: &Internal) -> Result<Page, DbError> {
    if node.keys.len() > INTERNAL_MAX || node.children.len() != node.keys.len() + 1 {
        return Err(DbError::Internal(
            "internal node exceeds the page capacity".to_string(),
        ));
    }
    let mut page = blank(TYPE_INDEX_INTERNAL, id);
    let bytes = page.as_mut_slice();
    write_u16(bytes, INTERNAL_COUNT, node.keys.len() as u16);
    write_u32(bytes, INTERNAL_LEFTMOST, node.children[0]);
    for (index, key) in node.keys.iter().enumerate() {
        let offset = INTERNAL_LEFTMOST + 4 + index * SEPARATOR_LEN;
        write_i64(bytes, offset, key.0);
        write_u64(bytes, offset + 8, key.1);
        write_u32(bytes, offset + 16, node.children[index + 1]);
    }
    Ok(page)
}

fn decode_internal(page: &Page) -> Result<Internal, DbError> {
    let bytes = page.as_slice();
    let id = read_u32(bytes, OFF_ID);
    if page_type(page)? != TYPE_INDEX_INTERNAL
        || read_u16(bytes, OFF_SLOTS) != 0
        || read_u32(bytes, OFF_NEXT) != 0
        || read_u32(bytes, PAGE_SIZE - 4) != 0
    {
        return Err(corrupt(id));
    }
    let count = usize::from(read_u16(bytes, INTERNAL_COUNT));
    if count == 0 || count > INTERNAL_MAX || read_u16(bytes, INTERNAL_COUNT + 2) != 0 {
        return Err(corrupt(id));
    }
    let mut children = Vec::with_capacity(count + 1);
    children.push(read_u32(bytes, INTERNAL_LEFTMOST));
    let mut keys = Vec::with_capacity(count);
    for index in 0..count {
        let offset = INTERNAL_LEFTMOST + 4 + index * SEPARATOR_LEN;
        keys.push((read_i64(bytes, offset), read_u64(bytes, offset + 8)));
        children.push(read_u32(bytes, offset + 16));
    }
    if children.contains(&0) {
        return Err(corrupt(id));
    }
    Ok(Internal { keys, children })
}

fn blank(page_type: u16, id: u32) -> Page {
    let mut page = Page::new();
    let bytes = page.as_mut_slice();
    write_u16(bytes, OFF_TYPE, page_type);
    write_u32(bytes, OFF_ID, id);
    write_u16(bytes, OFF_LOWER, HEADER_LEN as u16);
    write_u16(bytes, OFF_UPPER, HEADER_LEN as u16);
    page
}

fn page_type(page: &Page) -> Result<u16, DbError> {
    Ok(read_u16(page.as_slice(), OFF_TYPE))
}

fn missing() -> DbError {
    DbError::Internal("index entry is missing".to_string())
}

fn corrupt(page_id: u32) -> DbError {
    DbError::Storage(format!("database page {page_id} is corrupt"))
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

fn write_i64(bytes: &mut [u8], offset: usize, value: i64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("u16"))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("u64"))
}

fn read_i64(bytes: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("i64"))
}

/// Change one leaf slot in a cached page. The page is not resealed.
#[cfg(test)]
pub(crate) fn patch_leaf_entry(
    page: &mut Page,
    slot: usize,
    key: Option<i64>,
    row_id: Option<u64>,
) -> Result<(), DbError> {
    let id = read_u32(page.as_slice(), OFF_ID);
    if read_u16(page.as_slice(), OFF_TYPE) != TYPE_INDEX_LEAF {
        return Err(corrupt(id));
    }
    let count = usize::from(read_u16(page.as_slice(), LEAF_COUNT));
    if slot >= count {
        return Err(corrupt(id));
    }
    let offset = LEAF_BODY + slot * ENTRY_LEN;
    let bytes = page.as_mut_slice();
    if let Some(key) = key {
        write_i64(bytes, offset, key);
    }
    if let Some(row_id) = row_id {
        write_u64(bytes, offset + 8, row_id);
    }
    Ok(())
}

/// Remove one leaf slot from a cached page. The page is not resealed.
///
/// Later slots shift down and stay in their previous order, so a sorted leaf
/// stays sorted. The caller uses this to drop a posting after open.
#[cfg(test)]
pub(crate) fn drop_leaf_entry(page: &mut Page, slot: usize) -> Result<(), DbError> {
    let id = read_u32(page.as_slice(), OFF_ID);
    if read_u16(page.as_slice(), OFF_TYPE) != TYPE_INDEX_LEAF {
        return Err(corrupt(id));
    }
    let count = usize::from(read_u16(page.as_slice(), LEAF_COUNT));
    if slot >= count {
        return Err(corrupt(id));
    }
    let start = LEAF_BODY + slot * ENTRY_LEN;
    let end = LEAF_BODY + count * ENTRY_LEN;
    let bytes = page.as_mut_slice();
    bytes.copy_within(start + ENTRY_LEN..end, start);
    bytes[end - ENTRY_LEN..end].fill(0);
    write_u16(bytes, LEAF_COUNT, (count - 1) as u16);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    #[derive(Clone)]
    struct Mem {
        pages: HashMap<u32, Page>,
        next: u32,
        free: Vec<u32>,
    }

    impl Mem {
        fn new() -> Self {
            Self {
                pages: HashMap::new(),
                next: 1,
                free: Vec::new(),
            }
        }
    }

    impl Nodes for Mem {
        fn read_node(&mut self, id: u32) -> Result<Page, DbError> {
            self.pages.get(&id).cloned().ok_or_else(|| corrupt(id))
        }
        fn write_node(&mut self, id: u32, page: Page) -> Result<(), DbError> {
            self.pages.insert(id, page);
            Ok(())
        }
        fn alloc_node(&mut self) -> Result<u32, DbError> {
            if let Some(id) = self.free.pop() {
                return Ok(id);
            }
            let id = self.next;
            self.next = self.next.checked_add(1).expect("page id");
            Ok(id)
        }
        fn free_node(&mut self, id: u32) -> Result<(), DbError> {
            if self.free.contains(&id) || !self.pages.contains_key(&id) {
                return Err(corrupt(id));
            }
            self.pages.remove(&id);
            self.free.push(id);
            Ok(())
        }
    }

    fn small() -> Limits {
        Limits {
            leaf_max: 4,
            leaf_min: 2,
            internal_max: 5,
            internal_min: 2,
        }
    }

    fn assert_tree(store: &mut Mem, root: u32, model: &BTreeMap<Entry, ()>, limits: Limits) {
        let entries = validate(store, root, limits).expect("tree");
        let expected: Vec<_> = model.keys().copied().collect();
        assert_eq!(entries, expected);
        assert_pages(store, root);
        let equal_key = model.keys().next().map(|entry| entry.0);
        if let Some(key) = equal_key {
            let found = find_equal(store, root, key).unwrap();
            let want: Vec<_> = model
                .keys()
                .filter(|entry| entry.0 == key)
                .map(|entry| entry.1)
                .collect();
            assert_eq!(found, want);
        }
        let desc = iter_desc(store, root).unwrap();
        let mut reversed = expected;
        reversed.reverse();
        assert_eq!(desc, reversed);
    }

    fn assert_pages(store: &mut Mem, root: u32) {
        let mut owned = if root == 0 {
            Vec::new()
        } else {
            owned_pages(store, root).unwrap()
        };
        owned.sort_unstable();
        let mut live: Vec<_> = store.pages.keys().copied().collect();
        live.sort_unstable();
        assert_eq!(live, owned);
        let mut free = store.free.clone();
        let free_len = free.len();
        free.sort_unstable();
        free.dedup();
        assert_eq!(free.len(), free_len);
        assert!(free.iter().all(|id| !store.pages.contains_key(id)));
    }

    fn agree(
        store: &mut Mem,
        root: u32,
        model: &BTreeMap<Entry, ()>,
        low: Option<i64>,
        high: Option<i64>,
    ) {
        let found = find_range(store, root, low, high).unwrap();
        let expected: Vec<_> = model
            .keys()
            .copied()
            .filter(|entry| low.is_none_or(|bound| entry.0 >= bound))
            .filter(|entry| high.is_none_or(|bound| entry.0 <= bound))
            .collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn page_capacity_matches_the_byte_layout() {
        let leaf_region = (PAGE_SIZE - 4) - (HEADER_LEN + 4);
        let internal_region = (PAGE_SIZE - 4) - (HEADER_LEN + 8);
        assert_eq!(leaf_region / ENTRY_LEN, LEAF_MAX);
        assert_eq!(internal_region / SEPARATOR_LEN, INTERNAL_MAX);
        assert_eq!(LEAF_MIN, LEAF_MAX / 2);
        assert_eq!(INTERNAL_MIN, INTERNAL_MAX / 2);
        const _: () = assert!(LEAF_MIN * 2 <= LEAF_MAX);
        const _: () = assert!(INTERNAL_MIN * 2 < INTERNAL_MAX);
    }

    #[test]
    fn empty_and_single_leaf() {
        let mut store = Mem::new();
        let mut root = 0;
        assert!(find_equal(&mut store, root, 1).unwrap().is_empty());
        root = insert(&mut store, root, 7, 1, production()).unwrap();
        assert_eq!(height(&mut store, root).unwrap(), 1);
        assert_eq!(find_equal(&mut store, root, 7).unwrap(), vec![1]);
        root = delete(&mut store, root, 7, 1, production()).unwrap();
        assert_eq!(root, 0);
        assert!(validate(&mut store, root, production()).unwrap().is_empty());
    }

    #[test]
    fn duplicates_use_row_id_order_and_reject_the_same_pair() {
        let mut store = Mem::new();
        let mut root = 0;
        for row in [3, 1, 2] {
            root = insert(&mut store, root, 5, row, small()).unwrap();
        }
        assert_eq!(find_equal(&mut store, root, 5).unwrap(), vec![1, 2, 3]);
        let error = insert(&mut store, root, 5, 2, small()).unwrap_err();
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn ascending_descending_and_random_match_a_reference_model() {
        let mut store = Mem::new();
        let mut root = 0;
        let mut model = BTreeMap::new();
        for key in 0..200 {
            root = insert(&mut store, root, key, 1, small()).unwrap();
            model.insert((key, 1), ());
        }
        assert_tree(&mut store, root, &model, small());
        assert!(height(&mut store, root).unwrap() >= 3);

        for key in (0..200).rev() {
            root = delete(&mut store, root, key, 1, small()).unwrap();
            model.remove(&(key, 1));
            assert_tree(&mut store, root, &model, small());
        }

        let mut state = 0x1234_5678_u64;
        for step in 0..400 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = (state % 80) as i64 - 40;
            let row = (step % 3) as u64 + 1;
            let entry = (key, row);
            match model.entry(entry) {
                std::collections::btree_map::Entry::Vacant(vacant) => {
                    root = insert(&mut store, root, key, row, small()).unwrap();
                    vacant.insert(());
                }
                std::collections::btree_map::Entry::Occupied(occupied) => {
                    root = delete(&mut store, root, key, row, small()).unwrap();
                    occupied.remove();
                }
            }
            assert_tree(&mut store, root, &model, small());
        }
        let range = find_range(&mut store, root, Some(-10), Some(10)).unwrap();
        assert!(range.iter().all(|entry| (-10..=10).contains(&entry.0)));
    }

    #[test]
    fn production_fanout_splits_the_root_and_contracts_it() {
        let mut store = Mem::new();
        let mut root = 0;
        for key in 0..300 {
            root = insert(&mut store, root, key, 1, production()).unwrap();
        }
        assert!(height(&mut store, root).unwrap() >= 2);
        let entries = validate(&mut store, root, production()).unwrap();
        assert_eq!(entries.len(), 300);
        for key in 0..300 {
            root = delete(&mut store, root, key, 1, production()).unwrap();
        }
        assert_eq!(root, 0);
    }

    #[test]
    #[ignore = "release profile of the standalone integer tree"]
    fn profile_standalone_tree() {
        for count in [100_u64, 1_000, 10_000, 100_000] {
            let mut store = Mem::new();
            let mut root = 0;
            let started = std::time::Instant::now();
            for key in 0..count {
                root = insert(&mut store, root, key as i64, 1, production()).unwrap();
            }
            let build = started.elapsed();
            let levels = height(&mut store, root).unwrap();
            let lookup = std::time::Instant::now();
            for key in 0..count {
                let found = find_equal(&mut store, root, key as i64).unwrap();
                assert_eq!(found, vec![1]);
            }
            let equality = lookup.elapsed();
            let range = std::time::Instant::now();
            let rows = find_range(&mut store, root, Some(0), Some((count / 10) as i64)).unwrap();
            let range_time = range.elapsed();
            assert!(!rows.is_empty());
            println!(
                "keys={count} height={levels} build_us={} equality_ns={} range_us={} range_rows={}",
                build.as_micros(),
                equality.as_nanos() / count as u128,
                range_time.as_micros(),
                rows.len()
            );
        }
    }

    #[test]
    fn negative_keys_sort_numerically() {
        let mut store = Mem::new();
        let mut root = 0;
        for key in [3, -2, 0, -10, 8] {
            root = insert(&mut store, root, key, 1, small()).unwrap();
        }
        let keys: Vec<_> = iter_asc(&mut store, root)
            .unwrap()
            .into_iter()
            .map(|entry| entry.0)
            .collect();
        assert_eq!(keys, vec![-10, -2, 0, 3, 8]);
        let descending: Vec<_> = iter_desc(&mut store, root)
            .unwrap()
            .into_iter()
            .map(|entry| entry.0)
            .collect();
        assert_eq!(descending, vec![8, 3, 0, -2, -10]);
    }

    #[test]
    fn recorded_seeds_match_ranges_and_release_every_page() {
        for seed in [1, 42, 0x1234_5678, 0xA5A5_A5A5, 0xDEAD_BEEF, 0x0F0F_F0F0] {
            exercise_seed(seed, 250);
        }
    }

    fn exercise_seed(seed: u64, steps: usize) {
        let mut store = Mem::new();
        let mut root = 0;
        let mut model = BTreeMap::new();
        let mut state = seed;
        for step in 0..steps {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = (state as i64 % 90) - 45;
            let row = ((state >> 17) % 5) + 1;
            let entry = (key, row);
            match model.entry(entry) {
                std::collections::btree_map::Entry::Vacant(vacant) => {
                    root = insert(&mut store, root, key, row, small()).unwrap();
                    vacant.insert(());
                }
                std::collections::btree_map::Entry::Occupied(occupied) => {
                    root = delete(&mut store, root, key, row, small()).unwrap();
                    occupied.remove();
                }
            }
            assert_tree(&mut store, root, &model, small());
            if step % 20 == 0 {
                agree(&mut store, root, &model, Some(key - 3), Some(key + 3));
                agree(&mut store, root, &model, None, Some(0));
                agree(&mut store, root, &model, Some(0), None);
            }
        }
        let leftover: Vec<_> = model.keys().copied().collect();
        for (key, row) in leftover {
            root = delete(&mut store, root, key, row, small()).unwrap();
            model.remove(&(key, row));
            assert_tree(&mut store, root, &model, small());
        }
        assert_eq!(root, 0);
        assert!(store.pages.is_empty());
    }

    #[test]
    fn duplicate_keys_span_a_split_and_stay_ordered() {
        let mut store = Mem::new();
        let mut root = 0;
        let mut model = BTreeMap::new();
        for row in 1..=300 {
            root = insert(&mut store, root, -7, row, production()).unwrap();
            model.insert((-7, row), ());
        }
        root = insert(&mut store, root, -8, 1, production()).unwrap();
        model.insert((-8, 1), ());
        root = insert(&mut store, root, 4, 1, production()).unwrap();
        model.insert((4, 1), ());
        assert_tree(&mut store, root, &model, production());
        assert!(height(&mut store, root).unwrap() >= 2);
        agree(&mut store, root, &model, Some(-7), Some(-7));
        assert_eq!(
            find_equal(&mut store, root, -7).unwrap(),
            (1..=300).collect::<Vec<_>>()
        );
        for row in (1..=300).rev().take(150) {
            root = delete(&mut store, root, -7, row, production()).unwrap();
            model.remove(&(-7, row));
        }
        assert_tree(&mut store, root, &model, production());
    }

    #[test]
    fn a_broken_sibling_does_not_return_part_of_a_duplicate_run() {
        let mut store = Mem::new();
        let mut root = 0;
        for row in 1..=300 {
            root = insert(&mut store, root, 7, row, production()).unwrap();
        }
        root = insert(&mut store, root, 8, 301, production()).unwrap();
        assert_eq!(find_equal(&mut store, root, 7).unwrap().len(), 300);
        let left = leftmost_leaf(&mut store, root).unwrap();
        let mut page = store.read_node(left).unwrap();
        crate::storage::page::set_next_page(&mut page, 0);
        store.write_node(left, page).unwrap();
        let error = find_equal(&mut store, root, 7).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(find_equal(&mut store, root, 8).unwrap(), vec![301]);
    }

    #[test]
    fn production_fanout_reaches_three_levels_without_leaking_pages() {
        let mut store = Mem::new();
        let mut root = 0;
        let count = 52_000i64;
        for key in 0..count {
            root = insert(&mut store, root, key, 1, production()).unwrap();
        }
        assert!(height(&mut store, root).unwrap() >= 3);
        let entries = validate(&mut store, root, production()).unwrap();
        assert_eq!(entries.len(), count as usize);
        assert_eq!(entries.first(), Some(&(0, 1)));
        assert_eq!(entries.last(), Some(&(count - 1, 1)));
        assert!(
            entries
                .windows(2)
                .all(|pair| super::cmp_entry(pair[0], pair[1]) == std::cmp::Ordering::Less)
        );
        assert_pages(&mut store, root);
        let range = find_range(&mut store, root, Some(100), Some(150)).unwrap();
        assert_eq!(range, (100..=150).map(|key| (key, 1)).collect::<Vec<_>>());
        assert_eq!(find_equal(&mut store, root, 10_000).unwrap(), vec![1]);
        for key in 0..count {
            root = delete(&mut store, root, key, 1, production()).unwrap();
        }
        assert_eq!(root, 0);
        assert!(store.pages.is_empty());
        assert_pages(&mut store, 0);
    }

    #[test]
    fn cyclic_and_overdeep_pages_return_errors() {
        let mut store = Mem::new();
        let id = store.alloc_node().unwrap();
        let page = super::encode_internal(
            id,
            &super::Internal {
                keys: vec![(1, 1)],
                children: vec![id, id],
            },
        )
        .unwrap();
        store.write_node(id, page).unwrap();
        let error = owned_pages(&mut store, id).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        let error = validate(&mut store, id, small()).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        let error = insert(&mut store, id, 2, 1, small()).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");

        let mut deep = Mem::new();
        let mut previous = {
            let leaf_id = deep.alloc_node().unwrap();
            let leaf = super::encode_leaf(
                leaf_id,
                &super::Leaf {
                    entries: vec![(1, 1)],
                    left: 0,
                    right: 0,
                },
            )
            .unwrap();
            deep.write_node(leaf_id, leaf).unwrap();
            leaf_id
        };
        for level in 0..70 {
            let side = deep.alloc_node().unwrap();
            let side_page = super::encode_leaf(
                side,
                &super::Leaf {
                    entries: vec![(level as i64 + 2, 1)],
                    left: 0,
                    right: 0,
                },
            )
            .unwrap();
            deep.write_node(side, side_page).unwrap();
            let id = deep.alloc_node().unwrap();
            let page = super::encode_internal(
                id,
                &super::Internal {
                    keys: vec![(1, 1)],
                    children: vec![previous, side],
                },
            )
            .unwrap();
            deep.write_node(id, page).unwrap();
            previous = id;
        }
        let error = owned_pages(&mut deep, previous).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
    }

    fn page_image(store: &Mem) -> Vec<(u32, Vec<u8>)> {
        let mut pages: Vec<_> = store
            .pages
            .iter()
            .map(|(id, page)| (*id, page.as_slice().to_vec()))
            .collect();
        pages.sort_by_key(|(id, _)| *id);
        pages
    }

    fn delete_many_matches_one_at_a_time(limits: Limits, entries: Vec<Entry>, remove: Vec<Entry>) {
        let mut single = Mem::new();
        let mut batched = Mem::new();
        let mut single_root = 0;
        let mut batched_root = 0;
        for entry in &entries {
            single_root = insert(&mut single, single_root, entry.0, entry.1, limits).unwrap();
            batched_root = insert(&mut batched, batched_root, entry.0, entry.1, limits).unwrap();
        }
        assert_eq!(page_image(&single), page_image(&batched));
        for entry in &remove {
            single_root = delete(&mut single, single_root, entry.0, entry.1, limits).unwrap();
        }
        batched_root = delete_many(&mut batched, batched_root, &remove, limits).unwrap();
        let single_entries = validate(&mut single, single_root, limits).unwrap();
        let batched_entries = validate(&mut batched, batched_root, limits).unwrap();
        assert_eq!(single_entries, batched_entries);
        assert_eq!(single_entries, reference_delete(&entries, &remove).unwrap());
        assert_pages(&mut batched, batched_root);
        assert_pages(&mut single, single_root);
    }

    #[test]
    fn batched_leaf_removal_matches_single_deletes() {
        let sequential: Vec<_> = (1..=40).map(|row| (row, row as u64)).collect();
        delete_many_matches_one_at_a_time(small(), sequential.clone(), sequential.clone());
        delete_many_matches_one_at_a_time(
            small(),
            sequential.clone(),
            sequential.iter().copied().step_by(2).collect(),
        );
        delete_many_matches_one_at_a_time(
            small(),
            sequential.clone(),
            vec![(1, 1), (2, 2), (40, 40)],
        );
        let wide: Vec<_> = (1..=800).map(|row| (row, row as u64)).collect();
        delete_many_matches_one_at_a_time(production(), wide.clone(), wide[100..700].to_vec());
        delete_many_matches_one_at_a_time(
            production(),
            wide.clone(),
            wide.iter().copied().step_by(3).collect(),
        );
        let duplicates: Vec<_> = (1..=500).map(|row| (4, row)).collect();
        delete_many_matches_one_at_a_time(
            production(),
            duplicates.clone(),
            duplicates[20..400].to_vec(),
        );
        delete_many_matches_one_at_a_time(production(), wide.clone(), vec![(1, 1)]);
        delete_many_matches_one_at_a_time(production(), wide, Vec::new());
        let mut store = Mem::new();
        let mut root = 0;
        for row in 1..=20 {
            root = insert(&mut store, root, row, row as u64, small()).unwrap();
        }
        let before = page_image(&store);
        let error = delete_many(&mut store, root, &[(9, 99)], small()).unwrap_err();
        assert!(
            error.to_string().contains("index entry is missing"),
            "{error}"
        );
        assert_eq!(page_image(&store), before);
    }

    struct Run {
        id: u32,
        entries: Vec<Entry>,
    }

    fn fill(limits: Limits, entries: &[Entry]) -> (Mem, u32) {
        let mut store = Mem::new();
        let mut root = 0;
        for entry in entries {
            root = insert(&mut store, root, entry.0, entry.1, limits).unwrap();
        }
        (store, root)
    }

    fn leaf_runs(store: &mut Mem, root: u32) -> Vec<Run> {
        if root == 0 {
            return Vec::new();
        }
        let mut id = root;
        for _ in 0..64 {
            let page = store.read_node(id).unwrap();
            if page_type(&page).unwrap() == TYPE_INDEX_LEAF {
                break;
            }
            id = decode_internal(&page).unwrap().children[0];
        }
        let mut runs = Vec::new();
        let mut guard = 0;
        while id != 0 {
            guard += 1;
            assert!(guard < 1_000_000, "leaf chain");
            let leaf = decode_leaf(&store.read_node(id).unwrap()).unwrap();
            let next = leaf.right;
            runs.push(Run {
                id,
                entries: leaf.entries,
            });
            id = next;
        }
        runs
    }

    fn drive(
        store: &mut Mem,
        mut root: u32,
        remove: &[Entry],
        limits: Limits,
    ) -> Result<u32, DbError> {
        for entry in remove {
            root = delete(store, root, entry.0, entry.1, limits)?;
        }
        Ok(root)
    }

    /// Logical posting lists match one-at-a-time deletion, the Milestone 30
    /// grouped delete, and a sorted vector that never calls the tree. A bulk
    /// repack may use different pages. Both success paths must validate, and
    /// the bulk free list must not overlap a live page or name a page twice.
    fn same_tree(limits: Limits, entries: &[Entry], remove: &[Entry]) {
        let reference = reference_delete(entries, remove);
        let (mut single, single_root) = fill(limits, entries);
        let (mut grouped, grouped_root) = fill(limits, entries);
        let (mut batched, batched_root) = fill(limits, entries);
        let single_result = drive(&mut single, single_root, remove, limits);
        let grouped_result = delete_grouped(&mut grouped, grouped_root, remove, limits);
        let batched_result = delete_many(&mut batched, batched_root, remove, limits);
        match (&single_result, &grouped_result, &batched_result, &reference) {
            (Ok(left), Ok(middle), Ok(right), Ok(expect)) => {
                let single_entries = validate(&mut single, *left, limits).unwrap();
                let grouped_entries = validate(&mut grouped, *middle, limits).unwrap();
                let batched_entries = validate(&mut batched, *right, limits).unwrap();
                assert_eq!(single_entries, *expect);
                assert_eq!(grouped_entries, *expect);
                assert_eq!(batched_entries, *expect);
                assert_pages(&mut batched, *right);
                if let Some(absent) = absent_key(expect) {
                    assert!(
                        find_equal(&mut batched, *right, absent).unwrap().is_empty(),
                        "{absent}"
                    );
                }
                if let Some(key) = expect.first().map(|entry| entry.0) {
                    let found = find_equal(&mut batched, *right, key).unwrap();
                    let want: Vec<_> = expect
                        .iter()
                        .filter(|entry| entry.0 == key)
                        .map(|entry| entry.1)
                        .collect();
                    assert_eq!(found, want);
                }
            }
            (Err(left), Err(middle), Err(right), Err(expect)) => {
                assert_eq!(left.to_string(), middle.to_string());
                assert_eq!(left.to_string(), right.to_string());
                assert!(right.to_string().contains(expect), "{right}");
            }
            _ => panic!(
                "outcomes diverged single={single_result:?} grouped={grouped_result:?} bulk={batched_result:?} reference={reference:?}"
            ),
        }
    }

    /// Page bytes match one-at-a-time deletion. Used when the run cannot
    /// underflow, so bulk repack does not run.
    fn same_pages(limits: Limits, entries: &[Entry], remove: &[Entry]) {
        let (mut single, single_root) = fill(limits, entries);
        let (mut batched, batched_root) = fill(limits, entries);
        let left = drive(&mut single, single_root, remove, limits).unwrap();
        let right = delete_many(&mut batched, batched_root, remove, limits).unwrap();
        assert_eq!(left, right);
        assert_eq!(page_image(&single), page_image(&batched));
        assert_eq!(single.free, batched.free);
    }

    fn reference_delete(entries: &[Entry], remove: &[Entry]) -> Result<Vec<Entry>, &'static str> {
        let mut list = entries.to_vec();
        list.sort_by(|left, right| cmp_entry(*left, *right));
        for entry in remove {
            match list.binary_search_by(|item| cmp_entry(*item, *entry)) {
                Ok(index) => {
                    list.remove(index);
                }
                Err(_) => return Err("index entry is missing"),
            }
        }
        Ok(list)
    }

    fn absent_key(entries: &[Entry]) -> Option<i64> {
        let mut keys: Vec<i64> = entries.iter().map(|entry| entry.0).collect();
        keys.sort_unstable();
        keys.dedup();
        let mut candidate = i64::MIN;
        for key in keys {
            if candidate != key {
                return Some(candidate);
            }
            if candidate == i64::MAX {
                return None;
            }
            candidate += 1;
        }
        Some(candidate)
    }

    fn sequential(count: i64) -> Vec<Entry> {
        (1..=count).map(|key| (key, key as u64)).collect()
    }

    #[test]
    fn grouped_removal_uses_the_loaded_leaf_occupancy() {
        let limits = small();
        let entries = sequential(24);
        let (mut probe, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut probe, root);
        assert!(runs.len() > 2, "leaves {}", runs.len());
        for run in &runs {
            assert!(
                (limits.leaf_min..=limits.leaf_max).contains(&run.entries.len()),
                "leaf {} len {}",
                run.id,
                run.entries.len()
            );
        }
        let fat = runs
            .iter()
            .find(|run| run.entries.len() > limits.leaf_min)
            .expect("leaf above the minimum");
        let keep = limits.leaf_min;
        let room = fat.entries.len() - keep;
        same_tree(limits, &entries, &fat.entries[..1]);
        same_pages(limits, &entries, &fat.entries[..1]);
        same_tree(limits, &entries, &fat.entries[..room]);
        if room >= 2 {
            same_pages(limits, &entries, &fat.entries[..room]);
        }
        same_tree(limits, &entries, &fat.entries[..room + 1]);
        if room >= 2 {
            same_tree(limits, &entries, &fat.entries[..room]);
        }
        same_tree(limits, &entries, &[fat.entries[0]]);
        same_tree(limits, &entries, &[*fat.entries.last().unwrap()]);
        same_tree(limits, &entries, &fat.entries);
        same_tree(limits, &entries, &entries);
        same_tree(limits, &[], &[(1, 1)]);
        same_tree(limits, &entries[..1], &[]);
        same_tree(limits, &entries[..1], &entries[..1]);
        same_tree(limits, &entries[..3], &entries[..2]);
        same_tree(limits, &entries[..3], &entries[..3]);

        let _guard = crate::storage::profile::enable();
        let (mut store, root) = fill(limits, &entries);
        delete_many(&mut store, root, &fat.entries[..room + 1], limits).unwrap();
        let profile = crate::storage::profile::take();
        if room >= 2 {
            assert_eq!(profile.leaf_batches, u64::try_from(room).unwrap());
            assert_eq!(profile.index_singles, 1);
            assert_eq!(profile.bulk_spans, 0);
        } else {
            assert_eq!(profile.leaf_batches, 0);
            assert!(profile.bulk_spans > 0 || profile.index_singles > 0);
        }
        let (mut after, root) = fill(limits, &entries);
        let root = delete_many(&mut after, root, &fat.entries[..room], limits).unwrap();
        let again = leaf_runs(&mut after, root);
        let stayed = again.iter().find(|run| {
            !fat.entries[..room]
                .iter()
                .any(|entry| run.entries.contains(entry))
                && run.entries.len() == keep
        });
        assert!(
            stayed.is_some(),
            "a leaf should sit on the minimum after removing its spare postings"
        );
    }

    #[test]
    fn borrow_merge_and_root_changes_match_single_deletes() {
        let limits = small();
        let entries = sequential(24);
        let (mut probe, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut probe, root);
        assert!(runs.len() >= 3, "leaves {}", runs.len());
        for run in &runs {
            same_tree(limits, &entries, &[run.entries[0]]);
            same_tree(limits, &entries, &[*run.entries.last().unwrap()]);
            same_tree(limits, &entries, &run.entries);
        }
        let mut adjacent = runs[0].entries.clone();
        adjacent.extend(runs[1].entries.iter().copied());
        same_tree(limits, &entries, &adjacent);
        let fat = runs
            .iter()
            .find(|run| run.entries.len() >= limits.leaf_min + 2)
            .expect("leaf with a grouped prefix");
        let room = fat.entries.len() - limits.leaf_min;
        let mut grouped_then_fallback = fat.entries[..room].to_vec();
        grouped_then_fallback.push(fat.entries[room]);
        same_tree(limits, &entries, &grouped_then_fallback);
        let tight = runs
            .iter()
            .find(|run| run.entries.len() == limits.leaf_min)
            .expect("leaf at the minimum");
        let mut fallback_then_grouped = vec![tight.entries[0]];
        fallback_then_grouped.extend(fat.entries[..room].iter().copied());
        same_tree(limits, &entries, &fallback_then_grouped);
        let separator = runs[1].entries[0];
        same_tree(limits, &entries, &[separator]);
        same_tree(
            limits,
            &entries,
            &runs[1].entries[..2.min(runs[1].entries.len())],
        );
        same_tree(
            limits,
            &entries,
            &[
                runs[0].entries[0],
                *runs.last().unwrap().entries.last().unwrap(),
            ],
        );
        same_tree(limits, &entries, &entries[..entries.len() - 1]);
        let (mut store, root) = fill(limits, &entries);
        let root = delete_many(&mut store, root, &entries, limits).unwrap();
        assert_eq!(root, 0);
        assert_eq!(validate(&mut store, root, limits).unwrap(), Vec::new());

        let limits = production();
        let wide = sequential(600);
        let (mut probe, root) = fill(limits, &wide);
        let runs = leaf_runs(&mut probe, root);
        assert!(runs.len() > 1, "production leaves {}", runs.len());
        assert!(height(&mut probe, root).unwrap() >= 2);
        let fat = runs.iter().max_by_key(|run| run.entries.len()).unwrap();
        assert!(fat.entries.len() > limits.leaf_min);
        let room = fat.entries.len() - limits.leaf_min;
        same_tree(limits, &wide, &fat.entries[..1]);
        same_pages(limits, &wide, &fat.entries[..1]);
        same_tree(limits, &wide, &fat.entries[..room]);
        if room >= 2 {
            same_pages(limits, &wide, &fat.entries[..room]);
            same_pages(limits, &wide, &fat.entries[..room + 1]);
        }
        same_tree(limits, &wide, &fat.entries[..room + 1]);
        same_tree(limits, &wide, &[fat.entries[0]]);
        same_tree(limits, &wide, &[*fat.entries.last().unwrap()]);
        same_tree(limits, &wide, &runs[0].entries[..1]);
        same_tree(limits, &wide, &runs[0].entries);
        same_tree(
            limits,
            &wide,
            &[
                runs[0].entries[0],
                *runs.last().unwrap().entries.last().unwrap(),
            ],
        );
    }

    #[test]
    fn duplicate_extreme_and_repeated_postings_match_single_deletes() {
        let limits = small();
        let duplicates: Vec<Entry> = (1..=30).map(|row| (7, row)).collect();
        same_tree(limits, &duplicates, &duplicates[2..20]);
        same_tree(limits, &duplicates, &duplicates);
        let alternating: Vec<Entry> = (1_i64..=20).map(|row| (row % 2, row as u64)).collect();
        let every_other: Vec<Entry> = alternating.iter().copied().step_by(2).collect();
        same_tree(limits, &alternating, &every_other);
        let extremes = vec![
            (i64::MIN, 1),
            (i64::MIN, 2),
            (-4, 3),
            (0, 4),
            (0, 5),
            (9, 6),
            (i64::MAX, 7),
            (i64::MAX, 8),
        ];
        same_tree(limits, &extremes, &extremes[1..6]);
        same_tree(limits, &extremes, &[extremes[0], *extremes.last().unwrap()]);
        same_tree(limits, &duplicates, &[duplicates[3], duplicates[3]]);
        same_tree(
            limits,
            &duplicates,
            &[duplicates[1], (7, 99), duplicates[4]],
        );
        let (mut store, root) = fill(limits, &duplicates);
        let before = page_image(&store);
        let error =
            delete_many(&mut store, root, &[duplicates[0], duplicates[0]], limits).unwrap_err();
        assert!(
            error.to_string().contains("index entry is missing"),
            "{error}"
        );
        let (mut single, mut single_root) = fill(limits, &duplicates);
        single_root = delete(
            &mut single,
            single_root,
            duplicates[0].0,
            duplicates[0].1,
            limits,
        )
        .unwrap();
        let second = delete(
            &mut single,
            single_root,
            duplicates[0].0,
            duplicates[0].1,
            limits,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), second.to_string());
        assert_eq!(page_image(&store), page_image(&single));
        assert_ne!(page_image(&store), before);
    }

    #[test]
    fn a_late_tree_error_matches_single_deletes_and_is_not_repaired() {
        let limits = small();
        let entries = sequential(20);
        let (mut probe, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut probe, root);
        assert!(runs.len() >= 2);
        assert!(runs.len() >= 3, "leaves {}", runs.len());
        let fat = runs.last().unwrap();
        assert!(
            fat.entries.len() >= limits.leaf_min + 2,
            "rightmost leaf len {}",
            fat.entries.len()
        );
        let room = fat.entries.len() - limits.leaf_min;
        let mut remove = fat.entries[..room].to_vec();
        let bad = runs[1].id;
        remove.push(runs[1].entries[0]);
        let (mut single, single_root) = fill(limits, &entries);
        let (mut batched, batched_root) = fill(limits, &entries);
        for store in [&mut single, &mut batched] {
            let mut page = store.read_node(bad).unwrap();
            page.as_mut_slice()[0] = 0;
            page.as_mut_slice()[1] = 0;
            store.write_node(bad, page).unwrap();
        }
        let left = drive(&mut single, single_root, &remove, limits).unwrap_err();
        let right = delete_many(&mut batched, batched_root, &remove, limits).unwrap_err();
        assert_eq!(left.to_string(), right.to_string());
        assert!(right.to_string().contains("corrupt"), "{right}");
        assert_eq!(page_image(&single), page_image(&batched));
        assert_eq!(batched.read_node(bad).unwrap().as_slice()[0], 0);

        assert_eq!(runs[0].entries.len(), limits.leaf_min);
        let (mut single, mut single_root) = fill(limits, &entries);
        let (mut batched, mut batched_root) = fill(limits, &entries);
        let spare = runs[1].entries.len() - limits.leaf_min;
        let trim = &runs[1].entries[runs[1].entries.len() - spare..];
        single_root = drive(&mut single, single_root, trim, limits).unwrap();
        batched_root = delete_many(&mut batched, batched_root, trim, limits).unwrap();
        let right_id = runs[1].id;
        for store in [&mut single, &mut batched] {
            let mut page = store.read_node(right_id).unwrap();
            crate::storage::page::set_next_page(&mut page, 999);
            store.write_node(right_id, page).unwrap();
        }
        let victim = runs[0].entries[0];
        let left = delete(&mut single, single_root, victim.0, victim.1, limits).unwrap_err();
        let right = delete_many(&mut batched, batched_root, &[victim], limits).unwrap_err();
        assert_eq!(left.to_string(), right.to_string());
        assert!(right.to_string().contains("corrupt"), "{right}");
        assert_eq!(page_image(&single), page_image(&batched));
        assert_eq!(
            crate::storage::page::next_page_of(&batched.read_node(right_id).unwrap()),
            999
        );

        let (mut store, root) = fill(limits, &entries);
        let mut node = decode_internal(&store.read_node(root).unwrap()).unwrap();
        node.keys[0] = (i64::MIN, 0);
        store
            .write_node(root, encode_internal(root, &node).unwrap())
            .unwrap();
        let before = page_image(&store);
        let error = delete_many(&mut store, root, &[runs[0].entries[0]], limits).unwrap_err();
        assert!(
            error.to_string().contains("index entry is missing"),
            "{error}"
        );
        assert_eq!(page_image(&store), before);

        let (mut store, root) = fill(limits, &entries);
        let mut node = decode_internal(&store.read_node(root).unwrap()).unwrap();
        let routed = *runs.last().unwrap().entries.last().unwrap();
        let last = node.children.len() - 1;
        node.children[last] = 999;
        store
            .write_node(root, encode_internal(root, &node).unwrap())
            .unwrap();
        let before = page_image(&store);
        let error = delete_many(&mut store, root, &[routed], limits).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(page_image(&store), before);

        let donor = runs
            .iter()
            .rev()
            .find(|run| run.id != runs[0].id && run.entries.len() >= limits.leaf_min + 2)
            .expect("right-hand leaf with a grouped prefix");
        let (mut single, single_root) = fill(limits, &entries);
        let (mut batched, batched_root) = fill(limits, &entries);
        for store in [&mut single, &mut batched] {
            let mut page = store.read_node(runs[0].id).unwrap();
            page.as_mut_slice()[0] = 0;
            page.as_mut_slice()[1] = 0;
            store.write_node(runs[0].id, page).unwrap();
        }
        let prefix = &donor.entries[..2];
        let left = drive(&mut single, single_root, prefix, limits).unwrap_err();
        let right = delete_many(&mut batched, batched_root, prefix, limits).unwrap_err();
        assert!(left.to_string().contains("corrupt"), "{left}");
        assert!(right.to_string().contains("corrupt"), "{right}");
        assert_eq!(batched.read_node(runs[0].id).unwrap().as_slice()[0], 0);
        let donor_leaf = decode_leaf(&batched.read_node(donor.id).unwrap()).unwrap();
        assert_eq!(donor_leaf.entries, donor.entries);
    }

    #[test]
    fn bulk_rebalance_preserves_postings_occupancy_and_links() {
        let limits = small();
        let entries = sequential(40);
        let (mut probe, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut probe, root);
        assert!(runs.len() >= 4, "leaves {}", runs.len());
        assert!(height(&mut probe, root).unwrap() >= 2);
        let tight: Vec<Entry> = runs
            .iter()
            .filter(|run| run.entries.len() == limits.leaf_min)
            .take(3)
            .flat_map(|run| run.entries.iter().copied())
            .collect();
        assert!(tight.len() >= limits.leaf_min * 2, "tight {}", tight.len());
        same_tree(limits, &entries, &tight[..1]);
        same_tree(limits, &entries, &tight[..2]);
        same_tree(limits, &entries, &tight);
        same_tree(limits, &entries, &runs[0].entries);
        let mut consecutive = runs[0].entries.clone();
        consecutive.extend(runs[1].entries.iter().copied());
        consecutive.extend(runs[2].entries.iter().copied());
        same_tree(limits, &entries, &consecutive);
        same_tree(limits, &entries, &entries[..entries.len() - 1]);
        same_tree(limits, &entries, &entries);
        let duplicates: Vec<Entry> = (1..=24).map(|row| (3, row)).collect();
        same_tree(limits, &duplicates, &duplicates[4..18]);
        same_tree(limits, &duplicates, &duplicates);
        let extremes = vec![
            (i64::MIN, 1),
            (i64::MIN, u64::MAX),
            (-1, 2),
            (0, 3),
            (0, 4),
            (1, 5),
            (i64::MAX, 6),
            (i64::MAX, 7),
        ];
        same_tree(limits, &extremes, &extremes[1..7]);
        same_tree(limits, &extremes, &[extremes[0], extremes[3], extremes[7]]);

        let _guard = crate::storage::profile::enable();
        let (mut store, root) = fill(limits, &entries);
        let before = height(&mut store, root).unwrap();
        let root = delete_many(&mut store, root, &tight, limits).unwrap();
        let profile = crate::storage::profile::take();
        assert!(profile.bulk_spans > 0, "{profile:?}");
        assert!(profile.descents > 0, "{profile:?}");
        assert_eq!(profile.index_singles, 0, "{profile:?}");
        let got = validate(&mut store, root, limits).unwrap();
        let expect = reference_delete(&entries, &tight).unwrap();
        assert_eq!(got, expect);
        assert_pages(&mut store, root);
        let after = height(&mut store, root).unwrap();
        assert!(after <= before, "height {before} -> {after}");
        let runs = leaf_runs(&mut store, root);
        for run in &runs {
            let root_leaf = runs.len() == 1;
            if root_leaf {
                assert!(!run.entries.is_empty());
            } else {
                assert!(
                    (limits.leaf_min..=limits.leaf_max).contains(&run.entries.len()),
                    "leaf {} len {}",
                    run.id,
                    run.entries.len()
                );
            }
        }

        let (mut store, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut store, root);
        let tight_run = runs
            .iter()
            .find(|run| run.entries.len() == limits.leaf_min)
            .expect("minimum leaf");
        let distant = runs
            .iter()
            .rev()
            .find(|run| run.id != tight_run.id)
            .expect("another leaf")
            .id;
        let mut page = store.read_node(distant).unwrap();
        page.as_mut_slice()[0] = 0;
        page.as_mut_slice()[1] = 0;
        store.write_node(distant, page).unwrap();
        let before = page_image(&store);
        let remove = tight_run.entries.clone();
        let error = delete_many(&mut store, root, &remove, limits).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(page_image(&store), before);
        let (mut one, one_root) = fill(limits, &entries);
        let one_runs = leaf_runs(&mut one, one_root);
        let mut page = one.read_node(one_runs.last().unwrap().id).unwrap();
        page.as_mut_slice()[0] = 0;
        page.as_mut_slice()[1] = 0;
        one.write_node(one_runs.last().unwrap().id, page).unwrap();
        let victim = one_runs[0].entries[0];
        let error = delete_many(&mut one, one_root, &[victim], limits);
        assert!(error.is_ok() || error.unwrap_err().to_string().contains("corrupt"));

        let mut missing = tight[..2].to_vec();
        missing.push((99, 99));
        same_tree(limits, &entries, &missing);
    }

    fn kept_window(entries: &[Entry], keep: usize) -> Vec<Entry> {
        let start = (entries.len() - keep) / 2;
        entries[start..start + keep].to_vec()
    }

    fn without(entries: &[Entry], keep: &[Entry]) -> Vec<Entry> {
        let kept: HashSet<Entry> = keep.iter().copied().collect();
        entries
            .iter()
            .copied()
            .filter(|entry| !kept.contains(entry))
            .collect()
    }

    /// Delete a key-ordered set that starts in a minimum leaf and reaches both
    /// ends, so the span is the whole tree.
    fn rebuild_keep(limits: Limits, total: i64, keep: usize) -> (Mem, u32, Vec<Entry>) {
        let entries = sequential(total);
        assert!(keep < entries.len());
        let kept = if keep == 0 {
            Vec::new()
        } else {
            kept_window(&entries, keep)
        };
        let remove = without(&entries, &kept);
        assert_eq!(remove.first().map(|entry| entry.0), Some(1));
        assert_eq!(remove.last().map(|entry| entry.0), Some(total));
        let (mut store, root) = fill(limits, &entries);
        let root = delete_many(&mut store, root, &remove, limits).unwrap();
        (store, root, kept)
    }

    fn assert_built(store: &mut Mem, root: u32, limits: Limits, expect: &[Entry]) {
        if root == 0 {
            assert!(expect.is_empty());
            assert!(store.pages.is_empty());
            assert_pages(store, root);
            assert_eq!(height(store, root).unwrap(), 0);
            return;
        }
        let got = validate(store, root, limits).unwrap();
        assert_eq!(got, expect);
        assert_pages(store, root);
        let runs = leaf_runs(store, root);
        assert!(!runs.is_empty());
        let mut previous = 0;
        let mut flat = Vec::new();
        let mut leaf_ids = HashSet::new();
        for (index, run) in runs.iter().enumerate() {
            assert!(leaf_ids.insert(run.id), "leaf {} twice", run.id);
            let page = store.read_node(run.id).unwrap();
            assert_eq!(read_u32(page.as_slice(), OFF_ID), run.id);
            assert_eq!(page_type(&page).unwrap(), TYPE_INDEX_LEAF);
            let leaf = decode_leaf(&page).unwrap();
            assert_eq!(leaf.left, previous, "left sibling of {}", run.id);
            let right = if index + 1 == runs.len() {
                0
            } else {
                runs[index + 1].id
            };
            assert_eq!(leaf.right, right, "right sibling of {}", run.id);
            assert!(
                leaf.entries.len() <= limits.leaf_max,
                "leaf {} len {}",
                run.id,
                leaf.entries.len()
            );
            if runs.len() == 1 {
                assert!(!leaf.entries.is_empty());
                assert_eq!(root, run.id);
            } else {
                assert!(
                    leaf.entries.len() >= limits.leaf_min,
                    "leaf {} len {}",
                    run.id,
                    leaf.entries.len()
                );
            }
            flat.extend(leaf.entries);
            previous = run.id;
        }
        assert_eq!(flat, expect);
        let mut seen = HashSet::new();
        let mut child_refs = HashSet::new();
        let depth = walk_internals(store, root, true, limits, &mut seen, &mut child_refs);
        assert_eq!(height(store, root).unwrap(), depth);
        for run in &runs {
            assert!(seen.contains(&run.id), "leaf {} is unreachable", run.id);
        }
        assert!(store.free.iter().all(|id| !seen.contains(id)));
        assert!(store.free.iter().all(|id| *id != root));
    }

    fn walk_internals(
        store: &mut Mem,
        id: u32,
        is_root: bool,
        limits: Limits,
        seen: &mut HashSet<u32>,
        child_refs: &mut HashSet<u32>,
    ) -> u32 {
        assert!(seen.insert(id), "page {id} is reachable twice");
        let page = store.read_node(id).unwrap();
        assert_eq!(read_u32(page.as_slice(), OFF_ID), id);
        if page_type(&page).unwrap() == TYPE_INDEX_LEAF {
            return 1;
        }
        let node = decode_internal(&page).unwrap();
        assert!(!node.keys.is_empty(), "internal {id} has no separator");
        assert_eq!(node.children.len(), node.keys.len() + 1);
        assert!(node.keys.len() <= limits.internal_max);
        if is_root {
            assert!(
                node.children.len() >= 2,
                "root {id} has {} children",
                node.children.len()
            );
        } else {
            assert!(
                node.keys.len() >= limits.internal_min,
                "internal {id} has {} separators",
                node.keys.len()
            );
        }
        for pair in node.keys.windows(2) {
            assert!(cmp_entry(pair[0], pair[1]) == std::cmp::Ordering::Less);
        }
        let mut depth = 0;
        for (index, child) in node.children.iter().copied().enumerate() {
            assert!(
                child_refs.insert(child),
                "child {child} is referenced twice"
            );
            assert_ne!(child, id);
            assert!(store.pages.contains_key(&child), "child {child} is missing");
            let child_depth = walk_internals(store, child, false, limits, seen, child_refs);
            if index == 0 {
                depth = child_depth;
            } else {
                assert_eq!(child_depth, depth);
                let minimum = leaf_minimum(store, child);
                assert_eq!(node.keys[index - 1], minimum, "separator of child {child}");
            }
        }
        depth + 1
    }

    fn leaf_minimum(store: &mut Mem, mut id: u32) -> Entry {
        for _ in 0..64 {
            let page = store.read_node(id).unwrap();
            if page_type(&page).unwrap() == TYPE_INDEX_LEAF {
                return decode_leaf(&page).unwrap().entries[0];
            }
            id = decode_internal(&page).unwrap().children[0];
        }
        panic!("child chain");
    }

    fn lengths_of(store: &mut Mem, root: u32) -> Vec<usize> {
        leaf_runs(store, root)
            .iter()
            .map(|run| run.entries.len())
            .collect()
    }

    #[test]
    fn bulk_leaf_occupancy_hits_every_boundary() {
        let limits = small();
        let entries = sequential(40);
        let (mut probe, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut probe, root);
        assert!(runs.len() >= 8, "leaves {}", runs.len());
        assert!(runs.iter().any(|run| run.entries.len() == limits.leaf_min));
        let one = runs
            .iter()
            .find(|run| run.entries.len() == limits.leaf_min && run.id != runs[0].id)
            .expect("middle minimum leaf");
        let one_pos = runs.iter().position(|run| run.id == one.id).unwrap();
        assert!(one_pos + 1 < runs.len());
        same_tree(limits, &entries, &one.entries);
        let (mut store, root) = fill(limits, &entries);
        let root = delete_many(&mut store, root, &one.entries, limits).unwrap();
        assert_built(
            &mut store,
            root,
            limits,
            &reference_delete(&entries, &one.entries).unwrap(),
        );

        let two = {
            let mut keys = runs[one_pos].entries.clone();
            keys.extend(runs[one_pos + 1].entries.iter().copied());
            keys
        };
        same_tree(limits, &entries, &two);
        let (mut store, root) = fill(limits, &entries);
        let root = delete_many(&mut store, root, &two, limits).unwrap();
        assert_built(
            &mut store,
            root,
            limits,
            &reference_delete(&entries, &two).unwrap(),
        );

        let three = {
            let start = runs
                .windows(3)
                .position(|group| group.iter().all(|run| run.entries.len() == limits.leaf_min))
                .expect("three minimum leaves");
            let mut keys = Vec::new();
            for run in &runs[start..start + 3] {
                keys.extend(run.entries.iter().copied());
            }
            keys
        };
        let (mut store, root) = fill(limits, &entries);
        let root = delete_many(&mut store, root, &three, limits).unwrap();
        assert_built(
            &mut store,
            root,
            limits,
            &reference_delete(&entries, &three).unwrap(),
        );
        assert!(
            lengths_of(&mut store, root)
                .iter()
                .all(|len| { *len >= limits.leaf_min && *len <= limits.leaf_max })
        );

        assert_eq!(runs[0].entries.len(), limits.leaf_min);
        let (mut store, root) = fill(limits, &entries);
        let root = delete_many(&mut store, root, &runs[0].entries, limits).unwrap();
        assert_built(
            &mut store,
            root,
            limits,
            &reference_delete(&entries, &runs[0].entries).unwrap(),
        );

        let mut model = entries.clone();
        let (mut store, mut root) = fill(limits, &entries);
        loop {
            let last = leaf_runs(&mut store, root).last().unwrap().entries.clone();
            if last.len() == limits.leaf_min {
                root = delete_many(&mut store, root, &last, limits).unwrap();
                for entry in &last {
                    model.retain(|kept| kept != entry);
                }
                assert_built(&mut store, root, limits, &model);
                break;
            }
            let victim = *last.last().unwrap();
            root = delete(&mut store, root, victim.0, victim.1, limits).unwrap();
            model.retain(|kept| *kept != victim);
        }

        let (mut store, root, _) = rebuild_keep(limits, 40, 9);
        assert_eq!(lengths_of(&mut store, root), vec![3, 3, 3]);
        assert_built(&mut store, root, limits, &kept_window(&sequential(40), 9));

        let cases = [
            (1_usize, vec![1_usize]),
            (2, vec![2]),
            (3, vec![3]),
            (4, vec![4]),
            (5, vec![3, 2]),
            (6, vec![3, 3]),
            (7, vec![4, 3]),
            (8, vec![4, 4]),
            (10, vec![4, 3, 3]),
        ];
        for (keep, lengths) in cases {
            let (mut store, root, kept) = rebuild_keep(limits, 40, keep);
            assert_eq!(lengths_of(&mut store, root), lengths, "keep {keep}");
            assert_built(&mut store, root, limits, &kept);
            if lengths.len() == 1 {
                assert_eq!(height(&mut store, root).unwrap(), 1);
                assert_eq!(root, leaf_runs(&mut store, root)[0].id);
            }
        }
        let (mut store, root, _) = rebuild_keep(limits, 40, 0);
        assert_built(&mut store, root, limits, &[]);

        let limits = production();
        let production_cases = [
            (126_usize, vec![126_usize]),
            (127, vec![127]),
            (128, vec![128]),
            (253, vec![253]),
            (254, vec![254]),
            (255, vec![128, 127]),
            (381, vec![191, 190]),
            (634, vec![212, 211, 211]),
        ];
        for (keep, lengths) in production_cases {
            let (mut store, root, kept) = rebuild_keep(limits, 700, keep);
            assert_eq!(lengths_of(&mut store, root), lengths, "keep {keep}");
            assert_built(&mut store, root, limits, &kept);
            let root_leaf = lengths.len() == 1;
            for len in &lengths {
                assert!(*len <= limits.leaf_max);
                if root_leaf {
                    assert!(*len >= 1);
                } else {
                    assert!(*len >= limits.leaf_min, "{len}");
                }
            }
        }
    }

    #[test]
    fn bulk_internal_rebuild_covers_fanout_and_height() {
        let limits = small();
        let (mut store, root, kept) = rebuild_keep(limits, 48, 5);
        assert_eq!(lengths_of(&mut store, root), vec![3, 2]);
        assert_eq!(height(&mut store, root).unwrap(), 2);
        let node = decode_internal(&store.read_node(root).unwrap()).unwrap();
        assert_eq!(node.children.len(), 2);
        assert_eq!(node.keys.len(), 1);
        assert!(node.keys.len() < limits.internal_min);
        assert_built(&mut store, root, limits, &kept);

        let (mut store, root, kept) = rebuild_keep(limits, 48, 24);
        assert_eq!(lengths_of(&mut store, root), vec![4, 4, 4, 4, 4, 4]);
        assert_eq!(height(&mut store, root).unwrap(), 2);
        let node = decode_internal(&store.read_node(root).unwrap()).unwrap();
        assert_eq!(node.children.len(), 6);
        assert_eq!(node.keys.len(), limits.internal_max);
        assert_built(&mut store, root, limits, &kept);

        let (mut store, root, kept) = rebuild_keep(limits, 48, 25);
        assert_eq!(lengths_of(&mut store, root), vec![4, 4, 4, 4, 3, 3, 3]);
        assert_eq!(height(&mut store, root).unwrap(), 3);
        let root_node = decode_internal(&store.read_node(root).unwrap()).unwrap();
        assert_eq!(root_node.children.len(), 2);
        let mut child_fanout = Vec::new();
        for child in &root_node.children {
            let node = decode_internal(&store.read_node(*child).unwrap()).unwrap();
            child_fanout.push(node.children.len());
            assert!(node.keys.len() >= limits.internal_min);
        }
        assert_eq!(child_fanout, vec![4, 3]);
        assert_built(&mut store, root, limits, &kept);
        let old_internal: HashSet<u32> = internal_ids(&mut store, root)
            .unwrap()
            .into_iter()
            .collect();

        let runs = leaf_runs(&mut store, root);
        assert_eq!(runs.last().unwrap().entries.len(), 3);
        let mut remove = runs.last().unwrap().entries[..2].to_vec();
        remove.push(runs[0].entries[0]);
        let _guard = crate::storage::profile::enable();
        let root = delete_many(&mut store, root, &remove, limits).unwrap();
        let profile = crate::storage::profile::take();
        assert!(profile.root_contractions >= 1, "{profile:?}");
        assert!(profile.bulk_spans >= 1, "{profile:?}");
        let expect = reference_delete(&kept, &remove).unwrap();
        assert_eq!(height(&mut store, root).unwrap(), 2);
        assert_eq!(internal_ids(&mut store, root).unwrap().len(), 1);
        assert_built(&mut store, root, limits, &expect);
        assert!(old_internal.iter().all(|id| !store.pages.contains_key(id)));

        let runs = leaf_runs(&mut store, root);
        let short = runs
            .iter()
            .find(|run| run.entries.len() < limits.leaf_min + 2)
            .expect("tight leaf");
        let mut rest = short.entries.clone();
        for run in &runs {
            for entry in &run.entries {
                if !rest.contains(entry) {
                    rest.push(*entry);
                }
            }
        }
        let root = delete_many(&mut store, root, &rest, limits).unwrap();
        assert_built(&mut store, root, limits, &[]);
        assert!(!store.free.is_empty());

        let (mut store, root, kept) = rebuild_keep(limits, 48, 4);
        assert_eq!(lengths_of(&mut store, root), vec![4]);
        assert_eq!(height(&mut store, root).unwrap(), 1);
        assert_built(&mut store, root, limits, &kept);

        let (mut wide, wide_root) = fill(production(), &sequential(52_000));
        let tall = height(&mut wide, wide_root).unwrap();
        assert!(tall >= 3, "height {tall}");
        let first = leaf_runs(&mut wide, wide_root)[0].entries.clone();
        assert_eq!(first.len(), production().leaf_min);
        let root = delete_many(&mut wide, wide_root, &first, production()).unwrap();
        let expect = reference_delete(&sequential(52_000), &first).unwrap();
        assert_built(&mut wide, root, production(), &expect);
        assert!(height(&mut wide, root).unwrap() >= 3);
        let internals = internal_ids(&mut wide, root).unwrap();
        assert!(internals.len() >= 2, "internals {}", internals.len());
        for id in &internals {
            if *id == root {
                continue;
            }
            let node = decode_internal(&wide.read_node(*id).unwrap()).unwrap();
            assert!(node.keys.len() >= production().internal_min);
            assert!(node.keys.len() <= production().internal_max);
        }
        let before_pages = wide.pages.len();
        let remove: Vec<Entry> = (expect[0].0..=40_000)
            .map(|key| (key, key as u64))
            .collect();
        let root = delete_many(&mut wide, root, &remove, production()).unwrap();
        let expect = reference_delete(&expect, &remove).unwrap();
        assert_built(&mut wide, root, production(), &expect);
        assert!(height(&mut wide, root).unwrap() < tall);
        assert!(wide.pages.len() < before_pages);
        let remaining = expect.clone();
        let root = delete_many(&mut wide, root, &remaining, production()).unwrap();
        assert_built(&mut wide, root, production(), &[]);
        let freed = wide.free.clone();
        let next = wide.next;
        let mut root = 0;
        for key in 1..=40 {
            root = insert(&mut wide, root, key, key as u64, production()).unwrap();
        }
        assert_eq!(wide.next, next);
        assert!(freed.iter().any(|id| wide.pages.contains_key(id)));
        assert_built(
            &mut wide,
            root,
            production(),
            &(1..=40).map(|key| (key, key as u64)).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn bulk_reuses_pages_and_keeps_the_free_list_disjoint() {
        let limits = small();
        let entries = sequential(36);
        let (mut store, mut root) = fill(limits, &entries);
        let mut model = entries.clone();
        let mut high = store.next;
        for cycle in 0..5 {
            let runs = leaf_runs(&mut store, root);
            let mut remove = runs[0].entries.clone();
            remove.extend(runs[1].entries.iter().copied());
            let old_internal = internal_ids(&mut store, root).unwrap();
            root = delete_many(&mut store, root, &remove, limits).unwrap();
            for entry in &remove {
                model.retain(|kept| kept != entry);
            }
            assert_built(&mut store, root, limits, &model);
            assert!(
                old_internal
                    .iter()
                    .all(|id| !store.pages.contains_key(id) && store.free.contains(id))
            );
            let reused = store.free.clone();
            assert!(!reused.is_empty());
            for entry in &remove {
                root = insert(&mut store, root, entry.0, entry.1, limits).unwrap();
                model.push(*entry);
            }
            model.sort_by(|left, right| cmp_entry(*left, *right));
            assert_built(&mut store, root, limits, &model);
            assert!(reused.iter().any(|id| store.pages.contains_key(id)));
            if cycle == 0 {
                high = store.next;
            } else {
                assert_eq!(store.next, high);
            }
        }
        let ceiling = store.next;
        for _ in 0..3 {
            let runs = leaf_runs(&mut store, root);
            let remove = runs[0].entries.clone();
            root = delete_many(&mut store, root, &remove, limits).unwrap();
            for entry in &remove {
                root = insert(&mut store, root, entry.0, entry.1, limits).unwrap();
            }
            assert_built(&mut store, root, limits, &model);
        }
        assert_eq!(store.next, ceiling);

        let (mut store, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut store, root);
        let live = runs[0].id;
        let victim = runs
            .iter()
            .find(|run| run.entries.len() == limits.leaf_min && run.id != live)
            .expect("minimum leaf")
            .entries
            .clone();
        let before = page_image(&store);
        store.free.push(live);
        let error = delete_many(&mut store, root, &victim, limits).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(page_image(&store), before);

        let (mut store, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut store, root);
        let victim = runs
            .iter()
            .find(|run| run.entries.len() == limits.leaf_min)
            .expect("minimum leaf")
            .entries
            .clone();
        let before = page_image(&store);
        store.free.push(90_000);
        store.free.push(90_000);
        let error = delete_many(&mut store, root, &victim, limits).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(page_image(&store), before);
    }

    #[test]
    fn bulk_rejects_preexisting_corruption_without_writing() {
        let limits = small();
        let entries = sequential(24);
        let (mut probe, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut probe, root);
        let tight = runs
            .iter()
            .find(|run| run.entries.len() == limits.leaf_min)
            .unwrap()
            .entries
            .clone();

        let mut missing = tight.clone();
        missing.insert(0, (99, 99));
        let (mut store, root) = fill(limits, &entries);
        let before = page_image(&store);
        let error = delete_many(&mut store, root, &missing, limits).unwrap_err();
        assert!(
            error.to_string().contains("index entry is missing"),
            "{error}"
        );
        assert_eq!(page_image(&store), before);

        let (mut store, root) = fill(limits, &entries);
        let mut page = store.read_node(runs[0].id).unwrap();
        page.as_mut_slice()[0] = 0;
        store.write_node(runs[0].id, page).unwrap();
        let before = page_image(&store);
        let error = delete_many(&mut store, root, &tight, limits).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(page_image(&store), before);
        let (mut one, one_root) = fill(limits, &entries);
        let one_runs = leaf_runs(&mut one, one_root);
        let mut page = one.read_node(one_runs[0].id).unwrap();
        page.as_mut_slice()[0] = 0;
        one.write_node(one_runs[0].id, page).unwrap();
        let victim = *one_runs.last().unwrap().entries.last().unwrap();
        let single = delete_many(&mut one, one_root, &[victim], limits);
        assert!(single.is_ok() || single.unwrap_err().to_string().contains("corrupt"));

        let (mut store, root) = fill(limits, &entries);
        let right = runs.last().unwrap().id;
        let mut page = store.read_node(right).unwrap();
        page.as_mut_slice()[0] = 0;
        store.write_node(right, page).unwrap();
        let before = page_image(&store);
        let error = delete_many(&mut store, root, &tight, limits).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(page_image(&store), before);

        let (mut store, root) = fill(limits, &entries);
        let mut page = store.read_node(runs[1].id).unwrap();
        crate::storage::page::set_next_page(&mut page, 999);
        store.write_node(runs[1].id, page).unwrap();
        let before = page_image(&store);
        let error = delete_many(&mut store, root, &tight, limits).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(page_image(&store), before);

        let (mut store, root) = fill(limits, &entries);
        let mut node = decode_internal(&store.read_node(root).unwrap()).unwrap();
        node.children[0] = 999;
        store
            .write_node(root, encode_internal(root, &node).unwrap())
            .unwrap();
        let before = page_image(&store);
        let error = delete_many(&mut store, root, &tight, limits).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(page_image(&store), before);

        let (mut store, root) = fill(limits, &entries);
        let mut node = decode_internal(&store.read_node(root).unwrap()).unwrap();
        // A separator above every probe still routes the leftmost leaf to the
        // left child. The whole-tree check then rejects the separator. A probe
        // that the separator sends to the wrong child stops as a missing entry
        // before that walk, which is why i64::MIN is not used here.
        node.keys[0] = (i64::MAX, 0);
        store
            .write_node(root, encode_internal(root, &node).unwrap())
            .unwrap();
        let before = page_image(&store);
        let error = delete_many(&mut store, root, &runs[0].entries, limits).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(page_image(&store), before);

        let (mut store, root) = fill(limits, &entries);
        let mut page = store.read_node(root).unwrap();
        page.as_mut_slice()[0] = 0;
        store.write_node(root, page).unwrap();
        let before = page_image(&store);
        let error = delete_many(&mut store, root, &tight, limits).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(page_image(&store), before);

        let error = encode_leaf(
            1,
            &Leaf {
                entries: vec![(1, 1); LEAF_MAX + 1],
                left: 0,
                right: 0,
            },
        )
        .err()
        .expect("oversized leaf");
        assert!(error.to_string().contains("capacity"), "{error}");
        let error = encode_internal(
            1,
            &Internal {
                keys: vec![(1, 1); INTERNAL_MAX + 1],
                children: vec![1; INTERNAL_MAX + 2],
            },
        )
        .err()
        .expect("oversized internal node");
        assert!(error.to_string().contains("capacity"), "{error}");
    }

    struct Faulty {
        inner: Mem,
        fail_alloc: bool,
        fail_leaf: bool,
        fail_internal: bool,
        fail_free: bool,
        fail_read_after_leaf: bool,
        leaf_writes: u32,
    }

    impl Nodes for Faulty {
        fn read_node(&mut self, id: u32) -> Result<Page, DbError> {
            if self.fail_read_after_leaf && self.leaf_writes > 0 {
                return Err(corrupt(id));
            }
            self.inner.read_node(id)
        }
        fn write_node(&mut self, id: u32, page: Page) -> Result<(), DbError> {
            let kind = page_type(&page)?;
            if kind == TYPE_INDEX_LEAF {
                if self.fail_leaf {
                    return Err(DbError::Storage("injected leaf write failure".to_string()));
                }
                self.leaf_writes += 1;
            }
            if kind == TYPE_INDEX_INTERNAL && self.fail_internal {
                return Err(DbError::Storage(
                    "injected internal write failure".to_string(),
                ));
            }
            self.inner.write_node(id, page)
        }
        fn alloc_node(&mut self) -> Result<u32, DbError> {
            if self.fail_alloc {
                return Err(DbError::Storage(
                    "database page id space is exhausted".to_string(),
                ));
            }
            self.inner.alloc_node()
        }
        fn free_node(&mut self, id: u32) -> Result<(), DbError> {
            if self.fail_free {
                return Err(DbError::Storage("injected free failure".to_string()));
            }
            self.inner.free_node(id)
        }
    }

    #[test]
    fn bulk_faults_after_reconstruction_starts_do_not_report_success() {
        let limits = small();
        let entries = sequential(24);
        let (base, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut base.clone(), root);
        let remove = runs
            .iter()
            .find(|run| run.entries.len() == limits.leaf_min)
            .unwrap()
            .entries
            .clone();
        let before = page_image(&base);

        let mut alloc = Faulty {
            inner: base.clone(),
            fail_alloc: true,
            fail_leaf: false,
            fail_internal: false,
            fail_free: false,
            fail_read_after_leaf: false,
            leaf_writes: 0,
        };
        let error = delete_many(&mut alloc, root, &remove, limits).unwrap_err();
        assert!(error.to_string().contains("exhausted"), "{error}");
        assert_eq!(page_image(&alloc.inner), before);

        let mut leaf = Faulty {
            inner: base.clone(),
            fail_alloc: false,
            fail_leaf: true,
            fail_internal: false,
            fail_free: false,
            fail_read_after_leaf: false,
            leaf_writes: 0,
        };
        let error = delete_many(&mut leaf, root, &remove, limits).unwrap_err();
        assert!(error.to_string().contains("leaf write"), "{error}");
        assert_eq!(page_image(&leaf.inner), before);

        let mut internal = Faulty {
            inner: base.clone(),
            fail_alloc: false,
            fail_leaf: false,
            fail_internal: true,
            fail_free: false,
            fail_read_after_leaf: false,
            leaf_writes: 0,
        };
        let error = delete_many(&mut internal, root, &remove, limits).unwrap_err();
        assert!(error.to_string().contains("internal write"), "{error}");

        let mut relink = Faulty {
            inner: base.clone(),
            fail_alloc: false,
            fail_leaf: false,
            fail_internal: false,
            fail_free: false,
            fail_read_after_leaf: true,
            leaf_writes: 0,
        };
        let error = delete_many(&mut relink, root, &remove, limits).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");

        let mut free = Faulty {
            inner: base.clone(),
            fail_alloc: false,
            fail_leaf: false,
            fail_internal: false,
            fail_free: true,
            fail_read_after_leaf: false,
            leaf_writes: 0,
        };
        let error = delete_many(&mut free, root, &remove, limits).unwrap_err();
        assert!(error.to_string().contains("free"), "{error}");
    }

    #[test]
    fn bulk_then_ordinary_edits_keep_order_and_pages() {
        let limits = small();
        let (mut store, mut root, mut model) = rebuild_keep(limits, 40, 9);
        assert_built(&mut store, root, limits, &model);
        for (key, row) in [(i64::MIN, 100_u64), (i64::MAX, 101), (4, 102), (4, 4)] {
            if model.contains(&(key, row)) {
                continue;
            }
            root = insert(&mut store, root, key, row, limits).unwrap();
            model.push((key, row));
            model.sort_by(|left, right| cmp_entry(*left, *right));
            assert_built(&mut store, root, limits, &model);
        }
        let duplicate = find_equal(&mut store, root, 4).unwrap();
        let mut ordered = duplicate.clone();
        ordered.sort_unstable();
        assert_eq!(duplicate, ordered);
        let victim = model[0];
        root = delete(&mut store, root, victim.0, victim.1, limits).unwrap();
        model.retain(|entry| *entry != victim);
        assert_built(&mut store, root, limits, &model);
        let runs = leaf_runs(&mut store, root);
        let fat = runs
            .iter()
            .find(|run| run.entries.len() >= limits.leaf_min + 2)
            .or_else(|| runs.iter().find(|run| run.entries.len() >= 2))
            .unwrap();
        let grouped = fat.entries[..2].to_vec();
        root = delete_many(&mut store, root, &grouped, limits).unwrap();
        for entry in &grouped {
            model.retain(|kept| kept != entry);
        }
        assert_built(&mut store, root, limits, &model);
        if model.len() >= 2 {
            root = delete_many(&mut store, root, &model, limits).unwrap();
            assert_built(&mut store, root, limits, &[]);
        }
    }

    #[test]
    fn bulk_matches_the_reference_for_recorded_seeds() {
        let seeds = [0x31A1_u64, 0x31B2, 0x31C3, 0x31D4];
        for seed in seeds {
            for (limits, count) in [(small(), 48_i64), (production(), 400)] {
                let mut state = seed;
                let mut entries = Vec::new();
                for row in 1..=count {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let key = match state % 11 {
                        0 => i64::MIN,
                        1 => i64::MAX,
                        2 => state as i64,
                        3 => -(state as i64).saturating_abs(),
                        4 | 5 => (state as i64 % 5) - 2,
                        _ => (state as i64 % 80) - 20,
                    };
                    let entry = (key, row as u64);
                    if !entries.contains(&entry) {
                        entries.push(entry);
                    }
                }
                let mut ordered = entries.clone();
                ordered.sort_by(|left, right| cmp_entry(*left, *right));
                same_tree(limits, &entries, &[]);
                same_tree(limits, &entries, &ordered[..1]);
                same_tree(
                    limits,
                    &entries,
                    &ordered.iter().copied().step_by(2).collect::<Vec<_>>(),
                );
                same_tree(limits, &entries, &ordered[..ordered.len() / 2]);
                same_tree(limits, &entries, &ordered);
                let mut alternating = Vec::new();
                for (index, entry) in ordered.iter().copied().enumerate() {
                    if index % 2 == 0 {
                        alternating.push(entry);
                    }
                }
                same_tree(limits, &entries, &alternating);
                let (mut store, mut root) = fill(limits, &entries);
                let mut model = ordered.clone();
                for _ in 0..3 {
                    let half = model.len() / 2;
                    let remove = model[..half.max(2).min(model.len())].to_vec();
                    if remove.len() < 2 {
                        break;
                    }
                    root = delete_many(&mut store, root, &remove, limits).unwrap();
                    let expect = reference_delete(&model, &remove).unwrap();
                    assert_built(&mut store, root, limits, &expect);
                    for entry in &remove {
                        root = insert(&mut store, root, entry.0, entry.1, limits).unwrap();
                    }
                    model.sort_by(|left, right| cmp_entry(*left, *right));
                    assert_built(&mut store, root, limits, &model);
                }
            }
        }
    }

    fn agree_verifiers(limits: Limits, entries: &[Entry], remove: &[Entry]) {
        let (mut ordered_store, ordered_root) = fill(limits, entries);
        let (mut descent_store, descent_root) = fill(limits, entries);
        let ordered = delete_many(&mut ordered_store, ordered_root, remove, limits);
        let descent = {
            let _guard = ForceDescentVerify::new();
            delete_many(&mut descent_store, descent_root, remove, limits)
        };
        match (ordered, descent) {
            (Ok(left), Ok(right)) => {
                let ordered_entries = validate(&mut ordered_store, left, limits).unwrap();
                let descent_entries = validate(&mut descent_store, right, limits).unwrap();
                assert_eq!(ordered_entries, descent_entries, "{remove:?}");
                assert_eq!(page_image(&ordered_store), page_image(&descent_store));
                assert_eq!(ordered_store.free, descent_store.free);
                assert_eq!(ordered_store.next, descent_store.next);
                assert_pages(&mut ordered_store, left);
            }
            (Err(left), Err(right)) => {
                assert_eq!(left.to_string(), right.to_string(), "{remove:?}");
                assert_eq!(page_image(&ordered_store), page_image(&descent_store));
                assert_eq!(ordered_store.free, descent_store.free);
            }
            (left, right) => panic!("verifier split ordered={left:?} descent={right:?}"),
        }
    }

    #[test]
    fn ordered_leaf_chain_matches_per_posting_verification() {
        let limits = small();
        let entries = sequential(48);
        let (mut probe, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut probe, root);
        assert!(runs.len() >= 4, "leaves {}", runs.len());
        assert!(height(&mut probe, root).unwrap() >= 2);

        agree_verifiers(limits, &entries, &[]);
        agree_verifiers(limits, &entries, &[(0, 1)]);
        agree_verifiers(limits, &entries, &[(99, 99), entries[0]]);
        agree_verifiers(limits, &entries, &[entries[10], (99, 99), entries[11]]);
        agree_verifiers(limits, &entries, &entries[..entries.len() - 1]);
        let mut missing_last = entries.clone();
        missing_last.push((49, 49));
        agree_verifiers(limits, &entries, &missing_last);
        agree_verifiers(limits, &entries, &[entries[0], entries[0]]);
        agree_verifiers(limits, &entries, &[entries[3], entries[4], entries[3]]);
        agree_verifiers(limits, &entries, &[(4, 4), (4, 40)]);
        agree_verifiers(limits, &entries, &[entries[0], (4, 0)]);
        agree_verifiers(limits, &entries, &entries);
        agree_verifiers(limits, &entries, &entries[..1]);

        let mut reversed = entries.clone();
        reversed.reverse();
        agree_verifiers(limits, &entries, &reversed);
        let partial = vec![entries[0], entries[2], entries[1], entries[4]];
        agree_verifiers(limits, &entries, &partial);
        let back = vec![entries[0], entries[20], entries[5], entries[21]];
        agree_verifiers(limits, &entries, &back);

        let boundary = runs[0].entries.len();
        let mut after_boundary = entries[..boundary].to_vec();
        after_boundary.push((entries[boundary - 1].0, entries[boundary - 1].1 + 1));
        agree_verifiers(limits, &entries, &after_boundary);
        let mut before_boundary = entries[..boundary - 1].to_vec();
        before_boundary.push((entries[boundary].0, entries[boundary].1.saturating_add(1)));
        before_boundary.push(entries[boundary]);
        agree_verifiers(limits, &entries, &before_boundary);

        let duplicates: Vec<Entry> = (1..=30).map(|row| (4, row)).collect();
        agree_verifiers(limits, &duplicates, &duplicates[2..20]);
        agree_verifiers(
            limits,
            &duplicates,
            &[duplicates[0], duplicates[0], duplicates[1]],
        );
        let extremes = vec![
            (i64::MIN, 1),
            (i64::MIN, u64::MAX),
            (-1, 2),
            (0, 3),
            (i64::MAX, 6),
            (i64::MAX, 7),
        ];
        agree_verifiers(limits, &extremes, &extremes);
        agree_verifiers(limits, &extremes, &[extremes[0], (i64::MIN, 2)]);

        for id in [runs[1].id, runs[3].id, root] {
            let (mut left, left_root) = fill(limits, &entries);
            let (mut right, right_root) = fill(limits, &entries);
            for store in [&mut left, &mut right] {
                let mut page = store.read_node(id).unwrap();
                page.as_mut_slice()[0] = 0;
                store.write_node(id, page).unwrap();
            }
            let remove = runs[0].entries.clone();
            let ordered = delete_many(&mut left, left_root, &remove, limits);
            let descent = {
                let _guard = ForceDescentVerify::new();
                delete_many(&mut right, right_root, &remove, limits)
            };
            match (ordered, descent) {
                (Err(left_error), Err(right_error)) => {
                    assert_eq!(left_error.to_string(), right_error.to_string(), "page {id}");
                }
                (left_result, right_result) => {
                    panic!("corrupt page {id} split {left_result:?} {right_result:?}")
                }
            }
            assert_eq!(page_image(&left), page_image(&right), "page {id}");
        }

        let (mut left, left_root) = fill(limits, &entries);
        let (mut right, right_root) = fill(limits, &entries);
        for store in [&mut left, &mut right] {
            let mut node = decode_internal(&store.read_node(root).unwrap()).unwrap();
            node.keys[0] = (i64::MIN, 0);
            store
                .write_node(root, encode_internal(root, &node).unwrap())
                .unwrap();
        }
        let ordered = delete_many(&mut left, left_root, &runs[0].entries, limits);
        let descent = {
            let _guard = ForceDescentVerify::new();
            delete_many(&mut right, right_root, &runs[0].entries, limits)
        };
        let (Err(left_error), Err(right_error)) = (ordered, descent) else {
            panic!("separator errors diverged");
        };
        assert_eq!(left_error.to_string(), right_error.to_string());
        assert_eq!(page_image(&left), page_image(&right));

        let (mut left, left_root) = fill(limits, &entries);
        let (mut right, right_root) = fill(limits, &entries);
        for store in [&mut left, &mut right] {
            let mut page = store.read_node(runs[1].id).unwrap();
            crate::storage::page::set_next_page(&mut page, runs[1].id);
            store.write_node(runs[1].id, page).unwrap();
        }
        let span = {
            let mut remove = runs[0].entries.clone();
            remove.extend(runs[1].entries.iter().copied());
            remove.extend(runs[2].entries.iter().copied());
            remove
        };
        let ordered = delete_many(&mut left, left_root, &span, limits);
        let descent = {
            let _guard = ForceDescentVerify::new();
            delete_many(&mut right, right_root, &span, limits)
        };
        let (Err(left_error), Err(right_error)) = (ordered, descent) else {
            panic!("cycle errors diverged");
        };
        assert_eq!(left_error.to_string(), right_error.to_string());
        assert_eq!(page_image(&left), page_image(&right));

        let _profile = crate::storage::profile::enable();
        let (mut store, root) = fill(limits, &entries);
        delete_many(&mut store, root, &entries, limits).unwrap();
        let ordered_profile = crate::storage::profile::take();
        crate::storage::profile::reset();
        let (mut store, root) = fill(limits, &entries);
        {
            let _guard = ForceDescentVerify::new();
            delete_many(&mut store, root, &entries, limits).unwrap();
        }
        let descent_profile = crate::storage::profile::take();
        assert!(
            ordered_profile.descents < descent_profile.descents,
            "ordered {:?} descent {:?}",
            ordered_profile.descents,
            descent_profile.descents
        );
        assert!(ordered_profile.ordered_verified >= entries.len() as u64);
        assert_eq!(ordered_profile.fallback_verified, 0);
        assert!(descent_profile.fallback_verified >= entries.len() as u64);
        assert!(ordered_profile.sibling_steps > 0);

        crate::storage::profile::reset();
        let (mut store, root) = fill(limits, &entries);
        delete_many(&mut store, root, &reversed, limits).unwrap();
        let reversed_profile = crate::storage::profile::take();
        assert!(
            reversed_profile.ordered_verified > 0,
            "{reversed_profile:?}"
        );
        assert_eq!(
            reversed_profile.fallback_verified, 0,
            "{reversed_profile:?}"
        );
        assert!(
            reversed_profile.descents < descent_profile.descents,
            "reversed {} forward-descent {}",
            reversed_profile.descents,
            descent_profile.descents
        );
        crate::storage::profile::reset();
        let (mut store, root) = fill(limits, &entries);
        delete_many(&mut store, root, &partial, limits).unwrap();
        let partial_profile = crate::storage::profile::take();
        assert_eq!(partial_profile.ordered_verified, 0);
        assert!(partial_profile.fallback_verified > 0);

        let (mut ordered_store, mut ordered_root) = fill(limits, &entries);
        let (mut descent_store, mut descent_root) = fill(limits, &entries);
        for _ in 0..3 {
            let runs = leaf_runs(&mut ordered_store, ordered_root);
            let mut remove = runs[0].entries.clone();
            if runs.len() > 1 {
                remove.extend(runs[1].entries.iter().copied());
            }
            ordered_root = delete_many(&mut ordered_store, ordered_root, &remove, limits).unwrap();
            {
                let _guard = ForceDescentVerify::new();
                descent_root =
                    delete_many(&mut descent_store, descent_root, &remove, limits).unwrap();
            }
            assert_eq!(
                validate(&mut ordered_store, ordered_root, limits).unwrap(),
                validate(&mut descent_store, descent_root, limits).unwrap()
            );
            assert_eq!(page_image(&ordered_store), page_image(&descent_store));
            for entry in &remove {
                ordered_root =
                    insert(&mut ordered_store, ordered_root, entry.0, entry.1, limits).unwrap();
                {
                    let _guard = ForceDescentVerify::new();
                    descent_root =
                        insert(&mut descent_store, descent_root, entry.0, entry.1, limits).unwrap();
                }
            }
            assert_eq!(page_image(&ordered_store), page_image(&descent_store));
        }

        agree_verifiers(production(), &sequential(800), &sequential(400));
    }

    #[test]
    fn presence_map_bound_uses_the_page_fanout() {
        assert_eq!(max_index_pages(1), 1);
        assert_eq!(max_index_pages(2), 205);
        assert_eq!(max_index_pages(3), 41_821);
        assert!(!presence_reads_exceed_tree(0, 3));
        assert!(!presence_reads_exceed_tree(102, 2));
        assert!(presence_reads_exceed_tree(103, 2));
        assert!(!presence_reads_exceed_tree(13_940, 3));
        assert!(presence_reads_exceed_tree(13_941, 3));
        assert!(presence_reads_exceed_tree(2, 1));
        assert_eq!(max_index_pages(4), 8_531_485);
        assert!(max_index_pages(9) > max_index_pages(4));
        assert!(max_index_pages(9) < u64::MAX);
        assert_eq!(max_index_pages(10), u64::MAX);
        assert_eq!(max_index_pages(64), u64::MAX);
        assert_eq!(max_index_pages(0), 1);
        assert!(!presence_reads_exceed_tree(usize::MAX, 64));
        assert!(presence_reads_exceed_tree(usize::MAX, 3));
    }

    fn interleaved(entries: &[Entry]) -> Vec<Entry> {
        let mut out = Vec::with_capacity(entries.len());
        let mut low = 0;
        let mut high = entries.len();
        while low < high {
            high -= 1;
            out.push(entries[high]);
            if low < high {
                out.push(entries[low]);
                low += 1;
            }
        }
        out
    }

    fn agree_membership(limits: Limits, entries: &[Entry], remove: &[Entry]) {
        let (mut presence_store, presence_root) = fill(limits, entries);
        let (mut chain_store, chain_root) = fill(limits, entries);
        let (mut descent_store, descent_root) = fill(limits, entries);
        let presence = {
            let _guard = ForcePresenceMap::new();
            delete_many(&mut presence_store, presence_root, remove, limits)
        };
        let chain = {
            let _guard = ForceChainVerify::new();
            delete_many(&mut chain_store, chain_root, remove, limits)
        };
        let descent = {
            let _guard = ForceDescentVerify::new();
            delete_many(&mut descent_store, descent_root, remove, limits)
        };
        match (presence, chain, descent) {
            (Ok(left), Ok(middle), Ok(right)) => {
                let left_entries = validate(&mut presence_store, left, limits).unwrap();
                let middle_entries = validate(&mut chain_store, middle, limits).unwrap();
                let right_entries = validate(&mut descent_store, right, limits).unwrap();
                assert_eq!(left_entries, right_entries, "{remove:?}");
                assert_eq!(middle_entries, right_entries, "{remove:?}");
                assert_eq!(page_image(&presence_store), page_image(&descent_store));
                assert_eq!(page_image(&chain_store), page_image(&descent_store));
                assert_eq!(presence_store.free, descent_store.free);
                assert_eq!(chain_store.free, descent_store.free);
                assert_pages(&mut presence_store, left);
            }
            (Err(left), Err(middle), Err(right)) => {
                assert_eq!(left.to_string(), right.to_string(), "{remove:?}");
                assert_eq!(middle.to_string(), right.to_string(), "{remove:?}");
                assert_eq!(page_image(&presence_store), page_image(&descent_store));
                assert_eq!(page_image(&chain_store), page_image(&descent_store));
            }
            (left, middle, right) => {
                panic!("membership split presence={left:?} chain={middle:?} descent={right:?}")
            }
        }
    }

    fn agree_damaged(
        limits: Limits,
        entries: &[Entry],
        remove: &[Entry],
        damage: &mut dyn FnMut(&mut Mem, u32),
    ) {
        let (mut presence, presence_root) = fill(limits, entries);
        let (mut chain, chain_root) = fill(limits, entries);
        let (mut descent, descent_root) = fill(limits, entries);
        damage(&mut presence, presence_root);
        damage(&mut chain, chain_root);
        damage(&mut descent, descent_root);
        let presence_result = {
            let _guard = ForcePresenceMap::new();
            delete_many(&mut presence, presence_root, remove, limits)
        };
        let chain_result = {
            let _guard = ForceChainVerify::new();
            delete_many(&mut chain, chain_root, remove, limits)
        };
        let descent_result = {
            let _guard = ForceDescentVerify::new();
            delete_many(&mut descent, descent_root, remove, limits)
        };
        match (presence_result, chain_result, descent_result) {
            (Ok(left), Ok(middle), Ok(right)) => {
                assert_eq!(
                    validate(&mut presence, left, limits).unwrap(),
                    validate(&mut descent, right, limits).unwrap()
                );
                assert_eq!(
                    validate(&mut chain, middle, limits).unwrap(),
                    validate(&mut descent, right, limits).unwrap()
                );
                assert_eq!(page_image(&presence), page_image(&descent));
                assert_eq!(page_image(&chain), page_image(&descent));
            }
            (Err(left), Err(middle), Err(right)) => {
                assert_eq!(left.to_string(), right.to_string());
                assert_eq!(middle.to_string(), right.to_string());
                assert_eq!(page_image(&presence), page_image(&descent));
                assert_eq!(page_image(&chain), page_image(&descent));
            }
            (left, middle, right) => {
                panic!("damaged split presence={left:?} chain={middle:?} descent={right:?}")
            }
        }
    }

    #[test]
    fn presence_map_matches_chain_and_descent_verification() {
        let limits = small();
        let entries = sequential(48);
        let mixed = interleaved(&entries);
        assert!(monotonic_direction(&mixed).is_none());
        agree_membership(limits, &entries, &[]);
        agree_membership(limits, &entries, &entries[..1]);
        agree_membership(limits, &entries, &[(99, 99), entries[0]]);
        agree_membership(limits, &entries, &[entries[0], (99, 99), entries[2]]);
        let mut missing_last = mixed.clone();
        missing_last.push((99, 99));
        agree_membership(limits, &entries, &missing_last);
        agree_membership(limits, &entries, &[entries[0], entries[0], entries[1]]);
        agree_membership(limits, &entries, &[entries[4], entries[1], entries[4]]);
        agree_membership(limits, &entries, &[(4, 4), (4, 40), (5, 5)]);
        agree_membership(limits, &entries, &mixed);
        agree_membership(limits, &entries, &mixed[..mixed.len() / 2]);
        let mut back = vec![entries[30], entries[2], entries[31], entries[1]];
        back.push(entries[30]);
        agree_membership(limits, &entries, &back);
        let duplicates: Vec<Entry> = (1..=30).map(|row| (7, row)).collect();
        let mixed_duplicates = interleaved(&duplicates);
        agree_membership(limits, &duplicates, &mixed_duplicates);
        let extremes = vec![
            (i64::MIN, 1),
            (i64::MIN, u64::MAX),
            (-4, 2),
            (0, 3),
            (1, 4),
            (i64::MAX, 8),
            (i64::MAX, 9),
        ];
        agree_membership(limits, &extremes, &interleaved(&extremes));

        let (mut probe, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut probe, root);
        for id in [
            runs[0].id,
            runs[runs.len() / 2].id,
            runs[runs.len() - 1].id,
            root,
        ] {
            let (mut left, left_root) = fill(limits, &entries);
            let (mut right, right_root) = fill(limits, &entries);
            for store in [&mut left, &mut right] {
                let mut page = store.read_node(id).unwrap();
                page.as_mut_slice()[0] = 0;
                store.write_node(id, page).unwrap();
            }
            let presence = {
                let _guard = ForcePresenceMap::new();
                delete_many(&mut left, left_root, &mixed, limits)
            };
            let descent = {
                let _guard = ForceDescentVerify::new();
                delete_many(&mut right, right_root, &mixed, limits)
            };
            match (presence, descent) {
                (Err(left_error), Err(right_error)) => {
                    assert_eq!(left_error.to_string(), right_error.to_string(), "page {id}");
                }
                (left_result, right_result) => {
                    panic!("corrupt page {id} split {left_result:?} {right_result:?}")
                }
            }
            assert_eq!(page_image(&left), page_image(&right), "page {id}");
        }

        let mut damage = |store: &mut Mem, root_id: u32| {
            let mut node = decode_internal(&store.read_node(root_id).unwrap()).unwrap();
            node.keys[0] = (i64::MIN, 0);
            store
                .write_node(root_id, encode_internal(root_id, &node).unwrap())
                .unwrap();
        };
        agree_damaged(limits, &entries, &mixed, &mut damage);

        let mut damage = |store: &mut Mem, root_id: u32| {
            let mut node = decode_internal(&store.read_node(root_id).unwrap()).unwrap();
            node.children[0] = 0;
            store
                .write_node(root_id, encode_internal(root_id, &node).unwrap())
                .unwrap();
        };
        agree_damaged(limits, &entries, &mixed, &mut damage);

        let mut damage = |store: &mut Mem, root_id: u32| {
            let runs = leaf_runs(store, root_id);
            let mut page = store.read_node(runs[1].id).unwrap();
            crate::storage::page::set_next_page(&mut page, runs[1].id);
            store.write_node(runs[1].id, page).unwrap();
        };
        agree_damaged(limits, &entries, &mixed, &mut damage);

        let mut damage = |store: &mut Mem, root_id: u32| {
            let live = leaf_runs(store, root_id)[0].id;
            store.free.push(live);
        };
        agree_damaged(limits, &entries, &mixed, &mut damage);

        let mut damage = |store: &mut Mem, _root_id: u32| {
            store.free.push(90_000);
            store.free.push(90_000);
        };
        agree_damaged(limits, &entries, &mixed, &mut damage);

        let _profile = crate::storage::profile::enable();
        let (mut store, root) = fill(limits, &entries);
        {
            let _guard = ForcePresenceMap::new();
            delete_many(&mut store, root, &mixed, limits).unwrap();
        }
        let presence_profile = crate::storage::profile::take();
        crate::storage::profile::reset();
        let (mut store, root) = fill(limits, &entries);
        {
            let _guard = ForceChainVerify::new();
            delete_many(&mut store, root, &mixed, limits).unwrap();
        }
        let chain_profile = crate::storage::profile::take();
        assert!(
            presence_profile.presence_verified > 0,
            "{presence_profile:?}"
        );
        assert!(
            presence_profile.presence_builds >= 1,
            "{presence_profile:?}"
        );
        assert_eq!(presence_profile.ordered_verified, 0);
        assert!(
            presence_profile.descents < chain_profile.descents,
            "presence {} chain {}",
            presence_profile.descents,
            chain_profile.descents
        );
        assert!(chain_profile.fallback_verified > 0, "{chain_profile:?}");
        assert_eq!(chain_profile.presence_builds, 0);

        crate::storage::profile::reset();
        let reversed = {
            let mut copy = entries.clone();
            copy.reverse();
            copy
        };
        let (mut store, root) = fill(limits, &entries);
        {
            let _guard = ForcePresenceMap::new();
            delete_many(&mut store, root, &reversed, limits).unwrap();
        }
        let reversed_profile = crate::storage::profile::take();
        assert_eq!(reversed_profile.presence_builds, 0, "{reversed_profile:?}");
        assert!(
            reversed_profile.ordered_verified > 0,
            "{reversed_profile:?}"
        );

        crate::storage::profile::reset();
        let (mut store, root) = fill(production(), &sequential(800));
        let wide = interleaved(&sequential(800));
        let short = wide[..3].to_vec();
        delete_many(&mut store, root, &short, production()).unwrap();
        let short_profile = crate::storage::profile::take();
        assert_eq!(short_profile.presence_builds, 0, "{short_profile:?}");

        crate::storage::profile::reset();
        let (mut store, root) = fill(production(), &sequential(800));
        let long = wide[..200].to_vec();
        let tree_height = height(&mut store, root).unwrap();
        delete_many(&mut store, root, &long, production()).unwrap();
        let long_profile = crate::storage::profile::take();
        if presence_reads_exceed_tree(long.len().saturating_sub(1), tree_height) {
            assert!(
                long_profile.presence_builds >= 1,
                "{long_profile:?} height {tree_height}"
            );
            assert!(long_profile.presence_verified > 0, "{long_profile:?}");
        }
    }

    fn live_root(store: &Mem) -> u32 {
        if store.pages.is_empty() {
            return 0;
        }
        let mut children = HashSet::new();
        for (id, page) in &store.pages {
            if page_type(page).unwrap() != TYPE_INDEX_INTERNAL {
                continue;
            }
            let node = decode_internal(page).unwrap();
            for child in node.children {
                assert!(children.insert(child), "page {id} repeats child {child}");
            }
        }
        let roots: Vec<_> = store
            .pages
            .keys()
            .copied()
            .filter(|id| !children.contains(id))
            .collect();
        assert_eq!(roots.len(), 1, "roots {roots:?}");
        roots[0]
    }

    fn accepted_prefix(entries: &[Entry], remove: &[Entry]) -> Vec<Entry> {
        let mut live: HashSet<Entry> = entries.iter().copied().collect();
        let mut prefix = Vec::new();
        for entry in remove {
            if !live.remove(entry) {
                break;
            }
            prefix.push(*entry);
        }
        prefix
    }

    /// Three verifiers must remove the same request prefix the sorted reference
    /// accepts, and must leave every later request in the tree.
    fn delete_verified(label: &str, limits: Limits, entries: &[Entry], remove: &[Entry]) {
        let prefix = accepted_prefix(entries, remove);
        let expect = reference_delete(entries, &prefix).unwrap();
        let mut seen_image = None;
        let mut seen_free = None;
        let mut seen_error = None;
        for mode in ["presence", "chain", "descent"] {
            let (mut store, root) = fill(limits, entries);
            let result = match mode {
                "presence" => {
                    let _guard = ForcePresenceMap::new();
                    delete_many(&mut store, root, remove, limits)
                }
                "chain" => {
                    let _guard = ForceChainVerify::new();
                    delete_many(&mut store, root, remove, limits)
                }
                "descent" => {
                    let _guard = ForceDescentVerify::new();
                    delete_many(&mut store, root, remove, limits)
                }
                _ => unreachable!(),
            };
            let live = match result {
                Ok(root) => {
                    assert_eq!(prefix.len(), remove.len(), "{label} {mode}");
                    assert_eq!(live_root(&store), root, "{label} {mode}");
                    root
                }
                Err(error) => {
                    assert_ne!(prefix.len(), remove.len(), "{label} {mode}: {error}");
                    let text = error.to_string();
                    assert!(
                        text.contains("index entry is missing"),
                        "{label} {mode}: {text}"
                    );
                    if let Some(previous) = &seen_error {
                        assert_eq!(previous, &text, "{label} {mode}");
                    } else {
                        seen_error = Some(text);
                    }
                    live_root(&store)
                }
            };
            let got = validate(&mut store, live, limits).unwrap();
            assert_eq!(got, expect, "{label} {mode}");
            assert_built(&mut store, live, limits, &expect);
            let image = page_image(&store);
            let free = store.free.clone();
            if let Some(previous) = &seen_image {
                assert_eq!(previous, &image, "{label} {mode}");
                assert_eq!(seen_free.as_ref().unwrap(), &free, "{label} {mode}");
            } else {
                seen_image = Some(image);
                seen_free = Some(free);
            }
        }
        if prefix.len() != remove.len() {
            assert!(seen_error.is_some(), "{label}");
            for later in remove.iter().skip(prefix.len() + 1) {
                if entries.contains(later) && !prefix.contains(later) {
                    assert!(
                        expect.contains(later),
                        "{label} removed a posting after the first error: {later:?}"
                    );
                }
            }
        }
    }

    struct Trio {
        presence: Mem,
        chain: Mem,
        descent: Mem,
        presence_root: u32,
        chain_root: u32,
        descent_root: u32,
        model: Vec<Entry>,
    }

    impl Trio {
        fn open(limits: Limits, entries: &[Entry]) -> Self {
            let (presence, presence_root) = fill(limits, entries);
            let (chain, chain_root) = fill(limits, entries);
            let (descent, descent_root) = fill(limits, entries);
            Self {
                presence,
                chain,
                descent,
                presence_root,
                chain_root,
                descent_root,
                model: reference_delete(entries, &[]).unwrap(),
            }
        }

        fn delete(&mut self, limits: Limits, remove: &[Entry]) {
            self.presence_root = {
                let _guard = ForcePresenceMap::new();
                delete_many(&mut self.presence, self.presence_root, remove, limits).unwrap()
            };
            self.chain_root = {
                let _guard = ForceChainVerify::new();
                delete_many(&mut self.chain, self.chain_root, remove, limits).unwrap()
            };
            self.descent_root = {
                let _guard = ForceDescentVerify::new();
                delete_many(&mut self.descent, self.descent_root, remove, limits).unwrap()
            };
            assert_eq!(page_image(&self.presence), page_image(&self.chain));
            assert_eq!(page_image(&self.presence), page_image(&self.descent));
            assert_eq!(self.presence.free, self.chain.free);
            assert_eq!(self.presence.free, self.descent.free);
            assert_eq!(live_root(&self.presence), self.presence_root);
            self.model = reference_delete(&self.model, remove).unwrap();
            assert_built(&mut self.presence, self.presence_root, limits, &self.model);
            assert_eq!(
                validate(&mut self.chain, self.chain_root, limits).unwrap(),
                self.model
            );
            assert_eq!(
                validate(&mut self.descent, self.descent_root, limits).unwrap(),
                self.model
            );
        }

        fn add(&mut self, limits: Limits, entry: Entry) {
            self.presence_root = insert(
                &mut self.presence,
                self.presence_root,
                entry.0,
                entry.1,
                limits,
            )
            .unwrap();
            self.chain_root =
                insert(&mut self.chain, self.chain_root, entry.0, entry.1, limits).unwrap();
            self.descent_root = insert(
                &mut self.descent,
                self.descent_root,
                entry.0,
                entry.1,
                limits,
            )
            .unwrap();
            assert_eq!(page_image(&self.presence), page_image(&self.chain));
            assert_eq!(page_image(&self.presence), page_image(&self.descent));
            self.model.push(entry);
            self.model = reference_delete(&self.model, &[]).unwrap();
            assert_built(&mut self.presence, self.presence_root, limits, &self.model);
        }
    }

    #[test]
    fn presence_map_batches_follow_the_rebuilt_root() {
        let _profile = crate::storage::profile::enable();
        let limits = small();
        let entries = sequential(48);
        let (mut probe, probe_root) = fill(limits, &entries);
        let before_height = height(&mut probe, probe_root).unwrap();
        assert!(before_height >= 3, "{before_height}");
        let runs = leaf_runs(&mut probe, probe_root);
        let fat = runs
            .iter()
            .find(|run| run.id != probe_root && run.entries.len() >= limits.leaf_min + 2)
            .expect("fat leaf");
        let thin = runs
            .iter()
            .find(|run| {
                run.id != probe_root && run.id != fat.id && run.entries.len() == limits.leaf_min
            })
            .expect("thin leaf");
        let fat_keys = [fat.entries[0], fat.entries[2]];
        let thin_key = thin.entries[0];
        assert!(!fat.entries.contains(&thin_key));

        let mut grouped_then_map = vec![fat_keys[0], fat_keys[1], thin_key];
        grouped_then_map.extend(interleaved(&without(
            &entries,
            &[fat_keys[0], fat_keys[1], thin_key],
        )));
        crate::storage::profile::reset();
        {
            let (mut store, root) = fill(limits, &entries);
            let _guard = ForcePresenceMap::new();
            delete_many(&mut store, root, &grouped_then_map, limits).unwrap();
        }
        let grouped_profile = crate::storage::profile::take();
        assert!(grouped_profile.leaf_batches >= 1, "{grouped_profile:?}");
        assert_eq!(grouped_profile.presence_builds, 1, "{grouped_profile:?}");
        assert_eq!(grouped_profile.bulk_spans, 1, "{grouped_profile:?}");
        assert_eq!(grouped_profile.ordered_verified, 0, "{grouped_profile:?}");
        let mut trio = Trio::open(limits, &entries);
        trio.delete(limits, &grouped_then_map);
        assert!(trio.model.is_empty());

        let mut single_then_map = vec![fat_keys[0], thin_key];
        single_then_map.extend(interleaved(&without(&entries, &[fat_keys[0], thin_key])));
        crate::storage::profile::reset();
        {
            let (mut store, root) = fill(limits, &entries);
            let _guard = ForcePresenceMap::new();
            delete_many(&mut store, root, &single_then_map, limits).unwrap();
        }
        let single_profile = crate::storage::profile::take();
        assert!(single_profile.index_singles >= 1, "{single_profile:?}");
        assert_eq!(single_profile.presence_builds, 1, "{single_profile:?}");
        assert_eq!(single_profile.bulk_spans, 1, "{single_profile:?}");
        Trio::open(limits, &entries).delete(limits, &single_then_map);

        let mut first_rest = without(&entries, &[thin_key]);
        let second_keys = first_rest.split_off(first_rest.len() / 2);
        let mut first = vec![thin_key];
        first.extend(interleaved(&first_rest));
        let second = interleaved(&second_keys);
        crate::storage::profile::reset();
        let (mut store, mut root) = fill(limits, &entries);
        {
            let _guard = ForcePresenceMap::new();
            root = delete_many(&mut store, root, &first, limits).unwrap();
        }
        let first_profile = crate::storage::profile::take();
        assert_eq!(first_profile.presence_builds, 1, "{first_profile:?}");
        assert_eq!(first_profile.bulk_spans, 1, "{first_profile:?}");
        assert_eq!(first_profile.leaf_batches, 0, "{first_profile:?}");
        let returned = root;
        crate::storage::profile::reset();
        {
            let _guard = ForcePresenceMap::new();
            root = delete_many(&mut store, returned, &second, limits).unwrap();
        }
        let second_profile = crate::storage::profile::take();
        assert_eq!(second_profile.presence_builds, 1, "{second_profile:?}");
        assert!(second_profile.bulk_spans >= 1, "{second_profile:?}");
        assert_built(&mut store, root, limits, &[]);

        let mut trio = Trio::open(limits, &entries);
        let height_before = height(&mut trio.presence, trio.presence_root).unwrap();
        trio.delete(limits, &first);
        let freed = trio.presence.free.clone();
        assert!(!freed.is_empty(), "first rebuild freed no page");
        let height_mid = height(&mut trio.presence, trio.presence_root).unwrap();
        assert!(
            height_mid <= height_before,
            "{height_before} -> {height_mid}"
        );
        let runs = leaf_runs(&mut trio.presence, trio.presence_root);
        let packed = runs
            .iter()
            .find(|run| run.id != trio.presence_root && run.entries.len() == limits.leaf_max)
            .expect("packed leaf");
        let packed_keys = packed.entries.clone();
        let extras: Vec<Entry> = trio
            .model
            .iter()
            .copied()
            .filter(|entry| !packed_keys.contains(entry))
            .take(5)
            .collect();
        let mut reuse_request = vec![packed_keys[0], packed_keys[1], packed_keys[2]];
        reuse_request.extend(extras);
        trio.delete(limits, &reuse_request);
        assert!(
            freed.iter().any(|id| trio.presence.pages.contains_key(id)),
            "second rebuild did not reuse a freed page"
        );
        let runs = leaf_runs(&mut trio.presence, trio.presence_root);
        let grouped = runs
            .iter()
            .find(|run| run.id != trio.presence_root && run.entries.len() >= limits.leaf_min + 2)
            .expect("later fat leaf");
        let grouped_request = vec![grouped.entries[0], grouped.entries[2]];
        crate::storage::profile::reset();
        {
            let mut store = trio.presence.clone();
            let _guard = ForcePresenceMap::new();
            delete_many(&mut store, trio.presence_root, &grouped_request, limits).unwrap();
        }
        let grouped_later = crate::storage::profile::take();
        assert!(grouped_later.leaf_batches >= 1, "{grouped_later:?}");
        assert_eq!(grouped_later.presence_builds, 0, "{grouped_later:?}");
        trio.delete(limits, &grouped_request);
        let one = trio.model[0];
        crate::storage::profile::reset();
        {
            let mut store = trio.presence.clone();
            let _guard = ForcePresenceMap::new();
            delete_many(&mut store, trio.presence_root, &[one], limits).unwrap();
        }
        let single_later = crate::storage::profile::take();
        assert!(single_later.index_singles >= 1, "{single_later:?}");
        assert_eq!(single_later.presence_builds, 0, "{single_later:?}");
        trio.delete(limits, &[one]);

        let mut contraction = Trio::open(limits, &entries);
        let tall = height(&mut contraction.presence, contraction.presence_root).unwrap();
        let keep = entries[0];
        let almost = if thin_key == keep {
            interleaved(&without(&entries, &[keep]))
        } else {
            let mut request = vec![thin_key];
            request.extend(interleaved(&without(&entries, &[thin_key, keep])));
            request
        };
        contraction.delete(limits, &almost);
        let short = height(&mut contraction.presence, contraction.presence_root).unwrap();
        assert!(short < tall, "{tall} -> {short}");
        assert_eq!(contraction.model, vec![keep]);
        contraction.delete(limits, &[keep]);
        assert_eq!(contraction.presence_root, 0);
        let freed = contraction.presence.free.clone();
        assert!(!freed.is_empty());
        for row in 1..=24 {
            contraction.add(limits, (20_000 + row, row as u64));
        }
        assert!(
            freed
                .iter()
                .any(|id| contraction.presence.pages.contains_key(id)),
            "insert did not reuse a freed page"
        );
        let reused = interleaved(&contraction.model);
        contraction.delete(limits, &reused);
        assert!(contraction.model.is_empty());
    }

    #[test]
    fn presence_map_first_error_matches_the_sorted_reference() {
        let _profile = crate::storage::profile::enable();
        let limits = small();
        let entries = sequential(48);
        let (mut probe, probe_root) = fill(limits, &entries);
        let runs = leaf_runs(&mut probe, probe_root);
        let fat = runs
            .iter()
            .find(|run| run.id != probe_root && run.entries.len() >= limits.leaf_min + 2)
            .expect("fat leaf");
        let thin = runs
            .iter()
            .find(|run| {
                run.id != probe_root && run.id != fat.id && run.entries.len() == limits.leaf_min
            })
            .expect("thin leaf");
        let fat_keys = [fat.entries[0], fat.entries[2]];
        let thin_key = thin.entries[0];
        let left = &runs[runs.len() / 2 - 1];
        let right = &runs[runs.len() / 2];
        let boundary = *left.entries.last().unwrap();
        let between = (boundary.0, boundary.1 + 1);
        assert!(!entries.contains(&between));
        assert_eq!(
            cmp_entry(between, right.entries[0]),
            std::cmp::Ordering::Less
        );

        delete_verified("missing-first", limits, &entries, &[(99, 99), entries[0]]);
        let mut after_prefix = vec![thin_key];
        after_prefix.extend(
            entries
                .iter()
                .copied()
                .filter(|entry| *entry != thin_key)
                .take(6),
        );
        let survivor = entries
            .iter()
            .copied()
            .rev()
            .find(|entry| !after_prefix.contains(entry));
        let survivor = survivor.expect("survivor");
        after_prefix.push((99, 99));
        after_prefix.push(survivor);
        delete_verified("missing-after-prefix", limits, &entries, &after_prefix);
        crate::storage::profile::reset();
        {
            let (mut store, root) = fill(limits, &entries);
            let _guard = ForcePresenceMap::new();
            let error = delete_many(&mut store, root, &after_prefix, limits).unwrap_err();
            assert!(
                error.to_string().contains("index entry is missing"),
                "{error}"
            );
        }
        let rebuilt = crate::storage::profile::take();
        assert!(rebuilt.presence_builds >= 1, "{rebuilt:?}");
        assert!(rebuilt.bulk_spans >= 1, "{rebuilt:?}");
        assert!(rebuilt.map_misses >= 1, "{rebuilt:?}");

        delete_verified(
            "duplicate-in-batch",
            limits,
            &entries,
            &[thin_key, entries[10], thin_key, entries[20]],
        );
        let later = entries
            .iter()
            .copied()
            .find(|entry| *entry != fat_keys[0] && *entry != fat_keys[1])
            .unwrap();
        delete_verified(
            "duplicate-later-batch",
            limits,
            &entries,
            &[fat_keys[0], fat_keys[1], fat_keys[0], later],
        );
        let mut same_key = sequential(20);
        same_key.push((4, 40));
        same_key.push((4, 41));
        delete_verified(
            "two-row-ids",
            limits,
            &same_key,
            &[(4, 41), (4, 40), (5, 5)],
        );
        delete_verified(
            "two-row-ids-repeat",
            limits,
            &same_key,
            &[(4, 41), (4, 40), (4, 41), (6, 6)],
        );
        delete_verified(
            "direction-change",
            limits,
            &entries,
            &[(1, 1), (30, 30), (10, 10), (99, 99), (11, 11)],
        );
        delete_verified(
            "leaf-boundary",
            limits,
            &entries,
            &[boundary, between, right.entries[0]],
        );
    }

    #[test]
    fn presence_map_eligibility_follows_the_fanout_bound() {
        let _profile = crate::storage::profile::enable();
        let mut empty = Mem::new();
        let error = delete_many(&mut empty, 0, &[(1, 1)], small()).unwrap_err();
        assert!(
            error.to_string().contains("index entry is missing"),
            "{error}"
        );
        assert!(empty.pages.is_empty());
        let (mut store, root) = fill(small(), &sequential(4));
        assert_eq!(delete_many(&mut store, root, &[], small()).unwrap(), root);

        crate::storage::profile::reset();
        let (mut store, root) = fill(small(), &sequential(4));
        delete_many(&mut store, root, &[(1, 1)], small()).unwrap();
        let one = crate::storage::profile::take();
        assert_eq!(one.presence_builds, 0, "{one:?}");
        assert!(one.index_singles >= 1, "{one:?}");

        let ten = sequential(10);
        let (mut store, root) = fill(production(), &ten);
        assert_eq!(height(&mut store, root).unwrap(), 1);
        crate::storage::profile::reset();
        delete_many(&mut store, root, &interleaved(&ten), production()).unwrap();
        let single_leaf = crate::storage::profile::take();
        assert_eq!(single_leaf.presence_builds, 0, "{single_leaf:?}");

        let mut one_missing = interleaved(&ten);
        one_missing.push((99, 99));
        crate::storage::profile::reset();
        {
            let (mut store, root) = fill(production(), &ten);
            let error = delete_many(&mut store, root, &one_missing, production()).unwrap_err();
            assert!(
                error.to_string().contains("index entry is missing"),
                "{error}"
            );
        }
        let suffix_one = crate::storage::profile::take();
        assert_eq!(suffix_one.presence_builds, 0, "{suffix_one:?}");
        let mut two_missing = interleaved(&ten);
        two_missing.push((99, 99));
        two_missing.push((98, 98));
        delete_verified("height1-two-missing", production(), &ten, &two_missing);
        crate::storage::profile::reset();
        {
            let (mut store, root) = fill(production(), &ten);
            let _ = delete_many(&mut store, root, &two_missing, production());
        }
        let suffix_two = crate::storage::profile::take();
        assert!(suffix_two.presence_builds >= 1, "{suffix_two:?}");
        assert!(suffix_two.map_misses >= 1, "{suffix_two:?}");

        let entries = sequential(8);
        let (mut store, root) = fill(small(), &entries);
        assert_eq!(height(&mut store, root).unwrap(), 2);
        let estimate = estimate_scan(&mut store, root, small()).unwrap();
        assert!(estimate.exact, "{estimate:?}");
        assert_eq!(estimate.height, 2);
        assert!(estimate.pages > 1, "{}", estimate.pages);
        let mixed = interleaved(&entries);
        crate::storage::profile::reset();
        delete_many(&mut store, root, &mixed, small()).unwrap();
        let height_two = crate::storage::profile::take();
        assert!(
            count_u64(mixed.len()) > estimate.pages,
            "mixed {} pages {}",
            mixed.len(),
            estimate.pages
        );
        assert!(height_two.early_plans >= 1, "{height_two:?}");
        assert!(height_two.presence_builds >= 1, "{height_two:?}");
        crate::storage::profile::reset();
        {
            let (mut late_store, late_root) = fill(small(), &entries);
            let _guard = ForceLatePresence::new();
            delete_many(&mut late_store, late_root, &mixed, small()).unwrap();
        }
        let late_two = crate::storage::profile::take();
        assert_eq!(late_two.early_plans, 0, "{late_two:?}");
        assert_eq!(late_two.presence_builds, 0, "{late_two:?}");
        assert!(
            height_two.descents < late_two.descents,
            "early {} late {}",
            height_two.descents,
            late_two.descents
        );
        delete_verified("height2-early", small(), &entries, &mixed);

        let tall = sequential(48);
        let (mut store, root) = fill(small(), &tall);
        let tall_height = height(&mut store, root).unwrap();
        assert!(tall_height >= 3, "{tall_height}");
        let tall_estimate = estimate_scan(&mut store, root, small()).unwrap();
        if tall_height == 3 {
            assert!(tall_estimate.exact, "{tall_estimate:?}");
            assert!(
                tall_estimate.pages < max_index_pages(3),
                "estimate {} ceiling {}",
                tall_estimate.pages,
                max_index_pages(3)
            );
        }
        let mixed = interleaved(&tall);
        crate::storage::profile::reset();
        delete_many(&mut store, root, &mixed, small()).unwrap();
        let automatic = crate::storage::profile::take();
        assert!(automatic.early_plans >= 1, "{automatic:?}");
        assert!(automatic.presence_builds >= 1, "{automatic:?}");
        crate::storage::profile::reset();
        {
            let (mut late_store, late_root) = fill(small(), &tall);
            let _guard = ForceLatePresence::new();
            delete_many(&mut late_store, late_root, &mixed, small()).unwrap();
        }
        let late_tall = crate::storage::profile::take();
        assert_eq!(late_tall.presence_builds, 0, "{late_tall:?}");
        assert!(
            automatic.descents < late_tall.descents,
            "early {} late {}",
            automatic.descents,
            late_tall.descents
        );

        let ascending = sequential(48);
        crate::storage::profile::reset();
        {
            let (mut store, root) = fill(small(), &ascending);
            let _guard = ForcePresenceMap::new();
            delete_many(&mut store, root, &ascending, small()).unwrap();
        }
        let forward = crate::storage::profile::take();
        assert_eq!(forward.presence_builds, 0, "{forward:?}");
        assert!(forward.ordered_verified > 0, "{forward:?}");
        let mut descending = ascending.clone();
        descending.reverse();
        crate::storage::profile::reset();
        {
            let (mut store, root) = fill(small(), &ascending);
            let _guard = ForcePresenceMap::new();
            delete_many(&mut store, root, &descending, small()).unwrap();
        }
        let backward = crate::storage::profile::take();
        assert_eq!(backward.presence_builds, 0, "{backward:?}");
        assert!(backward.ordered_verified > 0, "{backward:?}");

        let wide = sequential(800);
        let (mut probe, root) = fill(production(), &wide);
        assert_eq!(height(&mut probe, root).unwrap(), 2);
        let wide_estimate = estimate_scan(&mut probe, root, production()).unwrap();
        assert!(wide_estimate.exact, "{wide_estimate:?}");
        assert_eq!(wide_estimate.height, 2);
        assert!(
            wide_estimate.pages < max_index_pages(2),
            "{}",
            wide_estimate.pages
        );
        let runs = leaf_runs(&mut probe, root);
        let thin = runs
            .iter()
            .find(|run| run.entries.len() == production().leaf_min)
            .expect("minimum leaf");
        let thin_key = thin.entries[0];
        let foreign = interleaved(
            &wide
                .iter()
                .copied()
                .filter(|entry| !thin.entries.contains(entry))
                .collect::<Vec<_>>(),
        );
        let mut below = vec![thin_key];
        below.extend(foreign.iter().copied().take(102));
        let mut above = vec![thin_key];
        above.extend(foreign.iter().copied().take(103));
        let mut short = vec![thin_key];
        let short_extra = usize::try_from(wide_estimate.pages).unwrap_or(usize::MAX);
        short.extend(foreign.iter().copied().take(short_extra.saturating_sub(2)));
        assert!(count_u64(short.len()) <= wide_estimate.pages);
        crate::storage::profile::reset();
        {
            let (mut store, root) = fill(production(), &wide);
            let _guard = ForceLatePresence::new();
            delete_many(&mut store, root, &below, production()).unwrap();
        }
        let below_profile = crate::storage::profile::take();
        assert_eq!(below_profile.early_plans, 0, "{below_profile:?}");
        assert_eq!(below_profile.presence_builds, 0, "{below_profile:?}");
        assert!(below_profile.fallback_verified > 0, "{below_profile:?}");
        crate::storage::profile::reset();
        {
            let (mut store, root) = fill(production(), &wide);
            let _guard = ForceLatePresence::new();
            delete_many(&mut store, root, &above, production()).unwrap();
        }
        let above_late = crate::storage::profile::take();
        assert!(above_late.presence_builds >= 1, "{above_late:?}");
        assert_eq!(above_late.early_plans, 0, "{above_late:?}");
        crate::storage::profile::reset();
        {
            let (mut store, root) = fill(production(), &wide);
            delete_many(&mut store, root, &below, production()).unwrap();
        }
        let below_early = crate::storage::profile::take();
        assert!(below_early.early_plans >= 1, "{below_early:?}");
        assert!(below_early.presence_builds >= 1, "{below_early:?}");
        assert!(below_early.presence_verified > 0, "{below_early:?}");
        assert_eq!(below_early.fallback_verified, 0, "{below_early:?}");
        assert!(
            below_early.descents < below_profile.descents,
            "early {} late {}",
            below_early.descents,
            below_profile.descents
        );
        crate::storage::profile::reset();
        {
            let (mut store, root) = fill(production(), &wide);
            delete_many(&mut store, root, &short, production()).unwrap();
        }
        let ten_profile = crate::storage::profile::take();
        assert_eq!(ten_profile.presence_builds, 0, "{ten_profile:?}");
        assert_eq!(ten_profile.early_plans, 0, "{ten_profile:?}");
        delete_verified("below-bound", production(), &wide, &below);
        delete_verified("above-bound", production(), &wide, &above);
        delete_verified("ten-postings", production(), &wide, &short);
        let mut repeated = above.clone();
        repeated.insert(3, repeated[1]);
        delete_verified("repeated-pair", production(), &wide, &repeated);
    }

    fn corruption_is_not_written(
        label: &str,
        limits: Limits,
        root: u32,
        damaged: &Mem,
        remove: &[Entry],
    ) {
        let image = page_image(damaged);
        let mut seen_error = None;
        for mode in ["presence", "chain", "descent"] {
            let mut store = damaged.clone();
            let result = match mode {
                "presence" => {
                    let _guard = ForcePresenceMap::new();
                    delete_many(&mut store, root, remove, limits)
                }
                "chain" => {
                    let _guard = ForceChainVerify::new();
                    delete_many(&mut store, root, remove, limits)
                }
                "descent" => {
                    let _guard = ForceDescentVerify::new();
                    delete_many(&mut store, root, remove, limits)
                }
                _ => unreachable!(),
            };
            let error = result.unwrap_err().to_string();
            assert!(error.contains("corrupt"), "{label} {mode}: {error}");
            assert!(
                !error.contains("index entry is missing"),
                "{label} {mode}: {error}"
            );
            if let Some(previous) = &seen_error {
                assert_eq!(previous, &error, "{label} {mode}");
            } else {
                seen_error = Some(error);
            }
            assert_eq!(page_image(&store), image, "{label} {mode}");
        }
    }

    #[test]
    fn presence_map_does_not_repair_a_damaged_tree() {
        let limits = small();
        let entries = sequential(48);
        let (probe, root) = fill(limits, &entries);
        let runs = leaf_runs(&mut probe.clone(), root);
        let thin = runs
            .iter()
            .find(|run| run.id != root && run.entries.len() == limits.leaf_min)
            .expect("thin leaf");
        let thin_key = thin.entries[0];
        let victim = runs
            .iter()
            .find(|run| run.id != thin.id && run.entries.len() >= 2)
            .expect("victim");
        let victim_id = victim.id;
        let mut remove = vec![thin_key];
        remove.extend(interleaved(&without(&entries, &[thin_key])));

        let mut typed = probe.clone();
        let mut page = typed.read_node(victim_id).unwrap();
        page.as_mut_slice()[0] = 0;
        typed.write_node(victim_id, page).unwrap();
        corruption_is_not_written("type", limits, root, &typed, &remove);

        let mut back = probe.clone();
        let mut leaf = decode_leaf(&back.read_node(victim_id).unwrap()).unwrap();
        leaf.left = victim_id;
        back.write_node(victim_id, encode_leaf(victim_id, &leaf).unwrap())
            .unwrap();
        corruption_is_not_written("back-pointer", limits, root, &back, &remove);

        let mut order = probe.clone();
        let mut leaf = decode_leaf(&order.read_node(victim_id).unwrap()).unwrap();
        leaf.entries.swap(0, 1);
        order
            .write_node(victim_id, encode_leaf(victim_id, &leaf).unwrap())
            .unwrap();
        corruption_is_not_written("order", limits, root, &order, &remove);

        let mut cycle = probe.clone();
        let mut page = cycle.read_node(victim_id).unwrap();
        crate::storage::page::set_next_page(&mut page, victim_id);
        cycle.write_node(victim_id, page).unwrap();
        corruption_is_not_written("cycle", limits, root, &cycle, &remove);

        agree_damaged(limits, &entries, &remove, &mut |store, root_id| {
            let mut node = decode_internal(&store.read_node(root_id).unwrap()).unwrap();
            node.keys[0] = (i64::MIN, 0);
            store
                .write_node(root_id, encode_internal(root_id, &node).unwrap())
                .unwrap();
        });

        agree_damaged(limits, &entries, &remove, &mut |store, root_id| {
            let mut node = decode_internal(&store.read_node(root_id).unwrap()).unwrap();
            node.children[0] = 0;
            store
                .write_node(root_id, encode_internal(root_id, &node).unwrap())
                .unwrap();
        });

        let live = victim_id;
        let mut presence = probe.clone();
        let mut chain = probe.clone();
        let mut descent = probe.clone();
        presence.free.push(live);
        chain.free.push(live);
        descent.free.push(live);
        let presence_result = {
            let _guard = ForcePresenceMap::new();
            delete_many(&mut presence, root, &remove, limits)
        };
        let chain_result = {
            let _guard = ForceChainVerify::new();
            delete_many(&mut chain, root, &remove, limits)
        };
        let descent_result = {
            let _guard = ForceDescentVerify::new();
            delete_many(&mut descent, root, &remove, limits)
        };
        match (presence_result, chain_result, descent_result) {
            (Ok(left), Ok(middle), Ok(right)) => {
                assert!(presence.free.contains(&live) && presence.pages.contains_key(&live));
                assert_eq!(page_image(&presence), page_image(&descent));
                assert_eq!(page_image(&chain), page_image(&descent));
                assert_eq!(
                    validate(&mut presence, left, limits).unwrap(),
                    validate(&mut descent, right, limits).unwrap()
                );
                assert_eq!(
                    validate(&mut chain, middle, limits).unwrap(),
                    validate(&mut descent, right, limits).unwrap()
                );
            }
            (Err(left), Err(middle), Err(right)) => {
                assert_eq!(left.to_string(), right.to_string());
                assert_eq!(middle.to_string(), right.to_string());
                assert_eq!(page_image(&presence), page_image(&descent));
                assert_eq!(page_image(&chain), page_image(&descent));
            }
            (left_result, middle, right_result) => {
                panic!("free-list split {left_result:?} {middle:?} {right_result:?}")
            }
        }

        let mut seen = None;
        for mode in ["presence", "chain", "descent"] {
            let mut store = probe.clone();
            store.free.push(90_000);
            store.free.push(90_000);
            let result = match mode {
                "presence" => {
                    let _guard = ForcePresenceMap::new();
                    delete_many(&mut store, root, &remove, limits)
                }
                "chain" => {
                    let _guard = ForceChainVerify::new();
                    delete_many(&mut store, root, &remove, limits)
                }
                "descent" => {
                    let _guard = ForceDescentVerify::new();
                    delete_many(&mut store, root, &remove, limits)
                }
                _ => unreachable!(),
            };
            let image = page_image(&store);
            let copies = store.free.iter().filter(|id| **id == 90_000).count();
            if result.is_ok() {
                assert!(copies >= 2, "{mode} repaired a duplicated free-list id");
            }
            if let Some((previous_image, previous_copies, previous_ok)) = &seen {
                assert_eq!(previous_image, &image, "{mode}");
                assert_eq!(*previous_copies, copies, "{mode}");
                assert_eq!(*previous_ok, result.is_ok(), "{mode}");
            } else {
                seen = Some((image, copies, result.is_ok()));
            }
        }
    }
}
