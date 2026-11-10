//! A deliberate defect: bytes that do not come from the tape.
//!
//! It shows the tests catch ambient randomness. Only the feature of the same name enables it.

use std::hash::{BuildHasher, Hasher};

/// Overwrites what was read from the tape with a source outside it.
pub(crate) fn overwrite(out: &mut [u8]) {
    let mut state = std::collections::hash_map::RandomState::new().build_hasher().finish();
    for byte in out {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        *byte = (state >> 33) as u8;
    }
}
