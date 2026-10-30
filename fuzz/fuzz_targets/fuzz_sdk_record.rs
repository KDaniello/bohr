//! Arbitrary text offered to the record parser.
//!
//! Records are written by somebody else's build, in a language we did not compile, and reach the
//! supervisor through a guest's own writes. A malformed line is dropped and counted by design —
//! what must never happen is a panic, an unbounded allocation, or a parser that does not return.
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

        // **What a record says must survive being written back.** Redaction reads every record it
        // keeps and writes it out again, so a field lost between the two is a field the bundle no
        // longer holds — and the loss is silent, because the line that comes out is still valid.
        // Asserting the second reading equals the first is what makes that loss a crash here
        // instead of a gap somebody notices in a report months later.
        let written = record.to_line();
        let again = bohr_sdk_proto::Record::parse_line(&written)
            .expect("a line this crate wrote must be one it can read");
        assert_eq!(record, again, "writing a record back changed what it says\n  {line}");

        // And the second writing must be the first: a shape that settled only after a round trip
        // would make one record two different lines depending on how often it had been read.
        assert_eq!(written, again.to_line(), "the written form did not settle\n  {line}");
    }
});
