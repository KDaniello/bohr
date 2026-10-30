//! What the kernel says about a process survives a crash, because it was never on the platter.
//!
//! A guest asks the system about itself through files under `/proc`, and this world answers by
//! making one. Held on the modelled disk they would be snapshotted, rolled back and torn like
//! anything else — so a crash would take away the process's own entry, and a guest that came back
//! and asked who it was would be told it does not exist.
//!
//! That is not a state any machine reaches, and a real client walked into it: an execution client
//! panics on `expect("own process")` after a crash, which looked like its defect and was this
//! world's. Recorded as a pair, because a run that read `/proc` and never crashed would pass either
//! way.

use bohr_backend_null::{GuestOp, NullProgram, MEMORY_BASE};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{judge_scenario, Judged, Scenario};
use bohr_trace::{DiskOpKind, EventKind};
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::{NetworkConfig, NodeFaultKind, ScheduledNodeFault};

/// `openat`.
const OPENAT: u32 = 257;
/// `read`.
const READ: u32 = 0;
/// `write`.
const WRITE: u32 = 1;
/// `fsync`.
const FSYNC: u32 = 74;
/// `nanosleep`.
const SLEEP: u32 = 35;

/// `O_CREAT | O_RDWR`, for the ordinary file this guest also keeps.
const OPEN_FLAGS: u64 = 0o100 | 2;

const PROC_PATH_AT: u64 = MEMORY_BASE;
const DATA_PATH_AT: u64 = MEMORY_BASE + 0x080;
const TIMESPEC_AT: u64 = MEMORY_BASE + 0x100;
const INBOX_AT: u64 = MEMORY_BASE + 0x200;
const PAYLOAD_AT: u64 = MEMORY_BASE + 0x400;

/// The descriptors this script hands out, in the order it opens them.
const PROC_FD: u64 = 3;
const DATA_FD: u64 = 4;

const KILL_AT: Tick = Tick(8 * Tick::MILLISECOND.get());
const RESTART_AT: Tick = Tick(12 * Tick::MILLISECOND.get());

/// Ask who it is, then make an ordinary file durable, then wait to be killed.
///
/// **The order is the whole test.** A namespace goes into the durable image whole at every sync,
/// so the sync after the `/proc` open is what puts that *name* on the disk — while its content
/// never goes there, because nothing ever syncs `/proc`. A crash then restores a name with nothing
/// behind it, and the guest that comes back and asks again reads nought.
///
/// A restarted node runs its program from the beginning, so the second asking is the same two
/// calls reached a second time rather than two more written here.
fn asks_who_it_is() -> NullProgram {
    let mut ops = vec![
        GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, PROC_PATH_AT, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: READ, args: [PROC_FD, INBOX_AT, 256, 0, 0, 0] },
        GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, DATA_PATH_AT, OPEN_FLAGS, 0o644, 0, 0] },
        GuestOp::Syscall { nr: WRITE, args: [DATA_FD, PAYLOAD_AT, 64, 0, 0, 0] },
        GuestOp::Syscall { nr: FSYNC, args: [DATA_FD, 0, 0, 0, 0, 0] },
    ];
    // Long enough to still be here when the kill and the restart arrive.
    ops.extend((0..40).map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] }));
    NullProgram::single(ops)
}

fn memory() -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&Tick::MILLISECOND.get().to_le_bytes());
    vec![
        (0, PROC_PATH_AT, b"/proc/self/stat\0".to_vec()),
        (0, DATA_PATH_AT, b"/data\0".to_vec()),
        (0, TIMESPEC_AT, timespec),
        (0, PAYLOAD_AT, vec![b'x'; 64]),
    ]
}

fn judge(faults: Vec<ScheduledNodeFault>) -> Judged {
    let scenario = Scenario {
        network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
        node_faults: faults,
        ..Scenario::default()
    };
    judge_scenario(
        vec![asks_who_it_is()],
        &memory(),
        scenario,
        &Tape::from_seed(Seed(1)),
        RunConfig { horizon: Some(Tick(64 * Tick::MILLISECOND.get())), ..RunConfig::default() },
    )
}

/// What each read of `/proc` came back with, in order.
fn proc_reads(judged: &Judged) -> Vec<i64> {
    judged
        .events
        .iter()
        .filter_map(|e| match e.kind {
            // The disk row rather than the syscall row: it says what the model actually handed
            // over, which is the thing under test.
            EventKind::DiskOp { op: DiskOpKind::Read, ret, .. } if e.node == NodeId::ZERO => {
                Some(ret)
            }
            _ => None,
        })
        .collect()
}

/// The claim: a crash does not take away what the disk never held.
#[test]
fn a_crash_leaves_what_the_kernel_says_about_the_process() {
    let crashed = judge(vec![
        ScheduledNodeFault { at_tick: KILL_AT, node: NodeId::ZERO, kind: NodeFaultKind::Kill },
        ScheduledNodeFault {
            at_tick: RESTART_AT,
            node: NodeId::ZERO,
            kind: NodeFaultKind::Restart,
        },
    ]);
    let reads = proc_reads(&crashed);
    assert!(
        reads.iter().all(|got| *got > 0),
        "a read of /proc came back empty after a crash: {reads:?}"
    );
}

/// And without the crash, so the pair means something.
#[test]
fn the_same_run_without_a_crash_reads_it_too() {
    let reads = proc_reads(&judge(Vec::new()));
    assert!(reads.iter().all(|got| *got > 0), "a read of /proc came back empty: {reads:?}");
}
