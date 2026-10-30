//! Deliberate defects: an answer taken from the host, and a wake that ignores its draw.
//!
//! Present so the tests that are supposed to catch a syscall reaching the host can be shown to
//! catch it. Reachable only through the feature that names it, which no ordinary build enables.

// This whole module is the deliberate violation the mutation harness enables one at a time; the
// compile-time bans hold everywhere else. See docs/TESTING.md 7.4.
#![allow(clippy::disallowed_methods)]

/// The host's own clock, in nanoseconds.
pub(crate) fn host_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// Releases the waiter that has been waiting longest, rather than the one the tape chose.
///
/// This is the shape of the bug the whole futex model exists to prevent: the queue order is the
/// host's, and a run inherits an ordering nobody recorded.
#[cfg(feature = "chaos-futex-order")]
pub(crate) const fn first_waiter(_choices: usize) -> usize {
    0
}

/// Answers every node's clock from the world clock, as if the cluster shared one machine.
///
/// The defect the per-node clocks exist to prevent: software that assumes two hosts agree on the
/// time passes every test here and fails in production.
#[cfg(feature = "chaos-node-clock-shared")]
pub(crate) fn shared_now(ctx: &bohr_kernel::WorldCtx<'_, '_>) -> bohr_types::Tick {
    ctx.world_now()
}

/// Fires due timers in an order taken from ambient hasher state, not `(tick, node, seq)`.
///
/// The defect the timer queue's ordering key exists to prevent: two entries due at one tick fire
/// in an order nobody recorded, and the trace differs between two runs of one seed.
#[cfg(feature = "chaos-deadline-tie")]
pub(crate) fn scramble_due<T>(due: &mut [T]) {
    use std::hash::{BuildHasher, Hasher};
    if due.len() > 1 {
        let ambient =
            std::collections::hash_map::RandomState::new().build_hasher().finish() as usize;
        due.rotate_left(ambient % due.len());
    }
}

/// Arms a scheduled fault a few ambient ticks late.
///
/// The defect the scenario clock exists to prevent: a fault that fires "about then" instead of
/// exactly then, so the same scenario file produces runs that disagree about when the world broke.
#[cfg(feature = "chaos-fault-moment-drift")]
pub(crate) fn drift_tick(at: bohr_types::Tick) -> bohr_types::Tick {
    use std::hash::{BuildHasher, Hasher};
    let ambient = std::collections::hash_map::RandomState::new().build_hasher().finish();
    bohr_types::Tick(at.get().saturating_add(ambient % 2_000))
}

/// Whether a vectored write to a socket is recognised as one.
///
/// The defect the descriptor audit exists to name, and one this tool actually had: `writev` was
/// left off the list of calls that follow a socket descriptor, so a server answering a request
/// with its headers and its body in one call was told `EBADF` about a socket it was holding. What
/// the client did afterwards looked exactly like a client defect, and was ours.
#[cfg(feature = "chaos-unknown-descriptor")]
pub(crate) const fn vectored_reaches_sockets() -> bool {
    false
}

/// Whether what the kernel answers about a process is kept off the disk image.
///
/// The other defect the crash audit exists to name, and the more expensive one: generated `/proc`
/// files were on the modelled disk, so a crash rolled them back and an execution client panicked
/// reading its own status. It was a syscall away from being filed upstream.
#[cfg(feature = "chaos-proc-on-the-disk")]
pub(crate) const fn kernel_answers_stay_off_the_disk() -> bool {
    false
}
