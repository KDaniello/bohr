//! A node that stopped is not a node that is idle, and the difference is that one still polls.
//!
//! An idle guest keeps reading the clock and polling; a stopped guest, parked in a wait nothing
//! will release, makes no calls. The run loop sees both as blocked with a future timer. Against a
//! real execution client, 12 of 20 runs left it unable to start and 8 of those were judged clean.
//!
//! This verdict needs no target-specific driver. It fires on the stopped guest and not on the
//! idle one.

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

/// Asks the time for ever, like an idle server.
fn polls_for_ever() -> NullProgram {
    NullProgram::single(
        (0..10_000)
            .map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] })
            .collect(),
    )
}

/// Makes a few calls and then waits on a word nobody writes.
///
/// Like an execution client that cannot open its store.
fn stops() -> NullProgram {
    let mut ops: Vec<GuestOp> = (0..3)
        .map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] })
        .collect();
    ops.push(GuestOp::Syscall { nr: FUTEX, args: [NOBODY_WAKES_AT, FUTEX_WAIT, 0, 0, 0, 0] });
    NullProgram::single(ops)
}

/// Makes a few calls and exits, like a finished caller.
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

/// A stopped node is named.
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

/// A cluster of idle nodes is clean.
#[test]
fn a_node_that_is_only_idle_is_not() {
    let judged = judge(polls_for_ever());
    assert_eq!(named(&judged), Vec::<NodeId>::new(), "an idle node was called stopped");
    assert_eq!(judged.report.outcome, Outcome::Clean);
}

/// A node the world killed is not reported as stopped, so the watch works in crash campaigns.
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
/// A finished caller is silent for the rest of the run and is not reported.
#[test]
fn a_node_whose_program_ended_is_not_accused_of_stopping() {
    let judged = judge(finishes());
    assert_eq!(named(&judged), Vec::<NodeId>::new(), "a caller that finished was called stopped");
    assert_eq!(judged.report.outcome, Outcome::Clean);
}

/// A run says how much adversity actually landed on it.
///
/// Faults are drawn and a run can end before they arm; twenty seeds against a real client fired
/// none, and only this number showed it. Checked with and without a scheduled fault.
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
