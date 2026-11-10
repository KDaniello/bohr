//! Drawn faults can be held back until the guest is up.
//!
//! A kill is drawn per eligible call, and an execution client makes most of its calls starting up,
//! so even 1/65536 odds kill it before it is up. Held back, the same odds reach the working node.
//!
//! The draw still happens while held back, so tape consumption never depends on time.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::{EventKind, FaultKind};
use bohr_types::{Seed, Tick};
use bohr_world::{FaultPolicy, NetworkConfig, Targeting};

/// `openat`.
const OPENAT: u32 = 257;
/// `write`.
const WRITE: u32 = 1;
/// `nanosleep`.
const SLEEP: u32 = 35;

/// `O_CREAT | O_RDWR`.
const OPEN_FLAGS: u64 = 0o100 | 2;

const PATH_AT: u64 = MEMORY_BASE;
const TIMESPEC_AT: u64 = MEMORY_BASE + 0x100;
const PAYLOAD_AT: u64 = MEMORY_BASE + 0x200;

const DATA_FD: u64 = 3;

/// Where a run held back until would start offering kills.
const ARM_AT: Tick = Tick(20 * Tick::MILLISECOND.get());

/// Writes without syncing, for ever, so a kill can fire at any moment.
fn writes_for_ever() -> NullProgram {
    let mut ops =
        vec![GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, PATH_AT, OPEN_FLAGS, 0o644, 0, 0] }];
    for _ in 0..400 {
        ops.push(GuestOp::Syscall { nr: WRITE, args: [DATA_FD, PAYLOAD_AT, 64, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }
    NullProgram::single(ops)
}

fn memory() -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&(Tick::MILLISECOND.get() / 10).to_le_bytes());
    vec![
        (0, PATH_AT, b"/data\0".to_vec()),
        (0, TIMESPEC_AT, timespec),
        (0, PAYLOAD_AT, vec![b'x'; 64]),
    ]
}

fn run(arm_after: Tick) -> Judged {
    let scenario = Scenario {
        network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
        fault_policy: FaultPolicy {
            kill: (1, 64),
            enomem: (0, 0),
            targeting: Targeting::Guided,
            restart_after: Tick(0),
            arm_after,
            budget_per_run: None,
            kinds: bohr_world::DrawnFaults::default(),
        },
        ..Scenario::default()
    };
    judge_scenario(
        vec![writes_for_ever()],
        &memory(),
        scenario,
        &Tape::from_seed(Seed(3)),
        RunConfig { horizon: Some(Tick(80 * Tick::MILLISECOND.get())), ..RunConfig::default() },
    )
}

/// When the node was killed, or never.
fn killed_at(judged: &Judged) -> Option<Tick> {
    judged
        .events
        .iter()
        .find(|e| matches!(e.kind, EventKind::FaultFired { fault: FaultKind::NodeKill, .. }))
        .map(|e| e.tick)
}

/// Held back, the kill lands after the moment it was held to.
#[test]
fn a_fault_held_back_fires_after_it_is_armed() {
    let at_once = killed_at(&run(Tick(0))).expect("an unheld policy kills early");
    assert!(
        at_once < ARM_AT,
        "the unheld run should die before {ARM_AT:?}, and died at {at_once:?}"
    );

    let held = killed_at(&run(ARM_AT)).expect("a held policy still kills, later");
    assert!(held >= ARM_AT, "a held fault fired at {held:?}, before it was armed");
}
