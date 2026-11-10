//! Arbitrary bytes offered to the tape reader, and drawn from once loaded.
//!
//! Tapes are mutated by design, so the container reader meets damaged input routinely.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(tape) = bohr_tape::Tape::from_container(data) else {
        return;
    };
    // A loaded tape draws without end or panic.
    let mut cursor = tape.cursor();
    for _ in 0..64 {
        let _ = cursor.draw_range(7, bohr_tape::DrawSite::SchedPick);
        let _ = cursor.draw_bool(1, 1000, bohr_tape::DrawSite::NetDeliver);
        let _ = cursor.draw_exponential(1_000_000, bohr_tape::DrawSite::NetDelay);
    }
    let _ = tape.to_container();
});
