//! Durability is a promise about one file, and a crash leaves a state the disk was never in.
//!
//! Two properties of the disk model, each stated as a pair. Neither can be shown by a single run:
//! an oracle that fired on the correct twin as well as the defective one would satisfy any claim
//! made about it and mean nothing.
//!
//! **A sync promises nothing about a file it was not asked about.** A store that writes its data
//! and syncs its log has made its log durable and nothing else — which is the whole reason a
//! write-ahead log is worth having, and the whole reason a store that syncs the wrong descriptor
//! loses data. A model that let one sync cover the disk would keep the data file's writes for
//! free and would never reach the defect.
//!
//! **A crash decides each sector for itself.** Between a file's last sync and the power going, each
//! sector it was written on either landed or did not. What comes back is therefore not a moment the
//! disk ever passed through, and a model that restored some earlier moment would only ever produce
//! states the store had already survived once.

use bohr_backend_null::{GuestOp, NullProgram, MEMORY_BASE};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{judge_scenario, Judged, Scenario};
use bohr_trace::{EventKind, FaultKind, Outcome};
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::{DurabilityConfig, NetworkConfig, NodeFaultKind, ScheduledNodeFault};

/// `openat`.
const OPENAT: u32 = 257;
/// `write`.
const WRITE: u32 = 1;
/// `pwrite64`: a write at a named offset, which is how a store rewrites a page in place.
const PWRITE64: u32 = 18;
/// `fsync`.
const FSYNC: u32 = 74;
/// `sendto`: the node telling somebody something, which is what stands in for an answer.
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
/// Appending because what shows the loss is the durability watcher, and a mark is a file length: a
/// store that only ever rewrote in place would lose data without its file ever going short, which
/// is a limit of the watcher rather than of the disk (`docs/API.md` §5.6).
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

/// Lay the file down and make it durable, so what follows rewrites rather than grows it.
///
/// The shape a store actually has: a file whose length settles early and whose pages are then
/// rewritten in place. Growing it instead would test the one axis a length does not have — a size
/// survives whole, so an extending write is all or nothing and no sector of it ever decides alone.
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
/// Over a spread of seeds because the crash's landing is drawn: the defect has to be reachable, and
/// the correct twin has to be safe on every one of them rather than on a lucky one.
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
/// The claim the whole model rests on. A crash that only ever restored an earlier moment would
/// produce nothing a store had not already survived, and no seed would ever tear anything.
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
/// **The shape that panicked the supervisor.** A write marks its sectors as not yet durable; a
/// later `ftruncate` takes the bytes above the new length but used to leave the marks behind. The
/// crash image then copied a sector out of a file shorter than the mark said it was — an index past
/// the end, in the sans-io core, from a guest doing nothing unusual and one drawn crash. A panic
/// there destroys the run it exists to record, and the audit that names an impossible image runs
/// after the arithmetic that panicked.
///
/// Over a spread of seeds because where a crash lands is drawn, and the sector that mattered has to
/// be one the draw reaches.
#[test]
fn a_file_cut_down_before_a_crash_does_not_take_the_supervisor_with_it() {
    /// `ftruncate`.
    const FTRUNCATE: u32 = 77;

    // Eight pages written and promised, then the first page rewritten -- which marks sectors
    // *below* what is durable -- and then the file cut down under those marks.
    let mut ops = established();
    ops.push(GuestOp::Syscall { nr: PWRITE64, args: [DATA_FD, PAYLOAD_AT, RECORD_BYTES, 0, 0, 0] });
    ops.push(GuestOp::Syscall { nr: FTRUNCATE, args: [DATA_FD, 100, 0, 0, 0, 0] });
    // And then go on living, so the scheduled kill has a node to arrive at.
    for _ in 0..40 {
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }

    for seed in 0..24 {
        // The claim is that this returns at all: before, one of these seeds took the supervisor
        // down inside the crash image rather than reporting anything about the guest.
        let judged = judge(NullProgram::single(ops.clone()), seed);
        assert!(
            judged.report.ticks.get() > 0,
            "seed {seed} produced a run that did nothing at all"
        );
    }
}
