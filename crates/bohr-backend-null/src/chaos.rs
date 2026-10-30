//! A deliberate defect: coverage that does not come from the guest alone.
//!
//! Present so the test that is supposed to catch it can be shown to catch it. Reachable only
//! through the feature that names it, which no ordinary build enables. See `docs/TESTING.md` §7.4.

/// Mixes a count of how often this process has recorded an edge into the edge itself.
///
/// The shape of the mistake it stands for: coverage gathered from anything but the guest's own
/// execution — a timestamp, a counter, an address the loader chose. The map then differs between
/// two runs of one seed, which makes every corpus decision built on it unrepeatable, and a search
/// whose corpus is unrepeatable produces findings nobody else can reach.
#[cfg(feature = "chaos-coverage-ambient")]
pub(crate) fn drift() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEEN: AtomicU64 = AtomicU64::new(0);
    SEEN.fetch_add(1, Ordering::Relaxed)
}
