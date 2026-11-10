//! The fixture a reduction works on: what it costs, and that it goes wrong for the stated reason.
//!
//! A long and a short run differ only in when the world acted. The run is judged a broken promise,
//! not a hang.

use bohr_kernel::RunConfig;
use bohr_tape::Tape;
use bohr_testkit::{
    Judged, judge_scenario,
    violation::{PROMISE, PingPong},
};
use bohr_trace::{EventKind, Outcome};
use bohr_types::{Seed, Tick};

fn run(fixture: PingPong) -> Judged {
    let tape = Tape::from_seed(Seed(1));
    judge_scenario(
        fixture.programs(),
        &fixture.preloads(),
        fixture.scenario(),
        &tape,
        RunConfig::default(),
    )
}

/// The oracle the tool ships names the promise and where it is written.
///
/// Records say which promise broke and on which line.
#[test]
fn a_broken_promise_is_judged_as_one_and_named() {
    let judged = run(PingPong::new(8).cut_at(Tick(1)).kill_at(Tick(1)));

    let Outcome::PropertyViolated(detail) = &judged.report.outcome else {
        panic!("the run was judged {:?} rather than a broken promise", judged.report.outcome);
    };
    assert!(detail.contains(PROMISE), "the verdict does not name it: {detail}");
    assert!(detail.contains("src/ping.rs:214"), "the verdict does not locate it: {detail}");

    let failed = judged
        .events
        .iter()
        .find(|e| matches!(&e.kind, EventKind::Assertion { condition: false, .. }))
        .expect("the failed assertion is on the timeline");
    assert_eq!(judged.report.violation_event, Some(failed.id));
    assert!(!judged.sdk_records.is_empty(), "the records the verdict was read from were not kept");
}

/// Left alone, the exchange keeps its promise.
#[test]
fn an_uninterrupted_exchange_keeps_its_promise() {
    let judged = run(PingPong::new(8));
    assert_eq!(judged.report.outcome, Outcome::Clean, "got {:?}", judged.report.outcome);
}

/// Run length depends on when the world acted; both runs use the same scripts.
#[test]
fn what_makes_the_run_long_is_the_world_and_not_the_guest() {
    let rounds = 400;
    let late = run(PingPong::new(rounds).cut_at(Tick(600_000_000)).kill_at(Tick(700_000_000)));
    let early = run(PingPong::new(rounds).cut_at(Tick(1)).kill_at(Tick(1)));

    assert!(
        matches!(late.report.outcome, Outcome::PropertyViolated(_)),
        "got {:?}",
        late.report.outcome
    );
    assert!(
        matches!(early.report.outcome, Outcome::PropertyViolated(_)),
        "got {:?}",
        early.report.outcome
    );
    assert!(
        early.report.events * 20 < late.report.events,
        "moving the world's interference to the first tick took {} events to {}, which is not the \
         collapse a reduction depends on",
        late.report.events,
        early.report.events
    );
    eprintln!("late {} events · early {} events", late.report.events, early.report.events);
}

/// The floor a reduction of this fixture stops at, measured.
///
/// Setup, one ping, the broken-promise record and the responder's kill cannot be removed.
#[test]
fn the_floor_a_reduction_stops_at() {
    let both = run(PingPong::new(4000).cut_at(Tick(1)).kill_at(Tick(1)));
    let converged = run(PingPong::new(4000).kill_at(Tick(1)));

    for judged in [&both, &converged] {
        assert!(
            matches!(judged.report.outcome, Outcome::PropertyViolated(_)),
            "the smallest form no longer reports the promise: {:?}",
            judged.report.outcome
        );
    }
    assert!(
        converged.report.events < both.report.events,
        "dropping the partition removed nothing, so a reduction has one fewer move than it looks"
    );
    eprintln!(
        "floor: {} events with the cut, {} without · {} draws",
        both.report.events,
        converged.report.events,
        converged.report.journal.len()
    );
}
