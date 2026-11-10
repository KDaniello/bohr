//! Deliberate defects: two ways a scheduling decision can stop coming from the tape.
//!
//! They show the tests catch them. Only the features of the same names enable them.

// The mutation harness enables these one at a time; the bans hold everywhere else. See
// docs/TESTING.md 7.4.
#![allow(clippy::disallowed_methods)]

use bohr_types::TaskId;

/// Reorders the runnable set by a container whose iteration order is seeded per process.
#[cfg(feature = "chaos-hash-iteration")]
#[allow(clippy::disallowed_types)]
pub(crate) fn in_hash_order(runnable: &[TaskId]) -> Vec<TaskId> {
    let set: std::collections::HashSet<TaskId> = runnable.iter().copied().collect();
    set.into_iter().collect()
}

/// Resolves the choice by host timing instead of a draw.
#[cfg(feature = "chaos-delivery-tie")]
pub(crate) fn first_to_arrive(runnable: &[TaskId]) -> TaskId {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let arrived = START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as usize;
    // Divided by the coarsest host clock granularity, whose low digits never change.
    runnable[(arrived / 100) % runnable.len()]
}
