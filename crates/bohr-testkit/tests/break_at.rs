//! Stopping a run at a named event, which is what handing a frozen world to a debugger rests on.
//!
//! Against the scripted backend, so it runs everywhere. What cannot be checked here is the other
//! half — a real process, actually detached, actually attachable — because that needs a terminal
//! and a debugger on the machine. `docs/TESTING.md` says so rather than pretending otherwise.

use bohr_kernel::{RunConfig, StopReason};
use bohr_tape::Tape;
use bohr_testkit::{judge_scenario, violation::PingPong};
use bohr_trace::Outcome;
use bohr_types::{EventId, Seed};

fn run_to(break_at: Option<EventId>) -> bohr_kernel::RunReport {
    let fixture = PingPong::new(64);
    let tape = Tape::from_seed(Seed(4));
    judge_scenario(
        fixture.programs(),
        &fixture.preloads(),
        fixture.scenario(),
        &tape,
        RunConfig { break_at, ..RunConfig::default() },
    )
    .report
}

/// The run stops where it was asked to, and everything before that happened exactly as it would
/// have — the point of a break is to look at a run that was going to happen anyway.
#[test]
fn a_run_stops_at_the_event_it_was_asked_to() {
    let whole = run_to(None);
    assert!(whole.events > 40, "the fixture is too short to break inside");

    let broken = run_to(Some(EventId(20)));
    assert_eq!(broken.stop_reason, StopReason::BreakRequested);
    assert!(broken.events >= 20, "stopped at {} before the event asked for", broken.events);
    assert!(broken.events < whole.events, "the break did not stop anything");
}

/// Overshooting is possible and must be small: the loop breaks between stops, and one stop can
/// record several events. What it must never be is silent, which is the caller's half.
#[test]
fn a_break_lands_at_or_after_the_event_and_close_to_it() {
    for at in [1u64, 5, 17, 33] {
        let broken = run_to(Some(EventId(at)));
        assert!(broken.events >= at, "stopped at {} before {at}", broken.events);
        assert!(
            broken.events - at < 8,
            "asked for {at} and stopped at {}, which is not the neighbourhood of the question",
            broken.events
        );
    }
}

/// A run somebody stopped is not a run that went wrong. Reporting it as a hang would put a finding
/// in the log of a command that was run to look at something.
#[test]
fn a_run_that_was_asked_to_stop_is_not_reported_as_a_hang() {
    let broken = run_to(Some(EventId(20)));
    let Outcome::StoppedOnRequest(detail) = &broken.outcome else {
        panic!("a break was judged {:?}", broken.outcome);
    };
    assert!(detail.contains("stopped at event"), "{detail}");
}

/// Breaking before the first event stops before anything is scheduled, which is what "before the
/// first instruction" means from the world's side.
#[test]
fn breaking_at_the_first_event_stops_before_anything_runs() {
    let broken = run_to(Some(EventId(0)));
    assert_eq!(broken.stop_reason, StopReason::BreakRequested);
    assert_eq!(broken.events, 0, "{} events happened before the break", broken.events);
}

/// A break past the end of the run never fires, and the run ends as it would have.
#[test]
fn a_break_past_the_end_of_a_run_changes_nothing() {
    let whole = run_to(None);
    let asked = run_to(Some(EventId(whole.events + 1000)));
    assert_eq!(asked.stop_reason, whole.stop_reason);
    assert_eq!(asked.digest, whole.digest, "asking for an unreachable break changed the run");
}
