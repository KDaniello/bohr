//! Arbitrary text offered to the record parser.
//!
//! Records come from other builds and languages. A malformed line is dropped and counted; the
//! parser never panics, allocates without bound or hangs.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    for line in text.lines() {
        let Ok(record) = bohr_sdk_proto::Record::parse_line(line) else {
            continue;
        };

        // A record survives a write and read back, as redaction does.
        let written = record.to_line();
        let again = bohr_sdk_proto::Record::parse_line(&written)
            .expect("a line this crate wrote must be one it can read");
        assert_eq!(record, again, "writing a record back changed what it says\n  {line}");

        // A second write equals the first.
        assert_eq!(written, again.to_line(), "the written form did not settle\n  {line}");
    }
});
