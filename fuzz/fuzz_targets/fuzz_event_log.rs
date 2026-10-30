//! Arbitrary bytes offered to the event log reader, whole and truncated.
//!
//! A log is framed so that a crash mid-write leaves it readable up to the last complete record,
//! which is only worth anything if reading a damaged one is safe.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The strict reader refuses damage; the lossy one returns the prefix it could read and says
    // where it stopped. Both meet the same bytes here.
    let _ = bohr_trace::read_events(data);
    let (events, _) = bohr_trace::read_events_lossy(data);

    // What was read must digest: a run compares digests, and an event that decoded but cannot be
    // projected would be a bundle that reads and never compares.
    let _ = bohr_trace::digest_of(&events);
});
