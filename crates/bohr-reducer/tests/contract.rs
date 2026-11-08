//! The reducer's entire contract, proven against a function rather than against a cluster.
//!
//! Two properties and nothing else: what comes out still reproduces what went in, and what comes
//! out is never larger. A reducer that produced a smaller scenario which no longer reproduces is
//! worse than no reducer, because it costs a maintainer an afternoon before they find out.
//!
//! The runner here is a pure function of a scenario. That is the whole point of the crate being
//! sans-io: the properties are decided by the reduction and not by the guest, so they can be
//! checked in milliseconds, on any machine, with no backend built.

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
/// Read out of the text rather than through the document, because what the test needs is the
/// moment belonging to *one* entry and the document deliberately does not model that — a reduction
/// brings every moment forward independently and has no reason to know which belongs to which.
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

/// A world where the guest waits until the responder is killed and then says its promise broke.
/// The run is as long as that wait; everything else in the configuration is scenery.
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

/// And it has to actually work: a reduction that returned its input unchanged would satisfy both
/// properties above and be worthless.
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

/// The budget is a hard stop, not a target: a reduction that overran would make "under two
/// thousand replays" a description rather than a promise.
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

/// Asking twice without answering asks the same thing, which is what a run that was lost is
/// reissued from.
#[test]
fn proposing_twice_without_accepting_proposes_the_same_candidate() {
    let input = Scenario::new(Seed(1), vec![0xAA; 64], SCENARIO.to_owned());
    let (violation, size) = answer(&input);
    let baseline =
        Baseline { violation: violation.expect("reproduces"), size, journal: Journal::new() };
    let reduction = Reduction::new(input, baseline, Limits::default()).expect("a budget");
    assert_eq!(reduction.next(), reduction.next());
}

/// A candidate the world would refuse is a candidate that did not reproduce, and nothing more.
///
/// **This is the shape that killed every liveness reduction.** Minimisation over the entries will
/// propose a subset with no `[[partition]]` left, and a scenario watching for a heal refuses one —
/// so the load failed, the failure was raised out of the whole command, and a maintainer was told
/// their own file was a usage error with the budget already spent. The reduction has to be able to
/// hear "no" from a candidate that never ran, and go on.
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
    // The reduction reached its own end rather than being carried out of, and what it produced
    // still reproduces what went in.
    assert_eq!(reduction.pass(), bohr_reducer::Pass::Done);
    let (violation, _) = answer(reduction.best());
    assert!(
        violation
            .is_some_and(|found| promise_broken("every ping is answered").reproduced_by(&found)),
        "the scenario that came out no longer reproduces what went in"
    );
}
