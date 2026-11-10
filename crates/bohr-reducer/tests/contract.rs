//! The reducer's entire contract, proven against a pure function.
//!
//! Two properties: the output reproduces the input, and is never larger.
//!
//! The runner is a pure function of the scenario, so these run in milliseconds anywhere.

use bohr_kernel::StopReason;
use bohr_reducer::{
    Baseline, CandidateOutcome, ConfigDoc, Keep, Limits, Reduction, Scenario, Size, Violation,
};
use bohr_tape::Journal;
use bohr_trace::{AssertionKind, Event, EventKind, Outcome};
use bohr_types::{EventId, NodeId, Seed, TaskId, Tick};

/// A configuration with three things scheduled, only one of which matters.
const SCENARIO: &str = "\
[network]
drop_probability = \"1/1000\"

[[partition]]
at_tick = 8000
heal_at_tick = 18446744073709551615
cut = { from = [0], to = [1] }

[[grey]]
link = { from = 0, to = 1 }
at_tick = 4000
until_tick = 6000

[[fault]]
kind = \"node_kill\"
node = 1
at_tick = 9000
";

fn broke(name: &str) -> Vec<Event> {
    let at = |id: u64, kind| Event {
        id: EventId(id),
        tick: Tick(id),
        node: NodeId(0),
        task: TaskId(0),
        kind,
    };
    vec![
        at(
            1,
            EventKind::Assertion {
                name: name.to_owned(),
                kind: AssertionKind::Always,
                hit: true,
                condition: false,
            },
        ),
        at(
            2,
            EventKind::Diagnostic {
                code: "BOHR-2012".to_owned(),
                message: format!("always(\"{name}\") failed at src/ping.rs:214"),
            },
        ),
    ]
}

fn promise_broken(name: &str) -> Violation {
    Violation::of(
        &Outcome::PropertyViolated("named elsewhere".to_owned()),
        StopReason::GuestFinished,
        &broke(name),
    )
    .expect("a finding")
}

/// When the kill is scheduled, if it is scheduled at all.
///
/// Read from the text, since the document does not tie moments to entries.
fn kill_at(config: &str) -> Option<u64> {
    let mut inside_fault = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside_fault = trimmed == "[[fault]]";
        }
        if inside_fault && let Some(value) = trimmed.strip_prefix("at_tick") {
            return value.trim_start_matches([' ', '=']).replace('_', "").trim().parse().ok();
        }
    }
    None
}

/// The guest waits for the responder's kill and then fails; the rest of the configuration is
/// noise.
fn answer(scenario: &Scenario) -> (Option<Violation>, Size) {
    let Some(fires_at) = kill_at(scenario.config()) else {
        return (None, Size { events: 4, draws: 2 });
    };
    let events = 14 + fires_at / 2;
    (Some(promise_broken("every ping is answered")), Size { events, draws: events })
}

/// Runs a reduction to convergence against `answer`, and returns it with how many replays it spent.
fn reduce(limits: Limits) -> (Reduction, u32) {
    let input = Scenario::new(Seed(1), vec![0xAA; 64], SCENARIO.to_owned());
    let (violation, size) = answer(&input);
    let baseline = Baseline {
        violation: violation.expect("the input reproduces"),
        size,
        journal: Journal::new(),
    };
    let mut reduction = Reduction::new(input, baseline, limits).expect("a workable budget");

    let mut probes = 0;
    while let Some(candidate) = reduction.next() {
        let (violation, size) = answer(&candidate.scenario);
        let outcome =
            CandidateOutcome { id: candidate.id, violation, size, journal: Journal::new() };
        probes += 1;
        assert!(probes < limits.replays * 4, "a reduction did not converge");
        reduction.accept(&outcome);
    }
    (reduction, probes)
}

/// The first property: what comes out still goes wrong the same way.
#[test]
fn a_reduction_still_reproduces_what_it_started_from() {
    let (reduction, _) = reduce(Limits::default());
    let (violation, _) = answer(reduction.best());
    let asked = promise_broken("every ping is answered");
    assert!(
        violation.is_some_and(|found| asked.reproduced_by(&found)),
        "the result no longer reproduces what it was asked to keep"
    );
}

/// The second property: what comes out is never larger than what went in.
#[test]
fn a_reduction_is_monotonic() {
    let input = Scenario::new(Seed(1), vec![0xAA; 64], SCENARIO.to_owned());
    let (_, before) = answer(&input);
    let (reduction, _) = reduce(Limits::default());
    assert!(reduction.size() <= before, "{:?} is not smaller than {before:?}", reduction.size());
}

/// The reduction removes the noise and moves the kill earlier.
#[test]
fn a_reduction_removes_what_does_not_matter_and_brings_forward_what_does() {
    let (reduction, probes) = reduce(Limits::default());
    let doc = ConfigDoc::parse(reduction.best().config());

    let tables: Vec<&str> = doc.entries().iter().map(|e| e.table.as_str()).collect();
    assert_eq!(tables, vec!["fault"], "the scenery was kept or the cause was removed");

    let fires_at = doc.moments()[0].ticks;
    assert!(fires_at < 100, "the fault still fires at tick {fires_at}");

    let (_, size) = answer(reduction.best());
    eprintln!("{:?} -> {size:?} in {probes} probes", Size { events: 4514, draws: 4514 });
    assert!(size.events < 30, "{} events is not a reduction", size.events);
}

/// The budget is never exceeded.
#[test]
fn the_budget_is_never_exceeded() {
    for replays in [4u32, 8, 25, 200] {
        let (reduction, probes) = reduce(Limits { replays, keep: Keep::Both });
        assert!(reduction.spent().total() <= replays, "spent {:?} of {replays}", reduction.spent());
        assert!(probes <= replays, "{probes} probes against a budget of {replays}");
    }
}

/// Whatever the budget, the answer is one that was proven — never one that ran out mid-probe.
#[test]
fn a_reduction_cut_short_still_answers_with_something_proven() {
    let (reduction, _) = reduce(Limits { replays: 6, keep: Keep::Both });
    let (violation, size) = answer(reduction.best());
    let asked = promise_broken("every ping is answered");
    assert!(violation.is_some_and(|found| asked.reproduced_by(&found)));
    assert!(size <= Size { events: 4514, draws: 4514 });
}

/// Asking twice without answering proposes the same candidate.
#[test]
fn proposing_twice_without_accepting_proposes_the_same_candidate() {
    let input = Scenario::new(Seed(1), vec![0xAA; 64], SCENARIO.to_owned());
    let (violation, size) = answer(&input);
    let baseline =
        Baseline { violation: violation.expect("reproduces"), size, journal: Journal::new() };
    let reduction = Reduction::new(input, baseline, Limits::default()).expect("a budget");
    assert_eq!(reduction.next(), reduction.next());
}

/// A candidate the world refuses to load counts as not reproducing.
///
/// A liveness scenario refuses a subset with no `[[partition]]`, and the reduction carries on.
#[test]
fn a_candidate_the_world_would_refuse_does_not_end_the_reduction() {
    let input = Scenario::new(Seed(1), vec![0xAA; 64], SCENARIO.to_owned());
    let (violation, size) = answer(&input);
    let baseline = Baseline {
        violation: violation.expect("the input reproduces"),
        size,
        journal: Journal::new(),
    };
    let limits = Limits { replays: 200, keep: Keep::Both };
    let mut reduction = Reduction::new(input, baseline, limits).expect("a workable budget");

    let mut refused = 0;
    while let Some(candidate) = reduction.next() {
        let id = candidate.id;
        // Every candidate that dropped the partition is one this world will not load.
        let unloadable = !candidate.scenario.config().contains("[[partition]]");
        let (violation, size) = if unloadable {
            refused += 1;
            (None, Size { events: 0, draws: 0 })
        } else {
            answer(&candidate.scenario)
        };
        reduction.accept(&CandidateOutcome { id, violation, size, journal: Journal::new() });
    }

    assert!(refused > 0, "no candidate dropped the partition, so nothing was under test");
    // The reduction finished and its result reproduces.
    assert_eq!(reduction.pass(), bohr_reducer::Pass::Done);
    let (violation, _) = answer(reduction.best());
    assert!(
        violation
            .is_some_and(|found| promise_broken("every ping is answered").reproduced_by(&found)),
        "the scenario that came out no longer reproduces what went in"
    );
}

/// A node that draws whether it dies at every call, and a defect reached only by a death after
/// the tenth call: the run then lasts fifty calls more and breaks its promise.
///
/// Read as the world reads it, two odds bytes per call and a kind byte after a fired draw, and
/// journalled the same way.
fn drawn_kills(scenario: &Scenario) -> (Option<Violation>, Size, Journal, Vec<(u64, u8)>) {
    const CALLS: u64 = 1_000;
    const FIRES_BELOW: u64 = 33;
    let prefix = scenario.prefix();
    let mut journal = Journal::new();
    let mut kills = Vec::new();
    let mut at = 0usize;
    for call in 0..CALLS {
        let odds = u64::from(u16::from_le_bytes([prefix[at], prefix[at + 1]]));
        let fired = odds < FIRES_BELOW;
        journal.push_odds(
            bohr_types::TapeOffset(at as u64),
            EventId(call),
            bohr_tape::DrawSite::NodeFault,
            odds,
            fired,
        );
        at += 2;
        if fired {
            let kind = prefix[at];
            journal.push(
                bohr_types::TapeOffset(at as u64),
                EventId(call),
                bohr_tape::DrawSite::NodeFault,
                1,
                u64::from(kind),
            );
            at += 1;
            kills.push((call, kind));
        }
        if kills.iter().any(|(when, _)| *when >= 10) {
            let events = call + 50;
            let size = Size { events, draws: journal.len() as u64 };
            return (Some(promise_broken("every ping is answered")), size, journal, kills);
        }
    }
    (None, Size { events: CALLS, draws: journal.len() as u64 }, journal, kills)
}

/// A prefix that kills at the calls given, as kind 7, and nowhere else.
fn killing_at(calls: &[u64]) -> Vec<u8> {
    let mut prefix = Vec::new();
    for call in 0..1_000u64 {
        if calls.contains(&call) {
            prefix.extend_from_slice(&[0, 0, 7]);
        } else {
            prefix.extend_from_slice(&[0xAA, 0xAA]);
        }
    }
    prefix
}

/// The fault pass removes the unneeded kill and fires the needed one earlier, as the same kind and
/// after the tenth call. The tape pass is off, so the kind is unchanged.
#[test]
fn a_drawn_kill_is_fired_sooner_and_one_not_needed_is_removed() {
    let input = Scenario::new(Seed(1), killing_at(&[300, 700]), String::new());
    let (violation, size, journal, _) = drawn_kills(&input);
    assert_eq!(size.events, 350);
    let baseline = Baseline { violation: violation.expect("reproduces"), size, journal };
    let limits = Limits { replays: 200, keep: Keep::Events };
    let mut reduction = Reduction::new(input, baseline, limits).expect("a budget");
    while let Some(candidate) = reduction.next() {
        let id = candidate.id;
        let (violation, size, journal, _) = drawn_kills(&candidate.scenario);
        reduction.accept(&CandidateOutcome { id, violation, size, journal });
    }

    let (violation, size, _, kills) = drawn_kills(reduction.best());
    assert!(violation.is_some(), "the result no longer reproduces");
    assert!(size.events < 100, "{} events is not a reduction of 350", size.events);
    assert_eq!(kills.len(), 1, "a kill the finding does not need was kept: {kills:?}");
    let (when, kind) = kills[0];
    assert!(when >= 10, "the kill moved to where the defect cannot be reached");
    assert_eq!(kind, 7, "the kill moved and fired as another kind");
    assert!(reduction.spent().faults > 0);
    assert!(reduction.spent().total() <= 200);
}

/// A candidate already answered is never proposed again.
#[test]
fn no_scenario_is_proposed_twice() {
    let input = Scenario::new(Seed(1), killing_at(&[5, 300, 700]), SCENARIO.to_owned());
    let (violation, size, journal, _) = drawn_kills(&input);
    let baseline = Baseline { violation: violation.expect("reproduces"), size, journal };
    let mut reduction =
        Reduction::new(input.clone(), baseline, Limits::default()).expect("a budget");
    let mut seen = vec![input];
    while let Some(candidate) = reduction.next() {
        assert!(!seen.contains(&candidate.scenario), "a scenario was proposed twice");
        seen.push(candidate.scenario.clone());
        let id = candidate.id;
        let (violation, size, journal, _) = drawn_kills(&candidate.scenario);
        reduction.accept(&CandidateOutcome { id, violation, size, journal });
    }
}
