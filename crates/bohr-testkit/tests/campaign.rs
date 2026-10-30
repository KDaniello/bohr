//! A campaign over real runs, and the promise that eight workers find what one finds.
//!
//! The claim `docs/ARCHITECTURE.md` §6.2 makes — a finding produced by worker seven of sixteen
//! replays identically under `-j 1` — is not a property of the process machinery. It is a property
//! of the campaign, and it is decided long before any process exists: by planning a generation
//! from the corpus alone, and by folding its results in the order they were planned rather than
//! the order they came back.
//!
//! So it is proven here, against the scripted backend, on every machine and in milliseconds. What
//! `bohr explore -j 8` adds on top is processes; if this test is green and that one is not, the
//! defect is in the plumbing rather than in the search.

use bohr_explorer::{Campaign, RunResult, WorkItem};
use bohr_testkit::{carry_out, Fixture};
use bohr_types::Seed;

/// How a generation's results come back when the workers are of different speeds.
#[derive(Clone, Copy)]
enum Arrival {
    /// One worker: everything in the order it was planned.
    InOrder,
    /// Eight workers of unequal speed: whatever finished, whenever.
    Jumbled,
}

/// Runs a campaign to a fixed budget and returns it.
fn campaign(generations: usize, size: usize, arrival: Arrival) -> Campaign {
    let mut campaign = Campaign::new(Seed(1234));
    for g in 0..generations {
        let planned: Vec<WorkItem> = campaign.plan(size);
        let mut results: Vec<RunResult> =
            planned.iter().map(|item| carry_out(Fixture::ManyFlips, item)).collect();

        if matches!(arrival, Arrival::Jumbled) {
            // Not a model of a scheduler, and it does not need to be: what is under test is that
            // the fold does not care what order it is handed, so any order that is not the
            // planned one will do. Rotating by the generation makes it a different wrong order
            // every time rather than one wrong order repeated.
            results.reverse();
            results.rotate_left(g % size.max(1));
        }

        campaign.fold(&planned, results);
    }
    campaign
}

/// What a campaign is, reduced to what two campaigns must agree on.
fn shape(c: &Campaign) -> (u64, u64, Vec<Vec<u8>>, Vec<String>, String) {
    (
        c.runs(),
        c.generation(),
        c.corpus().entries().iter().map(|e| e.prefix.clone()).collect(),
        c.findings().values().map(|f| format!("{}:{}", f.digest, f.kind)).collect(),
        format!("{}", c.corpus().seen().digest()),
    )
}

/// The gate. Same master seed, same budget, results arriving in any order — same campaign.
#[test]
fn a_campaign_does_not_depend_on_which_worker_was_quicker() {
    let ordered = campaign(10, 16, Arrival::InOrder);
    let jumbled = campaign(10, 16, Arrival::Jumbled);

    let (runs, generations, corpus, findings, covered) = shape(&ordered);
    let (runs_j, generations_j, corpus_j, findings_j, covered_j) = shape(&jumbled);

    assert_eq!(runs, runs_j);
    assert_eq!(generations, generations_j);
    assert_eq!(findings, findings_j, "the set of findings depended on arrival order");
    assert_eq!(corpus, corpus_j, "the corpus depended on arrival order");
    assert_eq!(covered, covered_j, "what was covered depended on arrival order");
}

/// The other half: a campaign is a function of its master seed, so it can be repeated at all.
#[test]
fn one_master_seed_and_one_budget_is_one_campaign() {
    assert_eq!(
        shape(&campaign(8, 12, Arrival::InOrder)),
        shape(&campaign(8, 12, Arrival::InOrder)),
        "two campaigns with one seed reached different places"
    );
}

/// A search that does not accumulate is a random walk. The corpus has to grow, and what the
/// campaign has covered has to grow with it.
#[test]
fn a_campaign_reaches_more_than_its_first_generation_did() {
    let short = campaign(1, 16, Arrival::InOrder);
    let long = campaign(12, 16, Arrival::InOrder);

    assert!(
        long.corpus().seen().edges() >= short.corpus().seen().edges(),
        "twelve generations covered less than one"
    );
    assert!(long.runs() > short.runs());
    assert!(!long.corpus().is_empty(), "a campaign that kept nothing has nothing to mutate");
}

/// Work is planned from the corpus, so re-planning after a worker dies gives back the same items
/// rather than a generation the campaign has half-folded.
#[test]
fn work_can_be_reissued_after_a_worker_dies() {
    let campaign = campaign(5, 8, Arrival::InOrder);
    let first = campaign.plan(8);
    let again = campaign.plan(8);

    let shape = |items: &[WorkItem]| {
        items.iter().map(|i| (i.index, i.seed.get(), i.prefix.clone())).collect::<Vec<_>>()
    };
    assert_eq!(shape(&first), shape(&again), "a reissued generation was not the same work");
}
