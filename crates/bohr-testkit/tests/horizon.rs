//! A run horizon: the ending a guest that never ends is given.
//!
//! Every oracle before this milestone read either an assertion the guest made about itself or the
//! status it exited with, so a guest that neither asserts nor exits could not be judged at all —
//! it exhausted its tick budget and was reported hung, whatever it had done. That is every
//! blockchain client, every database server, and every program this engine is now pointed at.
//!
//! A horizon is not a budget. A budget answers "this guest is stuck"; a horizon answers "this guest
//! is a server, and we have seen enough". The difference is visible in exactly one place — whether
//! the oracle is asked — and these are the properties that keep the two apart.

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

/// A guest that never ends, which is the shape of every target this milestone exists for: it sleeps
/// a millisecond, and goes back to the beginning to do it again.
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

/// Every run here is bounded well inside the event budget, so what ends it is the tick budget or
/// the horizon and never a third thing the test did not mean to measure.
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

/// The property the whole milestone rests on. Without a horizon this guest is hung; with one it is
/// judged, and a guest that did nothing wrong comes out clean.
#[test]
fn a_server_is_judged_where_it_used_to_be_reported_hung() {
    // Reported hung, and by the deadlock check rather than the tick budget: the loop only advances
    // time when nothing is runnable, and it will not chase a timer past the budget — so an endless
    // sleeper runs out of reachable deadlines rather than out of ticks. Either way it is a hang,
    // and either way nothing asked the oracle.
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

/// The horizon is where the run ends, not merely where it is allowed to.
#[test]
fn a_run_ends_at_the_moment_it_was_asked_to() {
    let horizon = Tick(20 * Tick::MILLISECOND.get());
    let judged = judge(at(Some(horizon)));

    assert!(
        judged.report.ticks >= horizon,
        "the run reached the horizon: {:?}",
        judged.report.ticks
    );
    // Within one syscall of it: the loop breaks between stops, so the last call before the horizon
    // may carry the clock a slice past it. Anything further would mean the check is in the wrong
    // place in the loop.
    assert!(
        judged.report.ticks.get() < horizon.get() + Tick::MILLISECOND.get(),
        "and did not run far past it: {:?}",
        judged.report.ticks
    );
}

/// A horizon changes what a run means, so it has to be part of what a run is: the same guest at the
/// same seed with two horizons is two runs, and a replay that lost the horizon would diverge.
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

/// A guest that stops making progress before the horizon is still a deadlock. The horizon says when
/// a run is complete; it does not say that everything before it was fine.
#[test]
fn a_guest_that_wedges_before_the_horizon_is_still_stuck() {
    // A wait on a word nobody will ever wake: nothing armed, nothing runnable, and a horizon far
    // away, so the run has nowhere to go and no time to get there.
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
