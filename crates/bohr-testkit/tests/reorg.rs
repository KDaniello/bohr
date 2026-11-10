//! A reorg: every node rewound at one moment, and history let run again.
//!
//! Protocol-free: the world has no notion of a block. A set of nodes returns to a common older
//! synced image at one instant, and the cluster continues on a different history.
//!
//! Checked by comparing the scenario with and without the fault.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::{EventKind, FaultKind, FaultTarget};
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::{NetworkConfig, NodeFaultKind, ScheduledNodeFault};

/// `openat`.
const OPENAT: u32 = 257;
/// `write`.
const WRITE: u32 = 1;
/// `fsync`; each completed sync is one step of rewindable history.
const FSYNC: u32 = 74;
/// `pread64`, which the node uses to read back what survived.
const PREAD64: u32 = 17;
/// `nanosleep`.
const SLEEP: u32 = 35;

/// `O_CREAT | O_RDWR`.
const OPEN_FLAGS: u64 = 0o100 | 2;

const PATH_AT: u64 = MEMORY_BASE;
const TIMESPEC_AT: u64 = MEMORY_BASE + 0x100;
const PAYLOAD_AT: u64 = MEMORY_BASE + 0x200;
const READBACK_AT: u64 = MEMORY_BASE + 0x300;

/// When history is rewound.
const REORG_AT: Tick = Tick(10 * Tick::MILLISECOND.get());

/// A node that appends a record and makes it durable, over and over, reading back as it goes.
///
/// After a rewind the read-back finds a shorter file, as a client sees an abandoned chain.
fn a_node() -> NullProgram {
    let mut ops =
        vec![GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, PATH_AT, OPEN_FLAGS, 0o644, 0, 0] }];
    for _ in 0..30 {
        ops.push(GuestOp::Syscall { nr: WRITE, args: [3, PAYLOAD_AT, 8, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: FSYNC, args: [3, 0, 0, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: PREAD64, args: [3, READBACK_AT, 8, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }
    NullProgram::single(ops)
}

fn memory() -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&Tick::MILLISECOND.get().to_le_bytes());

    let mut out = Vec::new();
    for node in 0..2u32 {
        out.push((node, PATH_AT, b"/chain\0".to_vec()));
        out.push((node, TIMESPEC_AT, timespec.clone()));
        out.push((node, PAYLOAD_AT, b"a record".to_vec()));
    }
    out
}

fn judge(faults: Vec<ScheduledNodeFault>) -> Judged {
    let scenario = Scenario {
        network: NetworkConfig { nodes: 2, ..NetworkConfig::default() },
        node_faults: faults,
        ..Scenario::default()
    };
    let config = RunConfig {
        horizon: Some(Tick(40 * Tick::MILLISECOND.get())),
        max_ticks: Tick(Tick::SECOND.get()),
        ..RunConfig::default()
    };
    judge_scenario(vec![a_node(), a_node()], &memory(), scenario, &Tape::from_seed(Seed(5)), config)
}

fn a_reorg(back: usize, nodes: Vec<NodeId>) -> Vec<ScheduledNodeFault> {
    vec![ScheduledNodeFault {
        at_tick: REORG_AT,
        node: NodeId::ZERO,
        kind: NodeFaultKind::ChainReorg { back, nodes },
    }]
}

/// Runs with and without a reorg differ by a measured amount.
#[test]
fn a_run_with_a_reorg_and_one_without_are_different_runs() {
    let plain = judge(Vec::new());
    let rewound = judge(a_reorg(2, Vec::new()));

    assert_ne!(plain.report.digest, rewound.report.digest);
    let delta = rewound.report.events.abs_diff(plain.report.events);
    assert!(delta > 0, "a reorg that changed nothing would not be a reorg");
}

/// Narrated once, at the world, and never as N node restarts.
///
/// One event, so it is distinguishable from independent rewinds.
#[test]
fn a_reorg_is_one_thing_that_happened_to_the_cluster() {
    let judged = judge(a_reorg(2, Vec::new()));

    let fired: Vec<&FaultKind> = judged
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultFired { fault, target } => {
                assert!(
                    matches!(target, FaultTarget::World),
                    "a reorg happens to the cluster, not to a node"
                );
                Some(fault)
            }
            _ => None,
        })
        .collect();

    assert_eq!(fired.len(), 1, "one fault fired, whatever it did to how many nodes");
    assert_eq!(*fired[0], FaultKind::ChainReorg);
}

/// The actual depth is on the timeline, since each node has its own sync history.
#[test]
fn how_far_history_went_back_is_recorded() {
    let judged = judge(a_reorg(2, Vec::new()));

    let payload = judged
        .events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::Domain { payload, .. } => Some(payload.clone()),
            _ => None,
        })
        .expect("a reorg records its depth");

    let asked = u32::from_le_bytes(payload[0..4].try_into().expect("four bytes"));
    let applied = u32::from_le_bytes(payload[4..8].try_into().expect("four bytes"));
    let nodes = u32::from_le_bytes(payload[8..12].try_into().expect("four bytes"));

    assert_eq!(asked, 2);
    assert_eq!(applied, 2, "the history was deep enough to take both steps");
    assert_eq!(nodes, 2, "both nodes went back");
}

/// A named subset is rewound, like a fork only part of the cluster followed.
#[test]
fn a_reorg_may_rewind_some_of_the_cluster() {
    let judged = judge(a_reorg(2, vec![NodeId(0)]));

    let restarted = judged
        .events
        .iter()
        .filter(|e| {
            matches!(e.kind, EventKind::NodeState { state: bohr_trace::NodeState::Restarting })
        })
        .count();
    assert_eq!(restarted, 1, "one node was rewound and the other carried on");
}

/// The same scenario at the same seed is the same run.
#[test]
fn a_reorg_reproduces() {
    let first = judge(a_reorg(2, Vec::new()));
    for _ in 0..50 {
        assert_eq!(judge(a_reorg(2, Vec::new())).report.digest, first.report.digest);
    }
}

/// Prints the measured difference for the gate.
#[test]
fn the_difference_a_reorg_makes_is_measured() {
    let plain = judge(Vec::new());
    let rewound = judge(a_reorg(2, Vec::new()));

    println!(
        "reorg: events {} → {}, draws {} → {}",
        plain.report.events,
        rewound.report.events,
        plain.report.journal.len(),
        rewound.report.journal.len()
    );

    // Recorded in `benches/baseline.toml` under `[m4_1.reorg]`; asserted as a range.
    let delta = plain.report.events.abs_diff(rewound.report.events);
    assert!(delta >= 10, "a reorg moved only {delta} events, which is not a rewind");
}
