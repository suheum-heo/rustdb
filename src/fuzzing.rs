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
    let limited = limit(bytes);
    let end = limited.len().min(8 * 1024);
    let text = String::from_utf8_lossy(&limited[..end]);
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

    #[test]
    fn valid_pages_and_logs_reach_the_success_path() {
        let record = storage::encode_record(&storage::StoredRecord {
            row_id: 1,
            prev: storage::NULL_LINK,
            next: storage::NULL_LINK,
            values: vec![
                crate::value::Value::Integer(1),
                crate::value::Value::Null,
                crate::value::Value::Boolean(true),
            ],
        })
        .unwrap();
        fuzz_decode_record(&record);
        assert!(storage::decode_record(&record).is_ok());

        let page = sealed_page();
        fuzz_decode_page(&page);
        let mut stored = storage::Page::new();
        stored.as_mut_slice().copy_from_slice(&page);
        assert!(storage::verify(&stored, storage::page_id_of(&stored)).is_ok());

        let log = committed_log(&stored);
        assert_eq!(fuzz_classify_wal(&log), 1);
        assert_eq!(fuzz_classify_wal(&log[..log.len() - 10]), 0);
        fuzz_parse_sql(b"SELECT name FROM users WHERE id = 1;");
    }

    /// Prefix of the mutation campaign. The longer run is ignored so CI stays short.
    #[test]
    fn mutation_prefix_does_not_panic() {
        for target in 0..4 {
            let stats = drive(target, seed_for(target), Drive::Count(4_096));
            assert_eq!(
                stats.crashes, 0,
                "target {target} input {:02x?}",
                stats.crash
            );
            assert_eq!(stats.execs, 4_096);
        }
    }

    /// Deterministic mutation campaign. This is not libFuzzer.
    ///
    /// Set `RUSTDB_MUTATION_SECS` to the wall-clock budget for each target.
    /// Without that variable the test returns immediately, so `--ignored`
    /// release profiles stay separate from a campaign. A panic fails the test
    /// and prints the input bytes.
    #[test]
    #[ignore = "local mutation campaign; set RUSTDB_MUTATION_SECS; not libFuzzer"]
    fn mutation_campaign() {
        let Ok(seconds) = std::env::var("RUSTDB_MUTATION_SECS") else {
            return;
        };
        let Ok(seconds) = seconds.parse::<u64>() else {
            return;
        };
        if seconds == 0 {
            return;
        }
        let budget = std::time::Duration::from_secs(seconds);
        for target in 0..4 {
            let stats = drive(target, seed_for(target), Drive::Duration(budget));
            let elapsed_ms = stats.elapsed.as_millis().max(1);
            let per_sec = u128::from(stats.execs) * 1000 / elapsed_ms;
            println!(
                "campaign target={} seed={:#x} elapsed_ms={} execs={} execs_per_sec={} crashes={} timeouts={} seed_inputs={} max_input={}",
                target_name(target),
                seed_for(target),
                elapsed_ms,
                stats.execs,
                per_sec,
                stats.crashes,
                stats.timeouts,
                stats.seed_inputs,
                stats.max_input
            );
            assert_eq!(
                stats.crashes,
                0,
                "target {} input {:02x?}",
                target_name(target),
                stats.crash
            );
        }
    }

    fn target_name(target: u8) -> &'static str {
        match target {
            0 => "record_decode",
            1 => "page_decode",
            2 => "wal_decode",
            _ => "sql_parse",
        }
    }

    fn seed_for(target: u8) -> u64 {
        0x1701_0000_0017 ^ (u64::from(target).wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    enum Drive {
        Count(u64),
        Duration(std::time::Duration),
    }

    struct Stats {
        execs: u64,
        elapsed: std::time::Duration,
        crashes: u64,
        timeouts: u64,
        seed_inputs: usize,
        max_input: usize,
        crash: Vec<u8>,
    }

    fn drive(target: u8, seed: u64, drive: Drive) -> Stats {
        let bases = seed_inputs();
        let mut rng = Lcg(seed);
        let mut buf = Vec::with_capacity(FUZZ_LIMIT);
        let started = std::time::Instant::now();
        let mut execs = 0u64;
        let mut max_input = 0usize;
        loop {
            match drive {
                Drive::Count(limit) if execs >= limit => break,
                Drive::Duration(budget) if started.elapsed() >= budget => break,
                _ => {}
            }
            mutate(&mut rng, &bases, &mut buf);
            max_input = max_input.max(buf.len());
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                exercise(target, &buf);
            }));
            execs += 1;
            if panicked.is_err() {
                return Stats {
                    execs,
                    elapsed: started.elapsed(),
                    crashes: 1,
                    timeouts: 0,
                    seed_inputs: bases.len(),
                    max_input,
                    crash: buf,
                };
            }
        }
        Stats {
            execs,
            elapsed: started.elapsed(),
            crashes: 0,
            timeouts: 0,
            seed_inputs: bases.len(),
            max_input,
            crash: Vec::new(),
        }
    }

    fn exercise(target: u8, bytes: &[u8]) {
        match target {
            0 => fuzz_decode_record(bytes),
            1 => fuzz_decode_page(bytes),
            2 => {
                let _ = fuzz_classify_wal(bytes);
            }
            _ => fuzz_parse_sql(bytes),
        }
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            self.0
        }
    }

    fn mutate(rng: &mut Lcg, bases: &[Vec<u8>], out: &mut Vec<u8>) {
        out.clear();
        let base = &bases[rng.next() as usize % bases.len()];
        let op = rng.next() % 8;
        if op == 0 || base.is_empty() {
            return;
        }
        let keep = if op == 1 {
            rng.next() as usize % (base.len() + 1)
        } else {
            base.len()
        };
        out.extend_from_slice(&base[..keep.min(FUZZ_LIMIT)]);
        if out.is_empty() {
            return;
        }
        match op {
            2 | 3 => {
                let index = rng.next() as usize % out.len();
                let bit = 1u8 << (rng.next() % 8);
                if op == 2 {
                    out[index] ^= bit;
                } else {
                    out[index] = rng.next() as u8;
                }
            }
            4 => {
                out.remove(rng.next() as usize % out.len());
            }
            5 => {
                let index = rng.next() as usize % out.len();
                out[index] = 0xff;
                if index + 1 < out.len() {
                    out[index + 1] = 0xff;
                }
            }
            6 => {
                let extra = out.len().min(64);
                out.extend_from_within(..extra);
            }
            _ => {
                let index = rng.next() as usize % (out.len() + 1);
                out.insert(index, rng.next() as u8);
            }
        }
        if out.len() > FUZZ_LIMIT {
            out.truncate(FUZZ_LIMIT);
        }
    }

    fn seed_inputs() -> Vec<Vec<u8>> {
        let record = storage::encode_record(&storage::StoredRecord {
            row_id: 7,
            prev: storage::NULL_LINK,
            next: storage::NULL_LINK,
            values: vec![
                crate::value::Value::Integer(i64::MIN),
                crate::value::Value::Text("Ada".to_string()),
                crate::value::Value::Boolean(false),
                crate::value::Value::Null,
            ],
        })
        .unwrap();
        let page = sealed_page();
        let mut stored = storage::Page::new();
        stored.as_mut_slice().copy_from_slice(&page);
        let log = committed_log(&stored);
        let mut claimed = vec![0u8; 22];
        claimed[20] = 0xff;
        claimed[21] = 0xff;
        let mut inputs = vec![
            Vec::new(),
            vec![0],
            vec![0xff; 64],
            claimed,
            record,
            page,
            log.clone(),
            log[..log.len().min(120)].to_vec(),
            b"SELECT".to_vec(),
            b"SELECT name FROM users WHERE id = 1;".to_vec(),
            b"INSERT INTO users VALUES (1, 'Ada');".to_vec(),
            "SELECT '".as_bytes().to_vec(),
            b"\xff\xfe not utf8".to_vec(),
        ];
        inputs.push(vec![0u8; FUZZ_LIMIT]);
        inputs
    }

    fn sealed_page() -> Vec<u8> {
        let mut page = storage::Page::new();
        storage::init_slotted(&mut page, storage::TYPE_HEAP, 2);
        let record = storage::encode_record(&storage::StoredRecord {
            row_id: 1,
            prev: storage::NULL_LINK,
            next: storage::NULL_LINK,
            values: vec![crate::value::Value::Integer(1)],
        })
        .unwrap();
        storage::insert_cell(&mut page, &record).unwrap();
        storage::seal(&mut page);
        page.as_slice().to_vec()
    }

    fn committed_log(page: &storage::Page) -> Vec<u8> {
        storage::encode_committed(&storage::Plan {
            nonce: 1,
            base_commit: 0,
            frames: &[(2, page.clone())],
        })
        .unwrap()
    }
}
