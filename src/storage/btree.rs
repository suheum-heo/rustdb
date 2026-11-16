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
use std::collections::HashSet;

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
        let id = store.alloc_node()?;
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
            let id = store.alloc_node()?;
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
    super::profile::note_index_single();
    match delete_at(store, root, (key, row_id), true, limits, 0)? {
        Removed::Empty => {
            store.free_node(root)?;
            Ok(0)
        }
        Removed::Present { .. } | Removed::Underfull { .. } => Ok(root),
        Removed::Collapsed { child, .. } => {
            store.free_node(root)?;
            Ok(child)
        }
    }
}

/// Delete `entries` in order. The final pages match calling [`delete`] once
/// per entry.
///
/// A run that sits in one leaf and leaves that leaf at or above its minimum is
/// removed in one rewrite. The ancestors on that path are rewritten once, with
/// the same separator a series of single deletes would finish on. A removal
/// that would go under the minimum, borrow, or merge uses [`delete`], so the
/// tree shape does not change. A missing pair returns the same error as
/// [`delete`] and does not continue.
pub(crate) fn delete_many(
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
        // Count a slot once. A second copy of the same pair ends the run, so
        // the next pass reports it missing after the first copy is removed.
        // The length used below is this leaf's, just decoded.
        let mut claimed = vec![false; leaf.entries.len()];
        let mut run = 0;
        while cursor + run < entries.len() {
            let wanted = entries[cursor + run];
            let Some(slot) = leaf
                .entries
                .iter()
                .enumerate()
                .position(|(index, entry)| !claimed[index] && *entry == wanted)
            else {
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
    if root == 0 {
        return Ok(Vec::new());
    }
    let mut seen = Vec::new();
    let mut leaves = Vec::new();
    let _ = check_node(store, root, true, limits, &mut seen, &mut leaves, 0)?;
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
    Ok(flat)
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
    seen: &mut Vec<u32>,
    leaves: &mut Vec<u32>,
    depth: u32,
) -> Result<Entry, DbError> {
    if depth >= MAX_DEPTH || !seen.iter().all(|page| *page != id) {
        return Err(corrupt(id));
    }
    seen.push(id);
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
            leaves.push(id);
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
                let minimum = check_node(store, child, false, limits, seen, leaves, depth + 1)?;
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
        let right_id = store.alloc_node()?;
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
    let right_id = store.alloc_node()?;
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
        store.free_node(right_id)?;
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
    store.free_node(id)?;
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
        store.free_node(right_id)?;
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
    store.free_node(id)?;
    Ok(())
}

fn path_to_leaf(
    store: &mut dyn Nodes,
    root: u32,
    probe: Entry,
) -> Result<(Vec<(u32, usize)>, u32), DbError> {
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
        assert_eq!(single_root, batched_root);
        assert_eq!(page_image(&single), page_image(&batched));
        assert_eq!(single.free, batched.free);
        let single_entries = validate(&mut single, single_root, limits).unwrap();
        let batched_entries = validate(&mut batched, batched_root, limits).unwrap();
        assert_eq!(single_entries, batched_entries);
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
            assert!(guard < 64, "leaf chain");
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

    /// Pages, the free list, and the validated entry list match one-at-a-time
    /// deletion. A missing pair stops both sides on the same error and the
    /// same pages.
    fn same_tree(limits: Limits, entries: &[Entry], remove: &[Entry]) {
        let (mut single, single_root) = fill(limits, entries);
        let (mut batched, batched_root) = fill(limits, entries);
        let single_result = drive(&mut single, single_root, remove, limits);
        let batched_result = delete_many(&mut batched, batched_root, remove, limits);
        match (single_result, batched_result) {
            (Ok(left), Ok(right)) => {
                assert_eq!(left, right);
                assert_eq!(page_image(&single), page_image(&batched));
                assert_eq!(single.free, batched.free);
                assert_eq!(
                    height(&mut single, left).unwrap(),
                    height(&mut batched, right).unwrap()
                );
                assert_eq!(
                    validate(&mut single, left, limits).unwrap(),
                    validate(&mut batched, right, limits).unwrap()
                );
            }
            (Err(left), Err(right)) => {
                assert_eq!(left.to_string(), right.to_string());
                assert_eq!(page_image(&single), page_image(&batched));
                assert_eq!(single.free, batched.free);
            }
            (Ok(_), Err(error)) => panic!("batch failed after single deletes succeeded: {error}"),
            (Err(error), Ok(_)) => panic!("batch succeeded after single deletes failed: {error}"),
        }
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
        same_tree(limits, &entries, &fat.entries[..room]);
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
        } else {
            assert_eq!(profile.leaf_batches, 0);
            assert_eq!(profile.index_singles, u64::try_from(room + 1).unwrap());
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
        same_tree(limits, &wide, &fat.entries[..room]);
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
}
