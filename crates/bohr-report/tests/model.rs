//! Reading stored bundles the way a reader will: the ones in the repository, unchanged.
//!
//! Fixtures rather than bundles built in the test, because what is under test is whether a bundle
//! *somebody else wrote* can be understood. A model exercised only against its own idea of a
//! bundle would keep working while every real one stopped opening.

use std::path::PathBuf;

use bohr_report::RunModel;
use bohr_trace::Bundle;

fn stored(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/bundles").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn model(name: &str) -> RunModel {
    let bytes = stored(name);
    let bundle = Bundle::parse(&bytes).expect("a stored bundle parses");
    RunModel::of(&bundle).expect("a stored bundle has a manifest")
}

#[test]
fn a_stored_bundle_becomes_a_model() {
    for name in ["two-tasks-seed0.bohr", "clock-and-random-seed0.bohr", "large-random-seed0.bohr"] {
        let model = model(name);
        assert_eq!(
            model.events.len() as u64,
            model.manifest.counters.events,
            "{name}: the timeline and the manifest disagree about how much happened"
        );
        assert!(model.truncated.is_none(), "{name}: the stored log did not read whole");
        assert!(!model.nodes().is_empty(), "{name}: a run with no nodes recorded events");
    }
}

/// Every draw belongs to an event, which is the join the journal exists for.
#[test]
fn draws_land_on_events_that_exist() {
    let model = model("clock-and-random-seed0.bohr");
    assert!(!model.journal.records().is_empty(), "the fixture drew nothing to join");

    let last = model.events.last().expect("the run recorded something").id;
    for (event, draws) in model.draws_by_event() {
        assert!(
            event.get() <= last.get() + 1,
            "{} draws were attributed to event {}, past the end of a run of {}",
            draws.len(),
            event.get(),
            last.get()
        );
    }
}

/// A guest that kept every promise has no violation to point at, and the model must say so rather
/// than pointing at whatever event happened to be last.
#[test]
fn a_clean_run_has_no_point_of_violation() {
    let model = model("two-tasks-seed0.bohr");
    assert!(model.violation.is_none(), "a clean run named a violation: {:?}", model.violation);
    assert_eq!(model.manifest.outcome, bohr_trace::Outcome::Clean);
}
