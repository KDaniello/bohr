//! Arbitrary bytes offered to the event log reader, whole and truncated.
//!
//! A log stays readable up to the last complete record after a crash, so reading damage must be
//! safe.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The strict reader refuses damage; the lossy one returns the readable prefix.
    let _ = bohr_trace::read_events(data);
    let (events, _) = bohr_trace::read_events_lossy(data);

    // Decoded events must digest.
    let _ = bohr_trace::digest_of(&events);
});
