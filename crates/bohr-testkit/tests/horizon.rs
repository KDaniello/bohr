//! A run horizon: the ending a guest that never ends is given.
//!
//! A server neither asserts nor exits, so without a horizon it is reported hung.
//!
//! A budget means the guest is stuck; a horizon means a server ran long enough, and the oracle is
//! asked.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::{RunConfig, StopReason};
use bohr_tape::Tape;
use bohr_testkit::{Scenario, judge_scenario};
use bohr_trace::Outcome;
use bohr_types::{Seed, Tick};

/// `nanosleep`, which parks the task until the tick it named.
const SLEEP: u32 = 35;
/// `futex`; operation 0 is a wait, which parks the task until somebody wakes the word.
const FUTEX: u32 = 202;

/// Where the sleep's `timespec` is placed: one millisecond, as two little-endian words.
const TIMESPEC_AT: u64 = MEMORY_BASE;

/// A guest that never ends: it sleeps a millisecond, again and again.
fn a_server() -> NullProgram {
    NullProgram::single(vec![
        GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] },
        GuestOp::Jump { pc: 0 },
    ])
}

/// One millisecond as the kernel's `struct timespec`.
fn a_millisecond() -> Vec<bohr_testkit::Preload> {
    let mut bytes = 0u64.to_le_bytes().to_vec();
    bytes.extend_from_slice(&Tick::MILLISECOND.get().to_le_bytes());
    vec![(0, TIMESPEC_AT, bytes)]
}

/// Bounded well inside the event budget, so only the tick budget or the horizon ends it.
fn at(horizon: Option<Tick>) -> RunConfig {
    RunConfig { horizon, max_ticks: Tick(50 * Tick::MILLISECOND.get()), ..RunConfig::default() }
}

fn judge(config: RunConfig) -> bohr_testkit::Judged {
    judge_scenario(
        vec![a_server()],
        &a_millisecond(),
        Scenario::default(),
        &Tape::from_seed(Seed(7)),
        config,
    )
}

/// Without a horizon the guest is hung; with one it is judged clean.
#[test]
fn a_server_is_judged_where_it_used_to_be_reported_hung() {
    // Hung via the deadlock check, since no timer before the budget remains; the oracle is not
    // asked.
    let hung = judge(at(None));
    assert!(
        matches!(hung.report.outcome, Outcome::GuestHung(_)),
        "without a horizon a server is hung: {:?}",
        hung.report.outcome
    );

    let judged = judge(at(Some(Tick(20 * Tick::MILLISECOND.get()))));
    assert_eq!(judged.report.stop_reason, StopReason::HorizonReached);
    assert_eq!(
        judged.report.outcome,
        Outcome::Clean,
        "the oracle saw the whole execution the scenario asked for"
    );
}

/// The run ends at the horizon.
#[test]
fn a_run_ends_at_the_moment_it_was_asked_to() {
    let horizon = Tick(20 * Tick::MILLISECOND.get());
    let judged = judge(at(Some(horizon)));

    assert!(
        judged.report.ticks >= horizon,
        "the run reached the horizon: {:?}",
        judged.report.ticks
    );
    // Within one syscall, since the loop checks between stops.
    assert!(
        judged.report.ticks.get() < horizon.get() + Tick::MILLISECOND.get(),
        "and did not run far past it: {:?}",
        judged.report.ticks
    );
}

/// The horizon is part of the run: two horizons give two digests.
#[test]
fn two_horizons_are_two_runs() {
    let early = judge(at(Some(Tick(10 * Tick::MILLISECOND.get()))));
    let late = judge(at(Some(Tick(20 * Tick::MILLISECOND.get()))));

    assert_ne!(early.report.digest, late.report.digest);
    assert!(early.report.events < late.report.events);
}

/// The same horizon at the same seed is the same run, a hundred times over.
#[test]
fn a_horizon_reproduces() {
    let config = at(Some(Tick(20 * Tick::MILLISECOND.get())));
    let first = judge(config);
    for _ in 0..100 {
        assert_eq!(judge(config).report.digest, first.report.digest);
    }
}

/// A deadlock before the horizon is still a deadlock.
#[test]
fn a_guest_that_wedges_before_the_horizon_is_still_stuck() {
    // A wait nothing will wake, with nothing armed.
    let wedged = judge_scenario(
        vec![NullProgram::single(vec![GuestOp::Syscall {
            nr: FUTEX,
            args: [MEMORY_BASE, 0, 0, 0, 0, 0],
        }])],
        &[],
        Scenario::default(),
        &Tape::from_seed(Seed(7)),
        at(Some(Tick(20 * Tick::MILLISECOND.get()))),
    );

    assert_eq!(wedged.report.stop_reason, StopReason::Deadlock);
    assert!(
        matches!(wedged.report.outcome, Outcome::GuestHung(_)),
        "a horizon must not launder a deadlock into a clean ending: {:?}",
        wedged.report.outcome
    );
}
