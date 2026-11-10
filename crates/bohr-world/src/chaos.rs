//! Deliberate defects: an answer taken from the host, and a wake that ignores its draw.
//!
//! They show the tests catch them. Only the features of the same names enable them.

// The mutation harness enables these one at a time; the bans hold everywhere else. See
// docs/TESTING.md 7.4.
#![allow(clippy::disallowed_methods)]

/// The host's own clock, in nanoseconds.
pub(crate) fn host_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// Releases the longest waiter instead of the one the tape chose.
#[cfg(feature = "chaos-futex-order")]
pub(crate) const fn first_waiter(_choices: usize) -> usize {
    0
}

/// Answers every node's clock from the world clock, as if the cluster shared one machine.
///
/// Removes per-node clock skew.
#[cfg(feature = "chaos-node-clock-shared")]
pub(crate) fn shared_now(ctx: &bohr_kernel::WorldCtx<'_, '_>) -> bohr_types::Tick {
    ctx.world_now()
}

/// Fires due timers in an order taken from ambient hasher state.
///
/// Two timers due at one tick then fire in an unrecorded order.
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
/// The same scenario then fires its fault at different moments.
#[cfg(feature = "chaos-fault-moment-drift")]
pub(crate) fn drift_tick(at: bohr_types::Tick) -> bohr_types::Tick {
    use std::hash::{BuildHasher, Hasher};
    let ambient = std::collections::hash_map::RandomState::new().build_hasher().finish();
    bohr_types::Tick(at.get().saturating_add(ambient % 2_000))
}

/// Whether a vectored write to a socket is recognised as one.
///
/// Disabled, `writev` on a socket gets `EBADF`; the descriptor audit must catch it.
#[cfg(feature = "chaos-unknown-descriptor")]
pub(crate) const fn vectored_reaches_sockets() -> bool {
    false
}

/// Whether what the kernel answers about a process is kept off the disk image.
///
/// Disabled, `/proc` files live on the modelled disk and a crash rolls them back; the crash audit
/// must catch it.
#[cfg(feature = "chaos-proc-on-the-disk")]
pub(crate) const fn kernel_answers_stay_off_the_disk() -> bool {
    false
}
