//! Reduction against a real run: forty thousand events, and what is left of them.
//!
//! Process-free and on any machine — the guests are scripted and the backend executes them in
//! memory — so the number this produces is a fact about the reduction rather than about the host
//! it was measured on.

use bohr_kernel::RunConfig;
use bohr_reducer::{
    Baseline, CandidateOutcome, ConfigDoc, Limits, Reduction, Scenario, Size, Violation,
};
use bohr_tape::Tape;
use bohr_testkit::{Judged, judge_scenario, violation::PingPong};
use bohr_types::Seed;
use bohr_world::config::load;

/// Rounds that record about forty thousand events while every one of them is answered.
const ROUNDS: usize = 4600;

/// Late enough that almost every round is answered, so what makes the run long is the waiting.
///
/// The heal is written as a tick the run will never reach rather than as the largest one there is:
/// a configuration file is TOML, whose integers stop at what fits in sixty-three bits.
const CUT_AT: u64 = 9_000_000_000;
const KILL_AT: u64 = 9_100_000_000;

/// The scenario as the configuration text a bundle would carry.
///
/// Written here rather than derived from the fixture because reduction works on the file: what it
/// removes is an entry somebody wrote, and the result has to be a file that same person could read.
fn config_text() -> String {
    format!(
        "[cluster]\n\
         nodes = 2\n\
         argv = [\"scripted\"]\n\
         \n\
         [network]\n\
         latency_mean_ticks = 100000\n\
         \n\
         [[grey]]\n\
         link = {{ from = 0, to = 1 }}\n\
         at_tick = 1000000\n\
         until_tick = 2000000\n\
         \n\
         [[partition]]\n\
         at_tick = {CUT_AT}\n\
         heal_at_tick = 9000000000000000\n\
         cut = {{ from = [0], to = [1] }}\n\
         \n\
         [[fault]]\n\
         kind = \"node_kill\"\n\
         node = 1\n\
         at_tick = {KILL_AT}\n"
    )
}

/// Carries out one scenario, in the form a reduction is entitled to know about it.
fn carry_out(scenario: &Scenario) -> (Option<Violation>, Size, bohr_tape::Journal, Judged) {
    let loaded = load(scenario.config()).expect("a configuration this test wrote");
    let fixture = PingPong::new(ROUNDS);
    let tape = Tape::with_prefix(scenario.seed(), scenario.prefix().to_vec());

    let judged = judge_scenario(
        fixture.programs(),
        &fixture.preloads(),
        bohr_testkit::Scenario {
            network: loaded.network,
            disk: loaded.disk,
            node_faults: loaded.node_faults,
            fault_policy: loaded.fault_policy,
            sdk_output: Some("/bohr/records.jsonl".to_owned()),
            liveness: loaded.liveness,
            durability: loaded.durability,
        },
        &tape,
        RunConfig::default(),
    );

    let violation =
        Violation::of(&judged.report.outcome, judged.report.stop_reason, &judged.events);
    let size = Size { events: judged.report.events, draws: judged.report.journal.len() as u64 };
    let journal = judged.report.journal.clone();
    (violation, size, journal, judged)
}

/// The scenario a reduction starts from: long, and going wrong for a reason that is in the file.
fn baseline() -> (Scenario, Baseline, Judged) {
    // Long enough to cover every draw the run makes, so a rewritten decision is one a mutation can
    // reach. The explorer's bound on a corpus entry does not apply here and must not be borrowed:
    // a run of this length draws well past it.
    let scenario = Scenario::new(Seed(1), vec![0u8; 128 * 1024], config_text());
    let (violation, size, journal, judged) = carry_out(&scenario);
    let baseline = Baseline {
        violation: violation.expect("the scenario to reduce reproduces something"),
        size,
        journal,
    };
    (scenario, baseline, judged)
}

/// The measurement: how far a reduction gets, and what it spends getting there.
#[test]
fn a_forty_thousand_event_scenario_reduces_to_a_readable_one() {
    let (scenario, before, judged) = baseline();
    eprintln!("baseline {} events · {:?}", before.size.events, judged.report.outcome);
    assert!(
        before.size.events > 30_000,
        "the scenario to reduce records {} events",
        before.size.events
    );

    let limits = Limits::default();
    let mut reduction =
        Reduction::new(scenario, before.clone(), limits).expect("a workable budget");
    let mut probes = 0u32;
    while let Some(candidate) = reduction.next() {
        let (violation, size, journal, _) = carry_out(&candidate.scenario);
        let outcome = CandidateOutcome { id: candidate.id, violation, size, journal };
        probes += 1;
        reduction.accept(&outcome);
    }

    let after = reduction.size();
    eprintln!(
        "events: {} -> {}, tape: {} -> {} draws · {probes} replays · {:?}",
        before.size.events,
        after.events,
        before.size.draws,
        after.draws,
        reduction.spent()
    );

    assert!(probes <= limits.replays, "{probes} replays against a budget of {}", limits.replays);
    assert!(after <= before.size, "a reduction that grew: {after:?} from {:?}", before.size);
    assert!(
        after.events < 20,
        "the reduced scenario records {} events, which is not a scenario anybody reads",
        after.events
    );
}

/// The other half of the contract, and the one a maintainer's afternoon depends on: the result
/// still goes wrong the same way, every time it is run.
#[test]
fn the_reduced_scenario_still_reproduces_what_it_started_from() {
    let (scenario, before, _) = baseline();
    let mut reduction =
        Reduction::new(scenario, before.clone(), Limits::default()).expect("a budget");
    while let Some(candidate) = reduction.next() {
        let (violation, size, journal, _) = carry_out(&candidate.scenario);
        reduction.accept(&CandidateOutcome { id: candidate.id, violation, size, journal });
    }

    for attempt in 0..100 {
        let (violation, _, _, _) = carry_out(reduction.best());
        let found = violation.unwrap_or_else(|| panic!("attempt {attempt} reproduced nothing"));
        assert!(
            before.violation.reproduced_by(&found),
            "attempt {attempt} went wrong in another way"
        );
    }
}

/// What the reduction actually removed, which is what a reader of the result sees. A scenario that
/// still listed everything it started with would be smaller only in a number.
#[test]
fn the_result_names_only_what_mattered() {
    let (scenario, before, _) = baseline();
    let mut reduction = Reduction::new(scenario, before, Limits::default()).expect("a budget");
    while let Some(candidate) = reduction.next() {
        let (violation, size, journal, _) = carry_out(&candidate.scenario);
        reduction.accept(&CandidateOutcome { id: candidate.id, violation, size, journal });
    }

    let doc = ConfigDoc::parse(reduction.best().config());
    let kept: Vec<&str> = doc.entries().iter().map(|entry| entry.table.as_str()).collect();
    eprintln!("kept: {kept:?} · moments: {:?}", doc.moments());
    assert!(kept.len() <= 2, "the result still lists {kept:?}");
    // Only the moments something happens at. A heal is written at a tick the run never reaches,
    // and bringing that forward would undo the partition rather than shrink the scenario — so a
    // reduction leaving it alone is a reduction doing the right thing.
    let fires_at: Vec<u64> = doc
        .moments()
        .iter()
        .filter(|moment| moment.key == "at_tick")
        .map(|moment| moment.ticks)
        .collect();
    assert!(
        fires_at.iter().all(|ticks| *ticks < KILL_AT / 1000),
        "nothing was brought forward: {fires_at:?}"
    );
}
