//! A full disk refuses new space, never space a file already holds.
//!
//! On ext4 an overwrite of what a file holds, or of what `fallocate` reserved, allocates nothing
//! and cannot fail for space. A write past both, and `fallocate` itself, can. Checked over many
//! seeds of a disk that is full half the time it is asked.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Scenario, judge_scenario};
use bohr_trace::EventKind;
use bohr_types::{NodeId, Seed, Tick};
use bohr_world::{DiskConfig, NetworkConfig};

const PWRITE64: u32 = 18;
const FTRUNCATE: u32 = 77;
const OPENAT: u32 = 257;
const FALLOCATE: u32 = 285;

const O_CREAT_RDWR: u64 = 0o100 | 2;
const KEEP_SIZE: u64 = 1;
const ENOSPC: i64 = -28;
const FD: u64 = 3;

const PATH_AT: u64 = MEMORY_BASE;
const DATA_AT: u64 = MEMORY_BASE + 0x100;

fn call(nr: u32, args: [u64; 6]) -> GuestOp {
    GuestOp::Syscall { nr, args }
}

/// What each call answered under one seed, the open and the truncate left out.
///
/// The file holds 64 bytes from a truncate, which asks for no space. Then: an overwrite inside
/// them, a write past them, a reservation of 64 more past them, a write inside that reservation.
fn answers(seed: u64) -> [i64; 4] {
    let ops = vec![
        call(OPENAT, [u64::MAX - 99, PATH_AT, O_CREAT_RDWR, 0o644, 0, 0]),
        call(FTRUNCATE, [FD, 64, 0, 0, 0, 0]),
        call(PWRITE64, [FD, DATA_AT, 16, 40, 0, 0]),
        call(PWRITE64, [FD, DATA_AT, 16, 64, 0, 0]),
        call(FALLOCATE, [FD, KEEP_SIZE, 80, 64, 0, 0]),
        call(PWRITE64, [FD, DATA_AT, 16, 100, 0, 0]),
        GuestOp::Exit(0),
    ];
    let memory = vec![(0, PATH_AT, b"/tmp/wal\0".to_vec()), (0, DATA_AT, vec![b'x'; 16])];
    let scenario = Scenario {
        network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
        disk: DiskConfig { enospc_probability: (1, 2), ..DiskConfig::default() },
        ..Scenario::default()
    };
    let config = RunConfig { max_ticks: Tick(Tick::SECOND.get()), ..RunConfig::default() };
    let judged = judge_scenario(
        vec![NullProgram::single(ops)],
        &memory,
        scenario,
        &Tape::from_seed(Seed(seed)),
        config,
    );
    let rets: Vec<i64> = judged
        .events
        .iter()
        .filter(|e| e.node == NodeId(0))
        .filter_map(|e| match e.kind {
            EventKind::Syscall { ret, .. } => Some(ret),
            _ => None,
        })
        .collect();
    [rets[2], rets[3], rets[4], rets[5]]
}

/// An overwrite and a write into reserved space never find the disk full; new space sometimes
/// does.
#[test]
fn a_full_disk_refuses_only_new_space() {
    let runs: Vec<[i64; 4]> = (0..200).map(answers).collect();
    for (seed, [inside, past, reserve, reserved]) in runs.iter().enumerate() {
        assert_eq!(*inside, 16, "seed {seed}: an overwrite of what the file holds was refused");
        assert!(*past == 16 || *past == ENOSPC, "seed {seed}: {past}");
        if *reserve == 0 {
            assert_eq!(*reserved, 16, "seed {seed}: a write into reserved space was refused");
        }
    }
    assert!(runs.iter().any(|r| r[1] == ENOSPC), "no write past the file found the disk full");
    assert!(runs.iter().any(|r| r[2] == ENOSPC), "no reservation found the disk full");
    assert!(runs.iter().any(|r| r[2] == 0), "no reservation was granted, so none was written into");
}
