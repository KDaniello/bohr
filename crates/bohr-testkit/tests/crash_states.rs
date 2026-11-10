//! Durability is a promise about one file, and a crash leaves a state the disk was never in.
//!
//! Two disk-model properties, each tested on a defective and a correct twin.
//!
//! A sync covers only its own file, so syncing the log leaves the data file at risk.
//!
//! A crash decides each unsynced sector independently, so the result can be a state the disk never
//! passed through.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::{EventKind, FaultKind, Outcome};
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::{DurabilityConfig, NetworkConfig, NodeFaultKind, ScheduledNodeFault};

/// `openat`.
const OPENAT: u32 = 257;
/// `write`.
const WRITE: u32 = 1;
/// `pwrite64`, for rewriting a page in place.
const PWRITE64: u32 = 18;
/// `fsync`.
const FSYNC: u32 = 74;
/// `sendto`, standing in for an answer.
const SENDTO: u32 = 44;
/// `nanosleep`.
const SLEEP: u32 = 35;

/// `O_CREAT | O_RDWR`.
const OPEN_FLAGS: u64 = 0o100 | 2;

const DATA_PATH_AT: u64 = MEMORY_BASE;
const LOG_PATH_AT: u64 = MEMORY_BASE + 0x080;
const TIMESPEC_AT: u64 = MEMORY_BASE + 0x100;
const PAYLOAD_AT: u64 = MEMORY_BASE + 0x200;
const SOCKADDR_AT: u64 = MEMORY_BASE + 0x300;

/// The descriptors the prologue hands out, in the order it opens them.
const DATA_FD: u64 = 3;
const LOG_FD: u64 = 4;
const SOCKET_FD: u64 = 5;

/// A record long enough to span several sectors, so a per-sector crash has somewhere to land.
const RECORD_BYTES: u64 = 4096;

/// Pages the file is laid down as before anything rewrites it.
const PAGES: u64 = 8;

const KILL_AT: Tick = Tick(8 * Tick::MILLISECOND.get());
const RESTART_AT: Tick = Tick(12 * Tick::MILLISECOND.get());

/// Two files and a socket: what every twin below starts with.
fn prologue() -> Vec<GuestOp> {
    vec![
        GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, DATA_PATH_AT, OPEN_FLAGS, 0o644, 0, 0] },
        GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, LOG_PATH_AT, OPEN_FLAGS, 0o644, 0, 0] },
        // AF_INET, SOCK_DGRAM.
        GuestOp::Syscall { nr: 41, args: [2, 2, 0, 0, 0, 0] },
    ]
}

/// The defect: append to the data, sync the *log*, and answer as though the data were durable.
///
/// Appends, since the durability watcher marks file lengths (`docs/API.md` §5.6).
fn syncs_the_wrong_file() -> NullProgram {
    appending_twin(LOG_FD)
}

/// The same work, syncing the file the answer is about.
fn syncs_the_right_file() -> NullProgram {
    appending_twin(DATA_FD)
}

/// Append a record, sync `synced`, answer, pause — forty times over.
fn appending_twin(synced: u64) -> NullProgram {
    let mut ops = prologue();
    for _ in 0..40 {
        ops.push(GuestOp::Syscall {
            nr: WRITE,
            args: [DATA_FD, PAYLOAD_AT, RECORD_BYTES, 0, 0, 0],
        });
        ops.push(GuestOp::Syscall { nr: FSYNC, args: [synced, 0, 0, 0, 0, 0] });
        ops.push(GuestOp::Syscall {
            nr: SENDTO,
            args: [SOCKET_FD, PAYLOAD_AT, 8, 0, SOCKADDR_AT, 16],
        });
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }
    NullProgram::single(ops)
}

/// Lay the file down and make it durable, so what follows rewrites it in place.
///
/// A real store's shape: the length settles early and pages are rewritten in place.
fn established() -> Vec<GuestOp> {
    let mut ops = prologue();
    for _ in 0..PAGES {
        ops.push(GuestOp::Syscall {
            nr: WRITE,
            args: [DATA_FD, PAYLOAD_AT, RECORD_BYTES, 0, 0, 0],
        });
    }
    ops.push(GuestOp::Syscall { nr: FSYNC, args: [DATA_FD, 0, 0, 0, 0, 0] });
    ops
}

/// Rewrite a page, sync `synced`, answer, pause — round and round the file.
fn rewriting_twin(synced: u64) -> NullProgram {
    let mut ops = established();
    for round in 0..40u64 {
        let page = (round % PAGES) * RECORD_BYTES;
        ops.push(GuestOp::Syscall {
            nr: PWRITE64,
            args: [DATA_FD, PAYLOAD_AT, RECORD_BYTES, page, 0, 0],
        });
        ops.push(GuestOp::Syscall { nr: FSYNC, args: [synced, 0, 0, 0, 0, 0] });
        ops.push(GuestOp::Syscall {
            nr: SENDTO,
            args: [SOCKET_FD, PAYLOAD_AT, 8, 0, SOCKADDR_AT, 16],
        });
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }
    NullProgram::single(ops)
}

/// A node that writes and never reaches outward, so it is never accused of anything.
fn quiet() -> NullProgram {
    let mut ops = established();
    for round in 0..40u64 {
        let page = (round % PAGES) * RECORD_BYTES;
        ops.push(GuestOp::Syscall {
            nr: PWRITE64,
            args: [DATA_FD, PAYLOAD_AT, RECORD_BYTES, page, 0, 0],
        });
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }
    NullProgram::single(ops)
}

fn memory() -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&Tick::MILLISECOND.get().to_le_bytes());

    // `sockaddr_in` for node 1: AF_INET, port 7000, 10.66.0.2.
    let mut peer = vec![2, 0, 0x1b, 0x58, 10, 66, 0, 2];
    peer.extend_from_slice(&[0; 8]);

    let mut out = Vec::new();
    for node in 0..2u32 {
        out.push((node, DATA_PATH_AT, b"/store.db\0".to_vec()));
        out.push((node, LOG_PATH_AT, b"/store.log\0".to_vec()));
        out.push((node, TIMESPEC_AT, timespec.clone()));
        out.push((node, PAYLOAD_AT, vec![b'r'; RECORD_BYTES as usize]));
        out.push((node, SOCKADDR_AT, peer.clone()));
    }
    out
}

fn judge(first: NullProgram, seed: u64) -> Judged {
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
        durability: Some(DurabilityConfig),
        ..Scenario::default()
    };
    let config = RunConfig {
        horizon: Some(Tick(30 * Tick::MILLISECOND.get())),
        max_ticks: Tick(Tick::SECOND.get()),
        ..RunConfig::default()
    };
    judge_scenario(vec![first, quiet()], &memory(), scenario, &Tape::from_seed(Seed(seed)), config)
}

/// Whether the run ended as a durability finding.
fn lost(judged: &Judged) -> bool {
    matches!(judged.report.outcome, Outcome::DurabilityViolated(_))
}

/// The pair. Syncing the log does not make the data durable, and syncing the data does.
///
/// Over many seeds, since the crash is drawn: the defect is reached and the correct twin is safe on
/// all of them.
#[test]
fn a_sync_promises_nothing_about_a_file_it_was_not_asked_about() {
    let wrong = (1..=20).filter(|seed| lost(&judge(syncs_the_wrong_file(), *seed))).count();
    assert!(
        wrong > 0,
        "a node that synced its log and answered about its data must be able to lose the data"
    );

    let right = (1..=20).filter(|seed| lost(&judge(syncs_the_right_file(), *seed))).count();
    assert_eq!(
        right, 0,
        "a node that synced what it answered about kept its promise on every seed"
    );
}

/// A crash leaves a state the disk was never in: some sectors of a write landed and others did not.
///
/// Some seed tears a write.
#[test]
fn a_crash_can_leave_some_sectors_of_a_write_and_not_others() {
    let tore = |judged: &Judged| {
        judged
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::FaultFired { fault: FaultKind::DiskTear, .. }))
    };

    let seeds = 1..=40;
    let torn = seeds.clone().filter(|seed| tore(&judge(rewriting_twin(LOG_FD), *seed))).count();
    assert!(
        torn > 0,
        "no seed out of {} tore a write, so the per-sector landing is unreachable",
        seeds.count()
    );
}

/// A file cut down after a write, and then a crash.
///
/// `ftruncate` drops the not-durable marks above the new length, so the crash image never reads
/// past the file's end. Over many seeds, since the crash is drawn.
#[test]
fn a_file_cut_down_before_a_crash_does_not_take_the_supervisor_with_it() {
    /// `ftruncate`.
    const FTRUNCATE: u32 = 77;

    // Eight pages synced, the first rewritten, then the file truncated under those marks.
    let mut ops = established();
    ops.push(GuestOp::Syscall { nr: PWRITE64, args: [DATA_FD, PAYLOAD_AT, RECORD_BYTES, 0, 0, 0] });
    ops.push(GuestOp::Syscall { nr: FTRUNCATE, args: [DATA_FD, 100, 0, 0, 0, 0] });
    // Keep running so the scheduled kill has a target.
    for _ in 0..40 {
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }

    for seed in 0..24 {
        // The run returns.
        let judged = judge(NullProgram::single(ops.clone()), seed);
        assert!(
            judged.report.ticks.get() > 0,
            "seed {seed} produced a run that did nothing at all"
        );
    }
}
