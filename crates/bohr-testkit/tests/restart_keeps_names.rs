//! A restarted node finds its names as a fresh boot would: a synced link is still there, and the
//! process knows the program it runs.
//!
//! A link is a name on the disk like any other, synced with the namespace; `/proc/self/exe` is
//! the kernel's, and a new process of the same program reads the same path.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::EventKind;
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::{NetworkConfig, NodeFaultKind, ScheduledNodeFault};

const WRITE: u32 = 1;
const SLEEP: u32 = 35;
const FSYNC: u32 = 74;
const SYMLINK: u32 = 88;
const READLINK: u32 = 89;
const OPENAT: u32 = 257;

const OPEN_FLAGS: u64 = 0o100 | 2;
const EEXIST: i64 = -17;
const DATA_FD: u64 = 3;

const TARGET_AT: u64 = MEMORY_BASE;
const LINK_AT: u64 = MEMORY_BASE + 0x40;
const DATA_AT: u64 = MEMORY_BASE + 0x80;
const EXE_AT: u64 = MEMORY_BASE + 0xC0;
const TIMESPEC_AT: u64 = MEMORY_BASE + 0x100;
const INBOX_AT: u64 = MEMORY_BASE + 0x200;

/// The path the node's program is started from.
const PROGRAM: &str = "/opt/node/bin/server";

const KILL_AT: Tick = Tick(8 * Tick::MILLISECOND.get());
const RESTART_AT: Tick = Tick(12 * Tick::MILLISECOND.get());

/// Link, sync any file so the namespace is durable, ask which program runs, then wait.
///
/// A restarted node runs the same script from its start.
fn links_and_asks() -> NullProgram {
    let mut ops = vec![
        GuestOp::Syscall { nr: SYMLINK, args: [TARGET_AT, LINK_AT, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: OPENAT, args: [u64::MAX - 99, DATA_AT, OPEN_FLAGS, 0o644, 0, 0] },
        GuestOp::Syscall { nr: WRITE, args: [DATA_FD, TARGET_AT, 8, 0, 0, 0] },
        GuestOp::Syscall { nr: FSYNC, args: [DATA_FD, 0, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: READLINK, args: [EXE_AT, INBOX_AT, 256, 0, 0, 0] },
    ];
    ops.extend((0..40).map(|_| GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] }));
    NullProgram::single(ops)
}

fn judge() -> Judged {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&Tick::MILLISECOND.get().to_le_bytes());
    let memory = vec![
        (0, TARGET_AT, b"/tmp/release-2\0".to_vec()),
        (0, LINK_AT, b"/tmp/current\0".to_vec()),
        (0, DATA_AT, b"/tmp/data\0".to_vec()),
        (0, EXE_AT, b"/proc/self/exe\0".to_vec()),
        (0, TIMESPEC_AT, timespec),
    ];
    let scenario = Scenario {
        network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
        node_faults: vec![
            ScheduledNodeFault { at_tick: KILL_AT, node: NodeId::ZERO, kind: NodeFaultKind::Kill },
            ScheduledNodeFault {
                at_tick: RESTART_AT,
                node: NodeId::ZERO,
                kind: NodeFaultKind::Restart,
            },
        ],
        programs: vec![PROGRAM.to_owned()],
        ..Scenario::default()
    };
    judge_scenario(
        vec![links_and_asks()],
        &memory,
        scenario,
        &Tape::from_seed(Seed(1)),
        RunConfig { horizon: Some(Tick(64 * Tick::MILLISECOND.get())), ..RunConfig::default() },
    )
}

fn answers(judged: &Judged, nr: u32) -> Vec<i64> {
    judged
        .events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Syscall { nr: n, ret, .. } if n == nr && e.node == NodeId::ZERO => Some(ret),
            _ => None,
        })
        .collect()
}

/// The restarted process finds the link it synced and the program it runs.
#[test]
fn a_restarted_node_finds_its_link_and_its_program() {
    let judged = judge();
    assert_eq!(answers(&judged, SYMLINK), vec![0, EEXIST], "the synced link did not survive");
    let exe = PROGRAM.len() as i64;
    assert_eq!(answers(&judged, READLINK), vec![exe, exe], "/proc/self/exe after the restart");
}
