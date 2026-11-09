//! Every adversity this world draws, turned on together, still makes one run.
//!
//! Each of them has a test of its own, and each of those turns on one thing. What none of them
//! asks is whether they **compose**: a volume that reports itself full while a crash is restoring
//! what a sync promised, a packet dropped to a node that is dead, a stall outliving the node that
//! started it. Those are the states a campaign spends most of its time in and the ones no
//! single-fault test visits.
//!
//! Two claims, and the second is the one that matters:
//!
//! **They all reach the guest.** A scenario that armed six kinds and landed one would pass any
//! check that only asked whether the run finished.
//!
//! **The run is still a function of its tape.** Determinism is the one promise this tool makes, and
//! the place it is most likely to break is where several models act on one moment — so it is
//! asserted here rather than assumed from the tests that arm one thing each.

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
/// `listening` binds and reads, which is what makes a duplicate or a reordering visible at all:
/// both are narrated at delivery, so a packet nobody is waiting for can only ever be recorded as
/// dropped.
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
        },
        fault_policy: FaultPolicy {
            kill: (1, 64),
            enomem: (0, 0),
            targeting: Targeting::Guided,
            restart_after: Tick(2 * Tick::MILLISECOND.get()),
            arm_after: Tick(0),
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
/// The world says two different things and the guest cannot tell them apart: a **fault** is
/// something the world was told to do to it, and a **packet fate** is the medium being the medium.
/// A test that read only the first would call a lossy link a quiet one.
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

/// They all reach the guest, rather than one of them crowding the others out.
#[test]
fn every_drawn_adversity_lands_in_one_run() {
    // Several seeds, because each kind is drawn and one seed is entitled to miss one of them.
    // What is being shown is that arming them together lets each through, not that any single run
    // must meet all six.
    let seen: BTreeSet<String> = (1..=6).flat_map(|seed| kinds(&run(seed))).collect();

    // Every kind a scenario can *draw*, which is a shorter list than the catalogue and that is the
    // point of stating it here. `NetPartition`, `NodeFreeze`, `NodeThaw`, `NodeRestart` and
    // `ChainReorg` are in the model's catalogue and cannot be drawn at all: they exist only where
    // somebody wrote a tick down, so a campaign never searches them. This test is where that gap
    // is recorded, and it is the list that will shrink when they become drawable.
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
/// The promise most likely to break where several models act on one moment, so it is asserted
/// against the hardest scenario this suite has rather than against the easiest.
#[test]
fn everything_at_once_still_repeats() {
    let once = run(3);
    let again = run(3);
    assert_eq!(once.report.digest, again.report.digest, "one tape produced two runs");
    assert_eq!(once.report.adversity, again.report.adversity);
    assert_eq!(kinds(&once), kinds(&again));
}
