#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    rustdb::fuzzing::fuzz_parse_sql(bytes);
});
