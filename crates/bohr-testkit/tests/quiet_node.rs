//! A node that stopped is not a node that is idle, and the difference is that one still polls.
//!
//! A guest with nothing to do does not fall silent: it asks the time, it waits on its poller, it
//! asks again. An execution client with no work spends a million clock reads and half a million
//! waits on saying so. A guest that has *stopped* — parked in a wait nothing will release — makes
//! no call at all.
//!
//! From the run loop the two are identical: every task blocked, a timer somewhere in the future. So
//! a horizon completes both, and a run that ended with a wedged guest is judged on the execution
//! that happened rather than on the one that stopped. Measured against a real execution client:
//! twelve runs of twenty left it unable to start, and the panel called eight of them clean.
//!
//! **This is the first verdict that needs no driver.** Every other way of noticing a dead guest
//! here reads something the guest said about itself, or something a caller asked it — and a caller
//! is written per target. A node that went quiet is a finding for anything.
//!
//! Stated as a pair, because a watcher that fired on the polling guest too would satisfy any claim
//! made about it and mean nothing.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::{EventKind, Outcome};
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::{LivenessConfig, NetworkConfig};

/// `nanosleep`.
const SLEEP: u32 = 35;
/// `futex`.
const FUTEX: u32 = 202;
/// `exit_group`.
const EXIT_GROUP: u32 = 231;
/// Block while the word still holds the expected value.
const FUTEX_WAIT: u64 = 0;

const TIMESPEC_AT: u64 = MEMORY_BASE;
/// A word nothing ever writes, so a wait on it is a wait nothing will release.
const NOBODY_WAKES_AT: u64 = MEMORY_BASE + 0x100;

/// How long a node may make no call before it is reported.
const QUIET: Tick = Tick(20 * Tick::MILLISECOND.get());
/// Far enough past the span that the check has fired well before the run ends.
const HORIZON: Tick = Tick(200 * Tick::MILLISECOND.get());

/// Asks the time for ever, which is what a server with nothing to do does.
fn polls_for_ever() -> NullProgram {
    NullProgram::single(
        (0..10_000)
            .map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] })
            .collect(),
    )
}

/// Makes a few calls and then waits on a word nobody writes.
///
/// The shape of a guest that has failed and is unwinding into a wait it will never leave — which is
/// what an execution client does after it decides it cannot open its store.
fn stops() -> NullProgram {
    let mut ops: Vec<GuestOp> = (0..3)
        .map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] })
        .collect();
    ops.push(GuestOp::Syscall { nr: FUTEX, args: [NOBODY_WAKES_AT, FUTEX_WAIT, 0, 0, 0, 0] });
    NullProgram::single(ops)
}

/// Makes a few calls and leaves, which is what a caller with nothing left to ask does.
fn finishes() -> NullProgram {
    let mut ops: Vec<GuestOp> = (0..3)
        .map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] })
        .collect();
    ops.push(GuestOp::Syscall { nr: EXIT_GROUP, args: [0; 6] });
    NullProgram::single(ops)
}

fn memory() -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&Tick::MILLISECOND.get().to_le_bytes());
    (0..2).map(|node| (node, TIMESPEC_AT, timespec.clone())).collect()
}

/// Both programs, with the second node the one under question.
fn judge(second: NullProgram) -> Judged {
    let scenario = Scenario {
        network: NetworkConfig { nodes: 2, ..NetworkConfig::default() },
        liveness: Some(LivenessConfig { after_heal_ticks: Tick(0), quiet_ticks: QUIET }),
        ..Scenario::default()
    };
    judge_scenario(
        vec![polls_for_ever(), second],
        &memory(),
        scenario,
        &Tape::from_seed(Seed(1)),
        RunConfig { horizon: Some(HORIZON), ..RunConfig::default() },
    )
}

/// Which nodes the watcher named, in order.
fn named(judged: &Judged) -> Vec<NodeId> {
    judged
        .events
        .iter()
        .filter(|e| {
            matches!(&e.kind, EventKind::Diagnostic { code, .. } if code == bohr_trace::LIVENESS_CODE)
        })
        .map(|e| e.node)
        .collect()
}

/// The claim: a node that stopped is named, and the run says so.
#[test]
fn a_node_that_stopped_is_a_finding() {
    let judged = judge(stops());
    assert_eq!(named(&judged), vec![NodeId(1)], "the watcher named the wrong nodes");
    assert!(
        matches!(judged.report.outcome, Outcome::LivenessViolated(_)),
        "a node that stopped was judged {:?}",
        judged.report.outcome
    );
}

/// And a cluster where both nodes are only idle is clean, so the pair means something.
#[test]
fn a_node_that_is_only_idle_is_not() {
    let judged = judge(polls_for_ever());
    assert_eq!(named(&judged), Vec::<NodeId>::new(), "an idle node was called stopped");
    assert_eq!(judged.report.outcome, Outcome::Clean);
}

/// And a node this world killed is not accused of having stopped.
///
/// **The case that would have made the watch unusable in the campaigns it was built for.** A dead
/// node makes no calls, which is what a stopped node looks like — so without this, arming the watch
/// alongside a crash campaign reports a stopped node at every kill whose outage outlasts the span,
/// and every run of a fault campaign carries a finding the world itself caused.
#[test]
fn a_node_this_world_killed_is_not_accused_of_stopping() {
    use bohr_world::{NodeFaultKind, ScheduledNodeFault};

    let killed = Scenario {
        network: NetworkConfig { nodes: 2, ..NetworkConfig::default() },
        liveness: Some(LivenessConfig { after_heal_ticks: Tick(0), quiet_ticks: QUIET }),
        node_faults: vec![ScheduledNodeFault {
            at_tick: Tick(4 * Tick::MILLISECOND.get()),
            node: NodeId(1),
            kind: NodeFaultKind::Kill,
        }],
        ..Scenario::default()
    };
    let judged = judge_scenario(
        vec![polls_for_ever(), polls_for_ever()],
        &memory(),
        killed,
        &Tape::from_seed(Seed(1)),
        RunConfig { horizon: Some(HORIZON), ..RunConfig::default() },
    );

    assert_eq!(named(&judged), Vec::<NodeId>::new(), "the world accused itself");
    assert_eq!(judged.report.outcome, Outcome::Clean);
}

/// Nor is a node whose program finished.
///
/// A caller that has asked everything it came to ask leaves, and is silent for whatever is left of
/// the run. Every scenario driven by such a caller would otherwise carry a finding about it —
/// including the one this watch was armed for, where the caller is the whole point.
#[test]
fn a_node_whose_program_ended_is_not_accused_of_stopping() {
    let judged = judge(finishes());
    assert_eq!(named(&judged), Vec::<NodeId>::new(), "a caller that finished was called stopped");
    assert_eq!(judged.report.outcome, Outcome::Clean);
}

/// A run says how much adversity actually landed on it.
///
/// **A campaign that injected nothing looks exactly like one that injected plenty and was answered
/// well.** Faults are drawn, so whether any fired is a fact about the run and not about the
/// configuration that armed them: a scenario can arm a policy at a moment the run ends before, and
/// reporting its clean runs without this would be reporting that a question was asked when it was
/// not. Measured — twenty seeds against a real client armed a kill and fired none, and only this
/// number said so.
///
/// The pair is a run with a fault written down and the same run without one.
#[test]
fn a_run_says_how_much_adversity_landed() {
    use bohr_world::{NodeFaultKind, ScheduledNodeFault};

    let with_a_kill = Scenario {
        network: NetworkConfig { nodes: 2, ..NetworkConfig::default() },
        node_faults: vec![ScheduledNodeFault {
            at_tick: Tick(4 * Tick::MILLISECOND.get()),
            node: NodeId(1),
            kind: NodeFaultKind::Kill,
        }],
        ..Scenario::default()
    };
    let judged = judge_scenario(
        vec![polls_for_ever(), polls_for_ever()],
        &memory(),
        with_a_kill,
        &Tape::from_seed(Seed(1)),
        RunConfig { horizon: Some(HORIZON), ..RunConfig::default() },
    );
    assert_eq!(judged.report.adversity, 1, "a scheduled kill was not counted");

    assert_eq!(
        judge(polls_for_ever()).report.adversity,
        0,
        "a run with nothing scheduled counted a fault"
    );
}
