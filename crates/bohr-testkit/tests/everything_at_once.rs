//! Every adversity this world draws, turned on together, still makes one run.
//!
//! Single-fault tests miss combinations, such as a full volume during crash recovery or a packet
//! to a dead node.
//!
//! Every armed kind reaches the guest, and the run is still a function of its tape.

use std::collections::BTreeSet;

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::{EventKind, FaultKind, PacketFate};
use bohr_types::{Seed, Tick};
use bohr_world::{DiskConfig, FaultPolicy, NetworkConfig, Targeting};

/// `openat`.
const OPENAT: u32 = 257;
/// `write`.
const WRITE: u32 = 1;
/// `fsync`.
const FSYNC: u32 = 74;
/// `socket`.
const SOCKET: u32 = 41;
/// `sendto`.
const SENDTO: u32 = 44;
/// `recvfrom`.
const RECVFROM: u32 = 45;
/// `bind`.
const BIND: u32 = 49;
/// `nanosleep`.
const SLEEP: u32 = 35;

/// `O_CREAT | O_RDWR`.
const OPEN_FLAGS: u64 = 0o100 | 2;
/// `AF_INET`, `SOCK_DGRAM`.
const INET_DGRAM: [u64; 6] = [2, 2, 0, 0, 0, 0];

const PATH_AT: u64 = MEMORY_BASE;
const TIMESPEC_AT: u64 = MEMORY_BASE + 0x100;
const PAYLOAD_AT: u64 = MEMORY_BASE + 0x200;
const SOCKADDR_AT: u64 = MEMORY_BASE + 0x300;

const DATA_FD: u64 = 3;
const SOCKET_FD: u64 = 4096;
const INBOX_AT: u64 = MEMORY_BASE + 0x1200;

/// Writes, syncs and works the socket, over and over — enough for every model to act on.
///
/// `listening` binds and reads, since duplicates and reorders are narrated only at delivery.
fn works_and_talks(listening: bool) -> NullProgram {
    let mut ops = vec![
        GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, PATH_AT, OPEN_FLAGS, 0o644, 0, 0] },
        GuestOp::Syscall { nr: SOCKET, args: INET_DGRAM },
    ];
    if listening {
        ops.push(GuestOp::Syscall { nr: BIND, args: [SOCKET_FD, SOCKADDR_AT, 16, 0, 0, 0] });
    }
    for _ in 0..120 {
        ops.push(GuestOp::Syscall { nr: WRITE, args: [DATA_FD, PAYLOAD_AT, 4096, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: FSYNC, args: [DATA_FD, 0, 0, 0, 0, 0] });
        if listening {
            ops.push(GuestOp::Syscall { nr: RECVFROM, args: [SOCKET_FD, INBOX_AT, 64, 0, 0, 0] });
        } else {
            ops.push(GuestOp::Syscall {
                nr: SENDTO,
                args: [SOCKET_FD, PAYLOAD_AT, 64, 0, SOCKADDR_AT, 16],
            });
        }
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }
    NullProgram::single(ops)
}

fn memory() -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&(Tick::MILLISECOND.get() / 4).to_le_bytes());

    // `sockaddr_in` for node 0: AF_INET, port 7000, 10.66.0.1.
    let mut address = vec![2, 0, 0x1b, 0x58, 10, 66, 0, 1];
    address.extend_from_slice(&[0; 8]);

    let mut out = Vec::new();
    for node in 0..2u32 {
        out.push((node, PATH_AT, b"/data\0".to_vec()));
        out.push((node, TIMESPEC_AT, timespec.clone()));
        out.push((node, PAYLOAD_AT, vec![b'x'; 4096]));
        out.push((node, SOCKADDR_AT, address.clone()));
    }
    out
}

/// Everything this world draws, at odds short enough that a run of this length meets each.
fn all_of_it() -> Scenario {
    Scenario {
        network: NetworkConfig {
            nodes: 2,
            latency_mean_ticks: 40_000,
            latency_jitter_ticks: 20_000,
            drop_probability: (1, 8),
            duplicate_probability: (1, 8),
            reorder_probability: (1, 8),
            ..NetworkConfig::default()
        },
        disk: DiskConfig {
            stall_probability: (1, 8),
            stall_mean_ticks: 200_000,
            eio_probability: (1, 16),
            enospc_probability: (1, 16),
            snapshot_depth: 4,
            reorder_unsynced: true,
        },
        fault_policy: FaultPolicy {
            kill: (1, 64),
            enomem: (0, 0),
            targeting: Targeting::Guided,
            restart_after: Tick(2 * Tick::MILLISECOND.get()),
            arm_after: Tick(0),
            disarm_after: None,
            suppressed: bohr_world::Suppressed::NONE,
            budget_per_run: None,
            kinds: bohr_world::DrawnFaults::default(),
        },
        ..Scenario::default()
    }
}

fn run(seed: u64) -> Judged {
    judge_scenario(
        vec![works_and_talks(true), works_and_talks(false)],
        &memory(),
        all_of_it(),
        &Tape::from_seed(Seed(seed)),
        RunConfig { horizon: Some(Tick(120 * Tick::MILLISECOND.get())), ..RunConfig::default() },
    )
}

/// Every kind of adversity the run narrated, in both vocabularies.
///
/// Both faults and packet fates, which the guest cannot tell apart.
fn kinds(judged: &Judged) -> BTreeSet<String> {
    judged
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultFired { fault, .. } => Some(format!("{fault:?}")),
            EventKind::PacketFate { fate, .. } if *fate != PacketFate::Delivered => {
                Some(format!("{fate:?}"))
            }
            _ => None,
        })
        .collect()
}

/// Every armed kind reaches the guest.
#[test]
fn every_drawn_adversity_lands_in_one_run() {
    // Across seeds, since one seed may miss a kind.
    let seen: BTreeSet<String> = (1..=6).flat_map(|seed| kinds(&run(seed))).collect();

    // The drawable kinds. `NetPartition`, `NodeFreeze`, `NodeThaw`, `NodeRestart` and `ChainReorg`
    // fire only when scheduled.
    let wanted = [
        format!("{:?}", FaultKind::DiskError),
        format!("{:?}", FaultKind::DiskFull),
        format!("{:?}", FaultKind::DiskTear),
        format!("{:?}", FaultKind::NodeKill),
        format!("{:?}", PacketFate::Dropped),
        format!("{:?}", PacketFate::Duplicated),
    ];
    for name in wanted {
        assert!(seen.contains(&name), "{name} never landed; six seeds saw {seen:?}");
    }
}

/// And with all of them at once the run is still a function of its tape.
///
/// Checked on the suite's hardest scenario.
#[test]
fn everything_at_once_still_repeats() {
    let once = run(3);
    let again = run(3);
    assert_eq!(once.report.digest, again.report.digest, "one tape produced two runs");
    assert_eq!(once.report.adversity, again.report.adversity);
    assert_eq!(kinds(&once), kinds(&again));
}
