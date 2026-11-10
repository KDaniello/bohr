//! A deliberate defect: a digest that depends on how long the run took.
//!
//! It shows the digest's exclusion list holds. Only the feature of the same name enables it.

// The mutation harness enables this; the bans hold everywhere else. See docs/TESTING.md 7.4.
#![allow(clippy::disallowed_methods)]

/// Folds host-measured time into the definition of which run this was.
pub(crate) fn absorb_elapsed(hasher: &mut blake3::Hasher) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0u64, |d| d.subsec_nanos().into());
    hasher.update(&nanos.to_le_bytes());
}
