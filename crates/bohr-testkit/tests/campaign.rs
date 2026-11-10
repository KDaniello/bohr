//! A campaign over real runs, and the promise that eight workers find what one finds.
//!
//! `docs/ARCHITECTURE.md` §6.2: a finding from any worker replays under `-j 1`. That follows from
//! planning each generation from the corpus and folding results in planned order.
//!
//! Checked here against the scripted backend in milliseconds. If this passes and `bohr explore -j
//! 8` fails, the defect is in the process plumbing.

use bohr_explorer::{Campaign, RunResult, WorkItem};
use bohr_testkit::{Fixture, carry_out};
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
            // Any unplanned order will do; rotating by generation varies it.
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

/// One master seed and budget give one campaign, in any arrival order.
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

/// A campaign repeats from its master seed.
#[test]
fn one_master_seed_and_one_budget_is_one_campaign() {
    assert_eq!(
        shape(&campaign(8, 12, Arrival::InOrder)),
        shape(&campaign(8, 12, Arrival::InOrder)),
        "two campaigns with one seed reached different places"
    );
}

/// The corpus and the campaign's coverage grow.
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

/// Re-planning after a worker dies gives the same items.
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
