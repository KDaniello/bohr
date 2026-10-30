//! Deliberate defects: two ways a scheduling decision can stop coming from the tape.
//!
//! Present so the tests that are supposed to catch them can be shown to catch them. Reachable only
//! through the features that name them, which no ordinary build enables.

// This whole module is the deliberate violation the mutation harness enables one at a time; the
// compile-time bans hold everywhere else. See docs/TESTING.md 7.4.
#![allow(clippy::disallowed_methods)]

use bohr_types::TaskId;

/// Reorders the runnable set by a container whose iteration order is seeded per process.
#[cfg(feature = "chaos-hash-iteration")]
#[allow(clippy::disallowed_types)]
pub(crate) fn in_hash_order(runnable: &[TaskId]) -> Vec<TaskId> {
    let set: std::collections::HashSet<TaskId> = runnable.iter().copied().collect();
    set.into_iter().collect()
}

/// Resolves the choice by which task the host got round to first, rather than by a draw.
#[cfg(feature = "chaos-delivery-tie")]
pub(crate) fn first_to_arrive(runnable: &[TaskId]) -> TaskId {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let arrived = START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as usize;
    // Divided by the coarsest clock granularity in use, because the low digits are constant where
    // the host counts in hundreds of nanoseconds and the arrival order would not move.
    runnable[(arrived / 100) % runnable.len()]
}
