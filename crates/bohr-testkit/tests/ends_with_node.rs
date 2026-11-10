//! A run can end when the node it was for ends.
//!
//! A service never ends, and a generous horizon wastes most of the run. Naming the driver's node
//! ends the run with it; the pair differs only in that.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::{RunConfig, StopReason};
use bohr_tape::Tape;
use bohr_testkit::{Scenario, judge_scenario};
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::NetworkConfig;

/// `nanosleep`.
const SLEEP: u32 = 35;

/// `exit_group`, which ends a process and every thread in it.
const EXIT_GROUP: u32 = 231;

const TIMESPEC_AT: u64 = MEMORY_BASE;

/// Long enough that a horizon is what would stop it.
const HORIZON: Tick = Tick(200 * Tick::MILLISECOND.get());

/// Serves for ever.
fn serves_for_ever() -> NullProgram {
    NullProgram::single(
        (0..10_000)
            .map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] })
            .collect(),
    )
}

/// Asks four questions and exits.
fn drives_and_finishes() -> NullProgram {
    let mut ops: Vec<GuestOp> = (0..4)
        .map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] })
        .collect();
    ops.push(GuestOp::Exit(0));
    NullProgram::single(ops)
}

fn memory() -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&Tick::MILLISECOND.get().to_le_bytes());
    (0..2).map(|node| (node, TIMESPEC_AT, timespec.clone())).collect()
}

/// The same cluster and the same horizon, ending where the scenario says.
fn run(ends_with: Option<NodeId>) -> (StopReason, u64) {
    let scenario = Scenario {
        network: NetworkConfig { nodes: 2, ..NetworkConfig::default() },
        ..Scenario::default()
    };
    let judged = judge_scenario(
        vec![serves_for_ever(), drives_and_finishes()],
        &memory(),
        scenario,
        &Tape::from_seed(Seed(1)),
        RunConfig { horizon: Some(HORIZON), ends_with, ..RunConfig::default() },
    );
    (judged.report.stop_reason, judged.report.events)
}

/// Naming the node shortens the run.
#[test]
fn a_run_ends_with_the_node_it_was_for() {
    let (waited_out, spent) = run(None);
    let (ended, cost) = run(Some(NodeId(1)));

    assert_eq!(waited_out, StopReason::HorizonReached);
    assert_eq!(ended, StopReason::NodeFinished);
    assert!(
        cost * 4 < spent,
        "ending with the driver should cost a fraction of waiting the horizon out, \
         and cost {cost} events against {spent}"
    );
}

/// Only the named node ends the run.
#[test]
fn another_nodes_ending_is_not_the_runs() {
    // Node zero never ends, so naming it leaves the horizon to do the work.
    let (reason, _) = run(Some(NodeId(0)));
    assert_eq!(reason, StopReason::HorizonReached);
}

/// A driver with more than one thread still ends the run when it ends.
///
/// `exit_group` with parked threads, as Go programs do, still makes the node `Dead`.
#[test]
fn a_driver_that_ends_with_threads_still_parked_ends_the_run() {
    let mut driver: Vec<GuestOp> = vec![GuestOp::Spawn, GuestOp::Spawn];
    driver
        .extend((0..4).map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] }));
    // `exit_group` takes every thread with it.
    driver.push(GuestOp::Syscall { nr: EXIT_GROUP, args: [0; 6] });

    // Two threads that go to sleep and are still there when the program ends.
    let parked = vec![GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] }; 10_000];
    let threaded = NullProgram { main: driver, children: vec![parked.clone(), parked] };

    let judged = judge_scenario(
        vec![serves_for_ever(), threaded],
        &memory(),
        Scenario { network: NetworkConfig::default(), ..Scenario::default() },
        &Tape::from_seed(Seed(4)),
        RunConfig { horizon: Some(HORIZON), ends_with: Some(NodeId(1)), ..RunConfig::default() },
    );

    assert_eq!(
        judged.report.stop_reason,
        StopReason::NodeFinished,
        "a node whose program exited with threads parked did not end the run"
    );
    assert!(
        judged.report.ticks < HORIZON,
        "the run reached {} of a {HORIZON} horizon, so it was the horizon that stopped it",
        judged.report.ticks
    );
}
