//! A run can be told how many faults it gets.
//!
//! Odds set when a fault fires; a budget sets how many. At 1/1000 per call an execution client was
//! killed 270 times in 20 runs, testing only its restart path.
//!
//! A budget caps the fires. A spent budget still draws, with zero odds, so tape consumption is
//! unchanged.

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::{EventKind, FaultKind};
use bohr_types::{Seed, Tick};
use bohr_world::{DrawnFaults, FaultPolicy, NetworkConfig, Targeting};

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

/// What a budget is meant to hold a run to.
const BUDGET: u32 = 2;

/// Writes without syncing for a long time, so a kill can fire at any moment.
fn writes_for_ever() -> NullProgram {
    let mut ops =
        vec![GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, PATH_AT, OPEN_FLAGS, 0o644, 0, 0] }];
    for _ in 0..600 {
        ops.push(GuestOp::Syscall { nr: WRITE, args: [DATA_FD, PAYLOAD_AT, 64, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }
    NullProgram::single(ops)
}

fn memory() -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&(Tick::MILLISECOND.get() / 20).to_le_bytes());
    vec![
        (0, PATH_AT, b"/data\0".to_vec()),
        (0, TIMESPEC_AT, timespec),
        (0, PAYLOAD_AT, vec![b'x'; 64]),
    ]
}

fn run(budget_per_run: Option<u32>, seed: u64) -> Judged {
    let scenario = Scenario {
        network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
        fault_policy: FaultPolicy {
            kill: (1, 16),
            enomem: (0, 0),
            targeting: Targeting::Guided,
            // Short, so the node is back in time to be killed again.
            restart_after: Tick(Tick::MILLISECOND.get() / 4),
            arm_after: Tick(0),
            budget_per_run,
            kinds: DrawnFaults::default(),
        },
        ..Scenario::default()
    };
    judge_scenario(
        vec![writes_for_ever()],
        &memory(),
        scenario,
        &Tape::from_seed(Seed(seed)),
        RunConfig { horizon: Some(Tick(60 * Tick::MILLISECOND.get())), ..RunConfig::default() },
    )
}

/// How many times the node was killed.
fn kills(judged: &Judged) -> usize {
    judged
        .events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::FaultFired { fault: FaultKind::NodeKill, .. }))
        .count()
}

/// A budgeted run fires its budget and no more; an unbudgeted one fires far more.
#[test]
fn a_run_fires_what_it_was_budgeted_and_no_more() {
    let unbounded = kills(&run(None, 7));
    assert!(
        unbounded > BUDGET as usize,
        "an unbudgeted run died {unbounded} times, which is no more than the budget it is \
         supposed to differ from"
    );

    for seed in 1..=8 {
        let budgeted = kills(&run(Some(BUDGET), seed));
        assert!(budgeted <= BUDGET as usize, "seed {seed} spent {budgeted} of {BUDGET} fires");
    }
}

/// A spent budget goes on drawing, and moves nothing decided before it ran out.
///
/// A spent budget keeps drawing two-byte odds, and up to the last allowed fire the budgeted and
/// unbounded runs are identical.
#[test]
fn a_budget_costs_no_tape() {
    let bounded = run(Some(BUDGET), 7);
    let unbounded = run(None, 7);

    let widths = |judged: &Judged| -> Vec<u32> {
        judged
            .report
            .journal
            .records()
            .iter()
            .filter(|r| r.site == bohr_tape::DrawSite::NodeFault as u16)
            .map(|r| r.len)
            .collect()
    };

    let (bounded, unbounded) = (widths(&bounded), widths(&unbounded));
    assert!(!bounded.is_empty(), "the budgeted run never drew at the fault site");

    // Two-byte odds and a zero-width kind; a spent budget adds no other width.
    for width in &bounded {
        assert!(*width == 0 || *width == 2, "a spent budget drew {width} bytes at the fault site");
    }

    // Everything up to and including the last fire the ceiling allowed.
    let last_fire = bounded.iter().rposition(|width| *width == 0).expect("no fire was drawn");
    assert_eq!(
        bounded[..=last_fire],
        unbounded[..=last_fire],
        "a budget moved a draw that came before it was spent"
    );
}
