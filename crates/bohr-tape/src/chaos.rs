//! A deliberate defect: bytes that do not come from the tape.
//!
//! Present so the tests that are supposed to catch ambient randomness can be shown to catch it.
//! Reachable only through the feature that names it, which no ordinary build enables.

use std::hash::{BuildHasher, Hasher};

/// Overwrites what was read from the tape with a source outside it.
pub(crate) fn overwrite(out: &mut [u8]) {
    let mut state = std::collections::hash_map::RandomState::new().build_hasher().finish();
    for byte in out {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        *byte = (state >> 33) as u8;
    }
}
