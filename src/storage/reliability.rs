//! Property tests for the page codec, redo log, and B+ tree.
//!
//! The default runner uses 64 cases. `PROPTEST_CASES` overrides that. These
//! tests sit beside the fixed regression tests. They do not replace them.

use std::collections::{BTreeMap, HashMap};

use proptest::prelude::*;
use proptest::test_runner::Config;

use super::btree::{self, Entry, Limits, Nodes};
use super::page::{self, MAX_RECORD_LEN, PAGE_SIZE, Page};
use super::record::{self, NULL_LINK, StoredRecord};
use super::wal::{self, LogClass};
use crate::error::DbError;
use crate::value::Value;

fn config() -> Config {
    Config::with_cases(64)
}

fn stored_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<i64>().prop_map(Value::Integer),
        any::<bool>().prop_map(Value::Boolean),
        "[a-zA-Z0-9 ]{0,24}".prop_map(Value::Text),
    ]
}

fn record_strategy() -> impl Strategy<Value = StoredRecord> {
    (1u64..10_000, prop::collection::vec(stored_value(), 0..6)).prop_map(|(row_id, values)| {
        StoredRecord {
            row_id,
            prev: NULL_LINK,
            next: NULL_LINK,
            values,
        }
    })
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn records_round_trip_or_reject_overflow(record in record_strategy()) {
        match record::encode_record(&record) {
            Ok(bytes) => {
                let decoded = record::decode_record(&bytes).expect("encoded record");
                prop_assert_eq!(decoded, record);
            }
            Err(error) => {
                let message = error.to_string();
                prop_assert!(
                    message.contains("maximum record size") || message.contains("not stored"),
                    "{message}"
                );
            }
        }
    }

    #[test]
    fn malformed_records_do_not_panic(bytes in prop::collection::vec(any::<u8>(), 0..96)) {
        let _ = record::decode_record(&bytes);
        let _ = record::decode_catalog(&bytes);
        let _ = record::decode_index(&bytes);
    }

    #[test]
    fn arbitrary_pages_do_not_panic(bytes in prop::collection::vec(any::<u8>(), 0..PAGE_SIZE)) {
        let mut page = Page::new();
        page.as_mut_slice()[..bytes.len()].copy_from_slice(&bytes);
        let id = page::page_id_of(&page);
        let _ = page::verify(&page, id);
        if let Ok(slots) = page::live_slots(&page) {
            for slot in slots {
                let _ = page::cell(&page, slot);
            }
        }
    }

    #[test]
    fn heap_cells_round_trip(payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..48), 1..6)) {
        let mut page = Page::new();
        page::init_slotted(&mut page, page::TYPE_HEAP, 3);
        let mut slots = Vec::new();
        for payload in &payloads {
            match page::insert_cell(&mut page, payload) {
                Ok(slot) => slots.push((slot, payload.clone())),
                Err(page::InsertError::Full | page::InsertError::TooBig) => break,
            }
        }
        page::seal(&mut page);
        page::verify(&page, 3).expect("sealed heap page");
        for (slot, payload) in &slots {
            prop_assert_eq!(page::cell(&page, *slot).expect("cell"), payload.as_slice());
        }
    }

    #[test]
    fn wal_bytes_are_classified_without_panic(bytes in prop::collection::vec(any::<u8>(), 0..128)) {
        let class = wal::classify_log(&bytes);
        if bytes.len() < 96 {
            prop_assert!(!matches!(class, LogClass::Committed));
        }
    }

    #[test]
    fn random_index_edits_match_a_sorted_map(
        ops in prop::collection::vec((any::<i16>(), 1u64..40, any::<bool>()), 1..48)
    ) {
        let limits = Limits {
            leaf_max: 4,
            leaf_min: 2,
            internal_max: 5,
            internal_min: 2,
        };
        let mut store = Mem::new();
        let mut root = 0u32;
        let mut model: BTreeMap<Entry, ()> = BTreeMap::new();
        for (key, row_id, insert) in ops {
            let key = i64::from(key);
            if insert {
                if model.contains_key(&(key, row_id)) {
                    continue;
                }
                root = btree::insert(&mut store, root, key, row_id, limits).expect("insert");
                model.insert((key, row_id), ());
            } else if model.remove(&(key, row_id)).is_some() {
                root = btree::delete(&mut store, root, key, row_id, limits).expect("delete");
            }
            let entries = btree::validate(&mut store, root, limits).expect("validate");
            let expected: Vec<Entry> = model.keys().copied().collect();
            prop_assert_eq!(entries, expected);
        }
        if let Some(&(key, _)) = model.keys().next() {
            let found = btree::find_equal(&mut store, root, key).expect("equal");
            let want: Vec<u64> = model
                .keys()
                .filter(|entry| entry.0 == key)
                .map(|entry| entry.1)
                .collect();
            prop_assert_eq!(found, want);
            let ranged = btree::find_range(&mut store, root, Some(key), Some(key + 3)).expect("range");
            let want: Vec<Entry> = model
                .keys()
                .copied()
                .filter(|entry| entry.0 >= key && entry.0 <= key + 3)
                .collect();
            prop_assert_eq!(ranged, want);
        }
    }
}

#[test]
fn an_oversized_text_value_is_rejected() {
    let record = StoredRecord {
        row_id: 1,
        prev: NULL_LINK,
        next: NULL_LINK,
        values: vec![Value::Text("x".repeat(MAX_RECORD_LEN))],
    };
    let error = record::encode_record(&record).unwrap_err();
    assert!(error.to_string().contains("maximum record size"));
}

#[test]
fn unknown_record_tags_and_truncated_text_are_rejected() {
    let mut bytes = vec![0u8; 22];
    bytes.extend(1u16.to_le_bytes());
    bytes.push(9);
    assert!(record::decode_record(&bytes).is_err());

    let mut truncated = vec![0u8; 22];
    truncated.extend(1u16.to_le_bytes());
    truncated.push(2);
    truncated.extend(20u32.to_le_bytes());
    truncated.extend(b"short");
    assert!(record::decode_record(&truncated).is_err());
    assert!(record::decode_record(&[0u8; 4]).is_err());
}

#[test]
fn a_checksum_mismatch_and_a_bad_slot_are_rejected() {
    let mut page = Page::new();
    page::init_slotted(&mut page, page::TYPE_HEAP, 4);
    let slot = page::insert_cell(&mut page, b"ada").unwrap();
    page::seal(&mut page);
    page::verify(&page, 4).unwrap();
    page.as_mut_slice()[page::OFF_CHECKSUM] ^= 0xff;
    assert!(page::verify(&page, 4).is_err());

    page.as_mut_slice()[page::OFF_CHECKSUM] ^= 0xff;
    page::seal(&mut page);
    let directory = page::HEADER_LEN;
    page.as_mut_slice()[directory..directory + 2].copy_from_slice(&1u16.to_le_bytes());
    page.as_mut_slice()[directory + 2..directory + 4].copy_from_slice(&10u16.to_le_bytes());
    page::seal(&mut page);
    assert!(page::verify(&page, 4).is_err());
    let _ = page::cell(&page, slot);
}

#[test]
fn null_integer_text_and_boolean_round_trip() {
    let record = StoredRecord {
        row_id: 7,
        prev: NULL_LINK,
        next: NULL_LINK,
        values: vec![
            Value::Null,
            Value::Integer(i64::MIN),
            Value::Text("Case".to_string()),
            Value::Boolean(false),
        ],
    };
    let bytes = record::encode_record(&record).unwrap();
    assert_eq!(record::decode_record(&bytes).unwrap(), record);
}

#[test]
fn a_committed_log_classifies_as_committed_and_damage_is_kept() {
    let mut page = Page::new();
    page::init_slotted(&mut page, page::TYPE_HEAP, 2);
    page::insert_cell(&mut page, b"row").unwrap();
    page::seal(&mut page);
    let bytes = wal::encode_committed(&wal::Plan {
        nonce: 1,
        base_commit: 0,
        frames: &[(2, page)],
    })
    .unwrap();
    assert!(matches!(wal::classify_log(&bytes), LogClass::Committed));
    assert!(matches!(
        wal::classify_log(&bytes[..bytes.len() - 1]),
        LogClass::Incomplete
    ));
    let mut damaged = bytes.clone();
    let checksum_at = damaged.len() - 32 + 12;
    damaged[checksum_at] ^= 0xff;
    assert!(matches!(wal::classify_log(&damaged), LogClass::Damaged));
    assert!(matches!(wal::classify_log(&[]), LogClass::Incomplete));
    assert!(matches!(wal::classify_log(b"nope"), LogClass::Incomplete));
}

#[test]
fn proptest_honors_proptest_cases() {
    let applied = proptest::test_runner::contextualize_config(config());
    let expected = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(64);
    assert_eq!(applied.cases, expected);
}

#[test]
fn a_huge_slot_count_fails_validation_without_overflowing() {
    let mut page = Page::new();
    page::init_slotted(&mut page, page::TYPE_HEAP, 1);
    page.as_mut_slice()[page::OFF_SLOTS..page::OFF_SLOTS + 2]
        .copy_from_slice(&u16::MAX.to_le_bytes());
    page::seal(&mut page);
    assert!(page::verify(&page, 1).is_err());
    let _ = page::live_slots(&page);
}

#[test]
fn a_partial_wal_frame_is_not_committed_and_a_bad_length_is_damaged() {
    let mut page = Page::new();
    page::init_slotted(&mut page, page::TYPE_HEAP, 2);
    page::insert_cell(&mut page, b"row").unwrap();
    page::seal(&mut page);
    let bytes = wal::encode_committed(&wal::Plan {
        nonce: 1,
        base_commit: 0,
        frames: &[(2, page)],
    })
    .unwrap();
    assert!(matches!(
        wal::classify_log(&bytes[..64]),
        LogClass::Incomplete
    ));
    assert!(matches!(
        wal::classify_log(&bytes[..74]),
        LogClass::Incomplete
    ));

    let mut bad_count = bytes.clone();
    let count_at = bad_count.len() - 32 + 8;
    bad_count[count_at..count_at + 4].copy_from_slice(&99u32.to_le_bytes());
    assert!(matches!(wal::classify_log(&bad_count), LogClass::Damaged));

    let mut overflow = bytes.clone();
    overflow[count_at..count_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(wal::classify_log(&overflow), LogClass::Damaged));

    let mut version = bytes;
    version[8..12].copy_from_slice(&99u32.to_le_bytes());
    assert!(matches!(wal::classify_log(&version), LogClass::Damaged));
}

#[test]
fn a_wal_frame_payload_is_not_trusted_when_its_page_checksum_is_wrong() {
    let mut page = Page::new();
    page::init_slotted(&mut page, page::TYPE_HEAP, 2);
    page::insert_cell(&mut page, b"row").unwrap();
    page::seal(&mut page);
    page.as_mut_slice()[page::PAGE_SIZE - 1] ^= 0xff;
    let bytes = wal::encode_committed(&wal::Plan {
        nonce: 1,
        base_commit: 0,
        frames: &[(2, page)],
    })
    .unwrap();
    assert!(matches!(wal::classify_log(&bytes), LogClass::Damaged));
    let mut recovered = Page::new();
    recovered
        .as_mut_slice()
        .copy_from_slice(&bytes[68..68 + PAGE_SIZE]);
    assert!(page::verify(&recovered, 2).is_err());
    assert!(page::validate_fresh_seal(&recovered, 2).is_ok());
}

#[test]
fn a_reduced_fanout_tree_splits_an_internal_node_and_keeps_duplicate_keys() {
    // leaf_max 4 is not the on-disk fanout. Production leaf splits are covered
    // by the store tests that insert 255 keys.
    let limits = Limits {
        leaf_max: 4,
        leaf_min: 2,
        internal_max: 4,
        internal_min: 2,
    };
    let mut store = Mem::new();
    let mut root = 0;
    let mut model = BTreeMap::new();
    for row_id in [1u64, 3, 2] {
        root = btree::insert(&mut store, root, 7, row_id, limits).unwrap();
        model.insert((7, row_id), ());
    }
    assert_eq!(
        btree::find_equal(&mut store, root, 7).unwrap(),
        vec![1, 2, 3]
    );

    for key in 0..40 {
        let row_id = 10 + key as u64;
        root = btree::insert(&mut store, root, key, row_id, limits).unwrap();
        model.insert((key, row_id), ());
    }
    assert!(
        btree::height(&mut store, root).unwrap() >= 3,
        "this reduced tree did not split an internal node"
    );
    assert_eq!(
        btree::validate(&mut store, root, limits).unwrap(),
        model.keys().copied().collect::<Vec<_>>()
    );

    let doomed: Vec<Entry> = model
        .keys()
        .copied()
        .filter(|entry| entry.0 % 2 == 0)
        .collect();
    for entry in doomed {
        root = btree::delete(&mut store, root, entry.0, entry.1, limits).unwrap();
        model.remove(&entry);
    }
    assert_eq!(
        btree::validate(&mut store, root, limits).unwrap(),
        model.keys().copied().collect::<Vec<_>>()
    );
    let ranged = btree::find_range(&mut store, root, Some(1), Some(5)).unwrap();
    let want: Vec<Entry> = model
        .keys()
        .copied()
        .filter(|entry| (1..=5).contains(&entry.0))
        .collect();
    assert_eq!(ranged, want);

    store.write_node(1, Page::new()).unwrap();
    assert!(btree::validate(&mut store, root, limits).is_err());
}

#[test]
fn a_zero_page_in_place_of_a_leaf_is_rejected() {
    let limits = Limits {
        leaf_max: 4,
        leaf_min: 2,
        internal_max: 5,
        internal_min: 2,
    };
    let mut store = Mem::new();
    let mut root = 0;
    for row_id in 1..8u64 {
        root = btree::insert(
            &mut store,
            root,
            i64::try_from(row_id).unwrap(),
            row_id,
            limits,
        )
        .unwrap();
    }
    btree::validate(&mut store, root, limits).unwrap();
    store.write_node(root, Page::new()).unwrap();
    assert!(btree::validate(&mut store, root, limits).is_err());
    assert!(btree::find_equal(&mut store, root, 1).is_err());
}

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
        self.pages
            .get(&id)
            .cloned()
            .ok_or_else(|| page::corrupt(id))
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
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| DbError::Internal("index page id space is exhausted".to_string()))?;
        Ok(id)
    }

    fn free_node(&mut self, id: u32) -> Result<(), DbError> {
        if self.free.contains(&id) || !self.pages.contains_key(&id) {
            return Err(page::corrupt(id));
        }
        self.pages.remove(&id);
        self.free.push(id);
        Ok(())
    }
}
