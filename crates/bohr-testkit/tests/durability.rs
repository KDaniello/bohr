//! A node that answered before it synced, and came back without the data.
//!
//! This is the `fsyncgate` shape, and the version of it that matters here is the *quiet* one: the
//! guest does not check itself, does not assert, does not exit. `testdata/bugs/kv_fsync` catches
//! itself out and so is found by the crash oracle already; a real client does not, and what has to
//! find it is the boundary.
//!
//! The pair is the claim. The defective node answers with writes behind it and loses them; the
//! correct twin does the same work in the other order — sync, then answer — on the same scenario,
//! the same seed and the same kill. An oracle that fired on both would satisfy "caught on a seeded
//! defect" and mean nothing.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::{EventKind, Outcome};
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::{DurabilityConfig, NetworkConfig, NodeFaultKind, ScheduledNodeFault};

/// `openat`.
const OPENAT: u32 = 257;
/// `write`.
const WRITE: u32 = 1;
/// `fsync`.
const FSYNC: u32 = 74;
/// `sendto`: the node telling somebody something, which is what stands in for an answer.
const SENDTO: u32 = 44;
/// `nanosleep`.
const SLEEP: u32 = 35;

/// `O_CREAT | O_RDWR`.
const OPEN_FLAGS: u64 = 0o100 | 2;

const PATH_AT: u64 = MEMORY_BASE;
const TIMESPEC_AT: u64 = MEMORY_BASE + 0x100;
const PAYLOAD_AT: u64 = MEMORY_BASE + 0x200;
/// Where the peer's address is placed, for the send that stands in for an answer.
const SOCKADDR_AT: u64 = MEMORY_BASE + 0x300;

/// When the node is killed, and when it comes back.
const KILL_AT: Tick = Tick(8 * Tick::MILLISECOND.get());
const RESTART_AT: Tick = Tick(12 * Tick::MILLISECOND.get());

/// The prologue every twin shares: a log to write to, and a socket to answer on.
fn prologue() -> Vec<GuestOp> {
    vec![
        GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, PATH_AT, OPEN_FLAGS, 0o644, 0, 0] },
        // AF_INET, SOCK_DGRAM.
        GuestOp::Syscall { nr: 41, args: [2, 2, 0, 0, 0, 0] },
    ]
}

/// The defect: write, answer, and make it durable a moment later.
///
/// The moment matters and is the whole shape of the bug. A node that answered and synced in the
/// same breath loses nothing, because there is no window for a crash to land in; the defect is
/// that the answer goes out and the sync happens *afterwards*, so the window is however long the
/// node takes to get round to it.
fn answers_before_syncing() -> NullProgram {
    let mut ops = prologue();
    for _ in 0..40 {
        ops.push(GuestOp::Syscall { nr: WRITE, args: [3, PAYLOAD_AT, 8, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: SENDTO, args: [4, PAYLOAD_AT, 8, 0, SOCKADDR_AT, 16] });
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: FSYNC, args: [3, 0, 0, 0, 0, 0] });
    }
    NullProgram::single(ops)
}

/// The same work with the same pauses, in the order that keeps the promise: sync, then answer.
fn syncs_before_answering() -> NullProgram {
    let mut ops = prologue();
    for _ in 0..40 {
        ops.push(GuestOp::Syscall { nr: WRITE, args: [3, PAYLOAD_AT, 8, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: FSYNC, args: [3, 0, 0, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: SENDTO, args: [4, PAYLOAD_AT, 8, 0, SOCKADDR_AT, 16] });
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }
    NullProgram::single(ops)
}

/// A node that answers nobody: it writes and syncs and never reaches outward.
fn never_answers() -> NullProgram {
    let mut ops = prologue();
    for _ in 0..40 {
        ops.push(GuestOp::Syscall { nr: WRITE, args: [3, PAYLOAD_AT, 8, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }
    NullProgram::single(ops)
}

/// What every node reads out of its own memory: a path, a sleep, a payload and a peer.
fn memory() -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&Tick::MILLISECOND.get().to_le_bytes());

    // `sockaddr_in` for node 1: AF_INET, port 7000, 10.66.0.2.
    let mut peer = vec![2, 0, 0x1b, 0x58, 10, 66, 0, 2];
    peer.extend_from_slice(&[0; 8]);

    let mut out = Vec::new();
    for node in 0..2u32 {
        out.push((node, PATH_AT, b"/store.db\0".to_vec()));
        out.push((node, TIMESPEC_AT, timespec.clone()));
        out.push((node, PAYLOAD_AT, b"a record".to_vec()));
        out.push((node, SOCKADDR_AT, peer.clone()));
    }
    out
}

fn judge(first: NullProgram, watching: bool) -> Judged {
    let scenario = Scenario {
        network: NetworkConfig { nodes: 2, ..NetworkConfig::default() },
        node_faults: vec![
            ScheduledNodeFault { at_tick: KILL_AT, node: NodeId(0), kind: NodeFaultKind::Kill },
            ScheduledNodeFault {
                at_tick: RESTART_AT,
                node: NodeId(0),
                kind: NodeFaultKind::Restart,
            },
        ],
        durability: watching.then_some(DurabilityConfig),
        ..Scenario::default()
    };
    let config = RunConfig {
        horizon: Some(Tick(30 * Tick::MILLISECOND.get())),
        max_ticks: Tick(Tick::SECOND.get()),
        ..RunConfig::default()
    };
    judge_scenario(
        vec![first, never_answers()],
        &memory(),
        scenario,
        &Tape::from_seed(Seed(3)),
        config,
    )
}

fn lost_on(judged: &Judged) -> Vec<NodeId> {
    judged
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic { code, .. } if code == bohr_trace::DURABILITY_CODE => {
                Some(event.node)
            }
            _ => None,
        })
        .collect()
}

/// The claim, in the only form that is falsifiable.
#[test]
fn a_node_that_answered_before_syncing_is_caught_and_one_that_did_not_is_free() {
    let broken = judge(answers_before_syncing(), true);
    assert!(
        matches!(broken.report.outcome, Outcome::DurabilityViolated(_)),
        "an answer the disk did not keep is a finding: {:?}",
        broken.report.outcome
    );
    assert_eq!(lost_on(&broken), vec![NodeId(0)]);

    let correct = judge(syncs_before_answering(), true);
    assert_eq!(
        correct.report.outcome,
        Outcome::Clean,
        "the same work in the order that keeps the promise is not a finding"
    );
    assert!(lost_on(&correct).is_empty());
}

/// A node that never told anybody anything cannot have broken a promise, however much it lost.
#[test]
fn a_node_that_answered_nobody_is_never_accused() {
    let judged = judge(never_answers(), true);
    assert_eq!(judged.report.outcome, Outcome::Clean);
    assert!(lost_on(&judged).is_empty());
}

/// Where a node said yes is on the timeline, because a reader of a report has to be able to see it
/// — and as a domain event rather than a diagnostic, because a diagnostic is what the oracle judges
/// on and an answer by itself is not a finding.
#[test]
fn an_answer_is_visible_on_the_timeline_without_being_a_finding() {
    // The kinds inside the distributed model's namespace, as `docs/API.md` §5.6 records them. By
    // number here rather than by name because this test reads the timeline the way any other
    // reader would: from outside the crate that wrote it.
    const ANSWERED: u32 = 1;

    let answers = |judged: &Judged| {
        judged
            .events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Domain { kind: ANSWERED, .. }))
            .count()
    };

    let broken = judge(answers_before_syncing(), true);
    assert!(answers(&broken) > 0, "every answer with writes behind it is on the timeline");

    let correct = judge(syncs_before_answering(), true);
    assert_eq!(correct.report.outcome, Outcome::Clean);
    assert_eq!(
        answers(&correct),
        0,
        "a node that synced first had nothing outstanding when it answered"
    );
}

/// What was at risk when a crash landed is on the timeline as a number, and it is not a verdict.
///
/// The number is what tells a campaign whether its crashes asked anything. A thousand clean runs
/// whose crashes all fell on a node with nothing outstanding are a thousand runs that asked
/// nothing, and no other part of a run distinguishes them from a thousand that did.
#[test]
fn what_a_crash_put_at_risk_is_recorded_whether_or_not_anything_was_lost() {
    const EXPOSED: u32 = 3;

    let exposure = |judged: &Judged| -> Vec<(u32, u64)> {
        judged
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Domain { kind: EXPOSED, payload, .. } if payload.len() == 12 => {
                    let files = u32::from_le_bytes(payload[..4].try_into().ok()?);
                    let bytes = u64::from_le_bytes(payload[4..].try_into().ok()?);
                    Some((files, bytes))
                }
                _ => None,
            })
            .collect()
    };

    let broken = judge(answers_before_syncing(), true);
    let at_risk = exposure(&broken);
    assert!(!at_risk.is_empty(), "a crash records what it took");
    assert!(
        at_risk.iter().any(|(files, bytes)| *files > 0 && *bytes > 0),
        "the node had answered for something it had not synced: {at_risk:?}"
    );

    // The same crash on a node that synced first: recorded, and recorded as nothing at risk. The
    // event has to be there either way, or a campaign could not tell a crash that asked nothing
    // from a crash that never happened.
    let correct = judge(syncs_before_answering(), true);
    assert_eq!(exposure(&correct), vec![(0, 0)]);
}

/// Nothing is watched unless a scenario asks for it, which is what keeps stored bundles replaying.
#[test]
fn a_scenario_that_does_not_watch_records_nothing() {
    let plain = judge(answers_before_syncing(), false);
    assert_eq!(plain.report.outcome, Outcome::Clean);
    assert!(lost_on(&plain).is_empty());
}

/// The same scenario at the same seed is the same run, finding and all.
#[test]
fn a_watched_run_reproduces() {
    let first = judge(answers_before_syncing(), true);
    for _ in 0..50 {
        let again = judge(answers_before_syncing(), true);
        assert_eq!(again.report.digest, first.report.digest);
        assert_eq!(again.report.outcome, first.report.outcome);
    }
}
