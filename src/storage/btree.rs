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

/// Row ids for one integer key, in row-id order.
#[cfg_attr(not(test), allow(dead_code))]
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
        for entry in &leaf.entries {
            if entry.0 == key {
                out.push(entry.1);
            } else if entry.0 > key && !out.is_empty() {
                return Ok(out);
            }
        }
        if leaf.right == 0 {
            break;
        }
        leaf_id = leaf.right;
    }
    Ok(out)
}

/// Inclusive integer range in key order, then row id.
#[cfg_attr(not(test), allow(dead_code))]
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
    let start = match low {
        Some(key) => (key, 0),
        None => leftmost(store, root)?,
    };
    let mut leaf_id = match low {
        Some(key) => find_leaf(store, root, (key, 0))?,
        None => leftmost_leaf(store, root)?,
    };
    let _ = start;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(leaf_id) {
            return Err(corrupt(leaf_id));
        }
        let leaf = decode_leaf(&store.read_node(leaf_id)?)?;
        for entry in &leaf.entries {
            if low.is_some_and(|key| entry.0 < key) {
                continue;
            }
            if high.is_some_and(|key| entry.0 > key) {
                return Ok(out);
            }
            out.push(*entry);
        }
        if leaf.right == 0 {
            break;
        }
        leaf_id = leaf.right;
    }
    Ok(out)
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

#[cfg_attr(not(test), allow(dead_code))]
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
}
