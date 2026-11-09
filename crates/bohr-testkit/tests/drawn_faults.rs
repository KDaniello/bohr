//! A campaign can search more than one kind of node fault.
//!
//! A fault that exists only where somebody wrote a tick down is a fault a campaign never searches:
//! it appears only where a person already guessed the moment.
//!
//! Two claims:
//!
//! **The kinds a scenario enables are the kinds it gets**, chosen by a draw over the enabled set —
//! so a mutation of the tape moves which kind fires, and a scenario that enabled one never meets
//! another.
//!
//! **Enabling one costs exactly what it cost before.** An index into a set of one is a choice with
//! one option and consumes no tape, so every scenario that draws only kills lays its tape out
//! exactly as it did. That is asserted here rather than left to the determinism suite to notice,
//! because it is the property that decides whether stored bundles survive this change.

use std::collections::BTreeSet;

use bohr_backend_null::{GuestOp, MEMORY_BASE, NullProgram};
use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{Judged, Scenario, judge_scenario};
use bohr_trace::{EventKind, FaultKind};
use bohr_types::{Seed, Tick};
use bohr_world::{DiskConfig, DrawnFaults, FaultPolicy, NetworkConfig, Targeting};

/// `openat`.
const OPENAT: u32 = 257;
/// `write`.
const WRITE: u32 = 1;
/// `fsync`.
const FSYNC: u32 = 74;
/// `nanosleep`.
const SLEEP: u32 = 35;

/// `O_CREAT | O_RDWR`.
const OPEN_FLAGS: u64 = 0o100 | 2;

const PATH_AT: u64 = MEMORY_BASE;
const TIMESPEC_AT: u64 = MEMORY_BASE + 0x100;
const PAYLOAD_AT: u64 = MEMORY_BASE + 0x200;

const DATA_FD: u64 = 3;

/// Writes and syncs for a long time, so there are plenty of moments to draw at.
fn works() -> NullProgram {
    let mut ops =
        vec![GuestOp::Syscall { nr: OPENAT, args: [u64::MAX, PATH_AT, OPEN_FLAGS, 0o644, 0, 0] }];
    for _ in 0..200 {
        ops.push(GuestOp::Syscall { nr: WRITE, args: [DATA_FD, PAYLOAD_AT, 512, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: FSYNC, args: [DATA_FD, 0, 0, 0, 0, 0] });
        ops.push(GuestOp::Syscall { nr: SLEEP, args: [TIMESPEC_AT, 0, 0, 0, 0, 0] });
    }
    NullProgram::single(ops)
}

fn memory_for(nodes: u32) -> Vec<bohr_testkit::Preload> {
    let mut timespec = 0u64.to_le_bytes().to_vec();
    timespec.extend_from_slice(&(Tick::MILLISECOND.get() / 4).to_le_bytes());
    (0..nodes)
        .flat_map(|node| {
            [
                (node, PATH_AT, b"/data\0".to_vec()),
                (node, TIMESPEC_AT, timespec.clone()),
                (node, PAYLOAD_AT, vec![b'x'; 512]),
            ]
        })
        .collect()
}

fn memory() -> Vec<bohr_testkit::Preload> {
    memory_for(1)
}

fn run(kinds: DrawnFaults, seed: u64) -> Judged {
    let scenario = Scenario {
        network: NetworkConfig { nodes: 1, ..NetworkConfig::default() },
        disk: DiskConfig::default(),
        fault_policy: FaultPolicy {
            kill: (1, 32),
            enomem: (0, 0),
            targeting: Targeting::Guided,
            restart_after: Tick(Tick::MILLISECOND.get()),
            arm_after: Tick(0),
            budget_per_run: None,
            kinds,
        },
        ..Scenario::default()
    };
    judge_scenario(
        vec![works()],
        &memory(),
        scenario,
        &Tape::from_seed(Seed(seed)),
        RunConfig { horizon: Some(Tick(120 * Tick::MILLISECOND.get())), ..RunConfig::default() },
    )
}

fn kinds_fired(judged: &Judged) -> BTreeSet<String> {
    judged
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultFired { fault, .. } => Some(format!("{fault:?}")),
            _ => None,
        })
        .collect()
}

/// The same, for a cluster large enough to have a link to make bad.
fn run_two(kinds: DrawnFaults, seed: u64) -> Judged {
    let scenario = Scenario {
        network: NetworkConfig { nodes: 2, ..NetworkConfig::default() },
        disk: DiskConfig::default(),
        fault_policy: FaultPolicy {
            kill: (1, 32),
            enomem: (0, 0),
            targeting: Targeting::Guided,
            restart_after: Tick(Tick::MILLISECOND.get()),
            arm_after: Tick(0),
            budget_per_run: None,
            kinds,
        },
        ..Scenario::default()
    };
    judge_scenario(
        vec![works(), works()],
        &memory_for(2),
        scenario,
        &Tape::from_seed(Seed(seed)),
        RunConfig { horizon: Some(Tick(120 * Tick::MILLISECOND.get())), ..RunConfig::default() },
    )
}

/// The claim: a scenario that enables a lifecycle kind meets it.
#[test]
fn a_campaign_draws_the_kinds_it_was_given() {
    let everything = DrawnFaults {
        kill: true,
        freeze: true,
        isolate: false,
        degrade: false,
        recover_from_snapshot: true,
        clock_step: false,
        clock_drift: false,
    };
    let seen: BTreeSet<String> =
        (1..=8).flat_map(|seed| kinds_fired(&run(everything, seed))).collect();

    for wanted in [FaultKind::NodeKill, FaultKind::NodeFreeze, FaultKind::NodeThaw] {
        let name = format!("{wanted:?}");
        assert!(seen.contains(&name), "{name} never fired; eight seeds saw {seen:?}");
    }
}

/// And a scenario that enables only the kill never meets another kind.
#[test]
fn a_scenario_that_asked_for_one_kind_gets_one_kind() {
    let seen: BTreeSet<String> =
        (1..=8).flat_map(|seed| kinds_fired(&run(DrawnFaults::default(), seed))).collect();

    assert!(seen.contains(&format!("{:?}", FaultKind::NodeKill)), "no kill fired at all");
    for unwanted in [FaultKind::NodeFreeze, FaultKind::NodeThaw] {
        let name = format!("{unwanted:?}");
        assert!(!seen.contains(&name), "{name} fired in a scenario that did not ask for it");
    }
}

/// The clock kinds are drawn like the lifecycle ones, each with its size drawn after it.
#[test]
fn a_campaign_draws_clock_faults() {
    let clocks = only(|kinds| {
        kinds.clock_step = true;
        kinds.clock_drift = true;
    });
    let seen: BTreeSet<String> = (1..=8).flat_map(|seed| kinds_fired(&run(clocks, seed))).collect();
    for wanted in [FaultKind::ClockStep, FaultKind::ClockDrift] {
        let name = format!("{wanted:?}");
        assert!(seen.contains(&name), "{name} never fired; eight seeds saw {seen:?}");
    }
}

/// One kind of link fault and nothing else.
fn only(kind: fn(&mut DrawnFaults)) -> DrawnFaults {
    let mut kinds = DrawnFaults {
        kill: false,
        freeze: false,
        isolate: false,
        degrade: false,
        recover_from_snapshot: false,
        clock_step: false,
        clock_drift: false,
    };
    kind(&mut kinds);
    kinds
}

/// The claim: a drawn isolation cuts the node off, and heals it again.
///
/// Stated over both directions of the pair, because a cut that only stopped what the node sent
/// would be a half-isolation, and a node that can still hear the cluster is not cut off from it.
#[test]
fn a_campaign_draws_the_cut_it_was_given() {
    let isolating = only(|k| k.isolate = true);
    let judged = (1..=8).map(|seed| run_two(isolating, seed)).find(|j| !cuts(j).is_empty());
    let judged = judged.expect("eight seeds and no isolation at all");
    let raised = cuts(&judged);
    let (from, to) = raised[0];

    assert!(
        raised.contains(&(to, from)),
        "{from} was cut from {to} and {to} was left able to reach it: {raised:?}"
    );
    // Each cut is narrated where it is raised and again where it heals, so a link a policy gives
    // a recovery to is named an even number of times. An odd count is a partition left standing.
    let outward = raised.iter().filter(|link| **link == (from, to)).count();
    assert_eq!(
        outward % 2,
        0,
        "the cut {from}->{to} was raised {outward} times and healed one fewer: {raised:?}"
    );
    assert_eq!(
        outward,
        raised.iter().filter(|link| **link == (to, from)).count(),
        "the two directions of one pair were cut a different number of times: {raised:?}"
    );
}

/// And a scenario that did not ask for one never gets one.
#[test]
fn a_scenario_that_asked_for_no_cut_gets_none() {
    for seed in 1..=8 {
        let judged = run_two(DrawnFaults::default(), seed);
        assert!(cuts(&judged).is_empty(), "seed {seed} cut a link nobody asked to cut");
        assert!(degradations(&judged).is_empty(), "seed {seed} spoiled a link nobody asked about");
    }
}

/// The claim: a drawn degradation spoils the node's links without cutting one.
///
/// The distinction is the whole point of the kind: a cut a guest can detect against a link that
/// merely stopped being good, which is what it handles worst.
#[test]
fn a_campaign_draws_the_degradation_it_was_given() {
    let degrading = only(|k| k.degrade = true);
    let judged = (1..=8).map(|seed| run_two(degrading, seed)).find(|j| !degradations(j).is_empty());
    let judged = judged.expect("eight seeds and no degradation at all");

    assert!(cuts(&judged).is_empty(), "a degradation cut a link, which is the other fault");
}

/// The links a run made bad, whether by dropping or by delaying.
fn degradations(judged: &Judged) -> Vec<(u32, u32)> {
    judged
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultFired {
                fault: FaultKind::NetDrop | FaultKind::NetDelay,
                target: bohr_trace::FaultTarget::Link { from, to },
            } => Some((from.0, to.0)),
            _ => None,
        })
        .collect()
}

/// The links a run cut or healed, in the order it narrated them.
fn cuts(judged: &Judged) -> Vec<(u32, u32)> {
    judged
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultFired {
                fault: FaultKind::NetPartition,
                target: bohr_trace::FaultTarget::Link { from, to },
            } => Some((from.0, to.0)),
            _ => None,
        })
        .collect()
}

/// Enabling one kind lays the tape out exactly as it did before there was a choice.
///
/// The property stored bundles depend on: an index into a set of one is not a decision, so the
/// default policy draws what it always drew and a run of it is the run it always was.
#[test]
fn one_kind_costs_no_tape() {
    let only_kills = run(DrawnFaults::default(), 5);
    let drawn: Vec<_> = only_kills
        .report
        .journal
        .records()
        .iter()
        .filter(|r| r.site == bohr_tape::DrawSite::NodeFault as u16)
        .collect();

    assert!(!drawn.is_empty(), "the policy never drew at all");

    // Two records per fire and only one of them costs anything: the odds are a fixed two bytes,
    // and the choice of kind is a choice with one option, which the journal records at nought.
    // A record is written either way — that is what makes the journal a full account of the run —
    // and what a stored bundle depends on is the width, not the count.
    let free = drawn.iter().filter(|r| r.len == 0).count();
    assert!(free > 0, "the kind was never drawn at all");
    for record in &drawn {
        assert!(
            record.len == 0 || record.len == 2,
            "a draw at the fault site cost {} bytes, which is neither the odds nor nothing",
            record.len
        );
    }
}
