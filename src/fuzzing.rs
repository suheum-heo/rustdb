#![cfg_attr(not(feature = "fuzz"), allow(dead_code))]

//! Byte-oriented entry points for local fuzzing.
//!
//! Each function stops at 64 KiB so a harness cannot ask the decoder to
//! allocate from an unbounded length prefix. Errors are returned to the
//! caller. A panic is a bug. This is not a claim about coverage from a short
//! run.

use crate::parser;
use crate::storage::{self, LogClass, PAGE_SIZE, Page};

const FUZZ_LIMIT: usize = 64 * 1024;

pub fn fuzz_decode_record(bytes: &[u8]) {
    let bytes = limit(bytes);
    let _ = storage::decode_record(bytes);
    let _ = storage::decode_catalog(bytes);
    let _ = storage::decode_index(bytes);
}

pub fn fuzz_decode_page(bytes: &[u8]) {
    let bytes = limit(bytes);
    let mut page = Page::new();
    let copy = bytes.len().min(PAGE_SIZE);
    page.as_mut_slice()[..copy].copy_from_slice(&bytes[..copy]);
    if storage::verify(&page, storage::page_id_of(&page)).is_ok()
        && let Ok(slots) = storage::live_slots(&page)
    {
        for slot in slots {
            let _ = storage::cell(&page, slot);
        }
    }
}

pub fn fuzz_classify_wal(bytes: &[u8]) -> u8 {
    match storage::classify_log(limit(bytes)) {
        LogClass::Incomplete => 0,
        LogClass::Committed => 1,
        LogClass::Damaged => 2,
    }
}

pub fn fuzz_parse_sql(bytes: &[u8]) {
    let bytes = &limit(bytes)[..bytes.len().min(8 * 1024)];
    let text = String::from_utf8_lossy(bytes);
    let _ = parser::parse(&text);
}

fn limit(bytes: &[u8]) -> &[u8] {
    let end = bytes.len().min(FUZZ_LIMIT);
    &bytes[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_inputs_do_not_panic() {
        let samples: &[&[u8]] = &[
            b"",
            b"\0",
            b"RUSTWAL\0",
            b"SELECT",
            &[0xff; 64],
            b"not sql \xff\xfe",
        ];
        for sample in samples {
            fuzz_decode_record(sample);
            fuzz_decode_page(sample);
            let _ = fuzz_classify_wal(sample);
            fuzz_parse_sql(sample);
        }
        let mut wide = vec![0u8; FUZZ_LIMIT + 32];
        wide[0] = 4;
        fuzz_decode_record(&wide);
        let _ = fuzz_classify_wal(&wide);
    }
}
