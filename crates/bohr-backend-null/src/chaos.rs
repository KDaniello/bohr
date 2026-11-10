//! A deliberate defect: coverage mixed with state from outside the guest.
//!
//! It exists to show that the test meant to catch it does. Only the feature of the same name
//! enables it. See `docs/TESTING.md` §7.4.

/// Mixes a per-process counter into each recorded edge.
///
/// Stands for coverage taken from anything but the guest's execution: a timestamp, a counter, a
/// loader address. The map then differs between two runs of one seed, and the campaign stops
/// being repeatable.
#[cfg(feature = "chaos-coverage-ambient")]
pub(crate) fn drift() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEEN: AtomicU64 = AtomicU64::new(0);
    SEEN.fetch_add(1, Ordering::Relaxed)
}
