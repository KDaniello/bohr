//! A cluster that healed and went on committing nothing.
//!
//! The oracle fires on the defective cluster and stays silent on its correct twin, with the same
//! scenario and seeds.
//!
//! Scripted against the null backend, so it tests the oracle only. Whether a search reaches such a
//! defect is measured on the seeded corpus on Linux.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::{EventKind, Outcome};
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::{LivenessConfig, NetworkConfig, ScheduledPartition};

/// `openat`, which the node uses to get itself a log to sync.
const OPENAT: u32 = 257;
/// `write`.
const WRITE: u32 = 1;
/// `fsync`, the only call counted as progress.
const FSYNC: u32 = 74;
/// `nanosleep`.
const SLEEP: u32 = 35;

/// `O_CREAT | O_RDWR`.
const OPEN_FLAGS: u64 = 0o100 | 2;

/// Where the log's path is placed in guest memory.
const PATH_AT: u64 = MEMORY_BASE;
/// Where the sleep's `timespec` is placed.
const TIMESPEC_AT: u64 = MEMORY_BASE + 0x100;
/// Where the bytes a node writes to its log are placed.
const PAYLOAD_AT: u64 = MEMORY_BASE + 0x200;

/// When the cut goes up, and when it lifts.
const CUT_AT: Tick = Tick(5 * Tick::MILLISECOND.get());
const HEAL_AT: Tick = Tick(15 * Tick::MILLISECOND.get());
/// How long a healed cluster may commit nothing before that is a finding.
const AFTER_HEAL: Tick = Tick(20 * Tick::MILLISECOND.get());

/// What each node writes at the top of the run so both twins share a prologue.
fn prologue() -> Vec<GuestOp> {
    vec![GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, PATH_AT, OPEN_FLAGS, 0o644, 0, 0] }]
}

/// One round: write something and make it durable, then wait.
fn a_round() -> Vec<GuestOp> {
    vec![
        GuestOp::Syscall { nr: WRITE, args: [3, PAYLOAD_AT, 8, 0, 0, 0] },
        GuestOp::Syscall { nr: FSYNC, args: [3, 0, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] },
    ]
}

/// A node that keeps committing for the whole run, whatever the network does.
fn healthy() -> NullProgram {
    let mut ops = prologue();
    for _ in 0..60 {
        ops.extend(a_round());
    }
    NullProgram::single(ops)
}

/// A node that commits for a while and then stops, holding a view of the cluster it never revises.
///
/// Nothing crashes, asserts or exits; the node is alive and the cluster commits nothing.
fn goes_quiet() -> NullProgram {
    let mut ops = prologue();
    for _ in 0..6 {
        ops.extend(a_round());
    }
    let sleeping = ops.len();
    ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    ops.push(GuestOp::Jump { pc: sleeping });
    NullProgram::single(ops)
}

/// The path, the timespec and the payload every node reads out of its own memory.
fn memory() -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&Tick::MILLISECOND.get().to_le_bytes());

    let mut out = Vec::new();
    for node in 0..2u32 {
        out.push((node, PATH_AT, b"/log\0".to_vec()));
        out.push((node, TIMESPEC_AT, timespec.clone()));
        out.push((node, PAYLOAD_AT, b"a record".to_vec()));
    }
    out
}

/// A cut between the two nodes that goes up and comes down again.
fn split_and_heal() -> NetworkConfig {
    NetworkConfig {
        nodes: 2,
        partitions: vec![ScheduledPartition {
            at_tick: CUT_AT,
            heal_at_tick: HEAL_AT,
            cut: vec![(NodeId(0), NodeId(1)), (NodeId(1), NodeId(0))],
        }],
        ..NetworkConfig::default()
    }
}

fn judge(nodes: Vec<NullProgram>, watching: bool) -> Judged {
    let scenario = Scenario {
        network: split_and_heal(),
        liveness: watching
            .then_some(LivenessConfig { after_heal_ticks: AFTER_HEAL, quiet_ticks: Tick(0) }),
        ..Scenario::default()
    };
    // Long enough for the heal, the window, and the answer.
    let config = RunConfig {
        horizon: Some(Tick(50 * Tick::MILLISECOND.get())),
        max_ticks: Tick(Tick::SECOND.get()),
        ..RunConfig::default()
    };
    judge_scenario(nodes, &memory(), scenario, &Tape::from_seed(Seed(7)), config)
}

fn silent_nodes(judged: &Judged) -> Vec<NodeId> {
    judged
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic { code, .. } if code == bohr_trace::LIVENESS_CODE => {
                Some(event.node)
            }
            _ => None,
        })
        .collect()
}

/// The defective cluster is caught and the correct one is not.
#[test]
fn a_cluster_that_goes_quiet_is_caught_and_a_healthy_one_is_not() {
    let quiet = judge(vec![goes_quiet(), goes_quiet()], true);
    assert!(
        matches!(quiet.report.outcome, Outcome::LivenessViolated(_)),
        "a cluster that healed and committed nothing is a finding: {:?}",
        quiet.report.outcome
    );
    assert_eq!(silent_nodes(&quiet), vec![NodeId(0), NodeId(1)]);

    let healthy = judge(vec![healthy(), healthy()], true);
    assert_eq!(
        healthy.report.outcome,
        Outcome::Clean,
        "a cluster that kept committing is not a finding"
    );
    assert!(silent_nodes(&healthy).is_empty());
}

/// One quiet node is identified by node, so a reduction can tell it from the cluster's silence.
#[test]
fn a_silence_names_the_node_that_was_silent() {
    let judged = judge(vec![goes_quiet(), healthy()], true);
    assert_eq!(silent_nodes(&judged), vec![NodeId(0)]);
}

/// Nothing is watched unless asked, since diagnostics enter the digest.
#[test]
fn a_scenario_that_does_not_watch_records_nothing_and_runs_identically() {
    let watched = judge(vec![goes_quiet(), goes_quiet()], true);
    let plain = judge(vec![goes_quiet(), goes_quiet()], false);

    assert!(silent_nodes(&plain).is_empty());
    assert_eq!(plain.report.outcome, Outcome::Clean);
    assert_ne!(
        watched.report.digest, plain.report.digest,
        "the watcher speaks on the timeline, so a run that watched is a different run"
    );
}

/// The same scenario at the same seed is the same run, findings and all.
#[test]
fn a_watched_run_reproduces() {
    let first = judge(vec![goes_quiet(), goes_quiet()], true);
    for _ in 0..50 {
        let again = judge(vec![goes_quiet(), goes_quiet()], true);
        assert_eq!(again.report.digest, first.report.digest);
        assert_eq!(again.report.outcome, first.report.outcome);
    }
}

/// The watcher counts a sync when the disk image freezes; a stalled `DiskOp` is emitted earlier.
///
/// With every sync stalling, the healthy twin is still clean and the quiet one is still caught.
#[test]
fn a_stalled_sync_is_measured_where_it_lands_and_not_where_it_starts() {
    let stalling = bohr_world::DiskConfig {
        stall_probability: (1, 1),
        stall_mean_ticks: Tick::MICROSECOND.get() * 100,
        ..bohr_world::DiskConfig::default()
    };

    let judged = |nodes: Vec<NullProgram>| {
        judge_scenario(
            nodes,
            &memory(),
            Scenario {
                network: split_and_heal(),
                disk: stalling,
                liveness: Some(LivenessConfig {
                    after_heal_ticks: AFTER_HEAL,
                    quiet_ticks: Tick(0),
                }),
                ..Scenario::default()
            },
            &Tape::from_seed(Seed(7)),
            RunConfig {
                horizon: Some(Tick(50 * Tick::MILLISECOND.get())),
                max_ticks: Tick(Tick::SECOND.get()),
                ..RunConfig::default()
            },
        )
    };

    let healthy = judged(vec![healthy(), healthy()]);
    assert_eq!(
        healthy.report.outcome,
        Outcome::Clean,
        "every sync stalled and every one landed, so the cluster is committing"
    );

    let quiet = judged(vec![goes_quiet(), goes_quiet()]);
    assert!(
        matches!(quiet.report.outcome, Outcome::LivenessViolated(_)),
        "a cluster that stopped asking is silent whether or not syncs stall: {:?}",
        quiet.report.outcome
    );
}
