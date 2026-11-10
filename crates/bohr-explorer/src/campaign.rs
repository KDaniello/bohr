//! The campaign: what to run next, and what the answers meant.
//!
//! # Generations
//!
//! A campaign runs in generations. Generation *N* is planned from the corpus at the end of *N−1*
//! and the master seed only. Workers take its items in any order, and results are folded in index
//! order, so the campaign is a function of its master seed and budget, never of worker count or
//! timing.
//!
//! A generation ends with its slowest item, so it is several times larger than the worker count;
//! see `docs/ARCHITECTURE.md` §6.2.

use bohr_tape::Journal;
use bohr_types::{CoverageMap, Digest, Seed};

use crate::{
    choices::Choices,
    corpus::{Corpus, Offer, Origin},
    mutate::{Context, Operator, mutate},
};

/// One run the campaign wants carried out.
#[derive(Clone, Debug)]
pub struct WorkItem {
    /// Position in this generation; results are folded in this order.
    pub index: u32,
    /// The tape to run: this seed extended by this prefix.
    pub seed: Seed,
    /// The bytes the run reads before the keystream.
    pub prefix: Vec<u8>,
    /// Where it came from, for the corpus entry it may become.
    pub origin: Origin,
}

/// What a run amounted to, as far as the search is concerned.
///
/// A subset of the run report, so the search does not depend on the kernel's output shape.
#[derive(Clone, Debug)]
pub struct RunResult {
    /// The item this answers.
    pub index: u32,
    /// Which run it was. Two runs with one digest are one finding reported twice.
    pub digest: Digest,
    /// Whether the run found something, and what kind of something.
    pub outcome: Option<String>,
    /// What the guest executed.
    pub coverage: CoverageMap,
    /// How many edges the guest was instrumented with, as the guest itself said.
    ///
    /// A map with fewer counters than this cannot tell every path apart.
    pub guest_edges: u64,
    /// What it drew, and where — what a journal-guided mutation aims with.
    pub journal: Journal,
    /// How many faults landed on the guest in this run.
    ///
    /// Summed and reported, so a campaign that injected nothing is visible.
    pub adversity: u32,
    /// Campaign-scoped properties the guest evaluated, as `(name, evaluations, held)`: how often
    /// each was evaluated in this run and how often it held.
    ///
    /// Only campaign-scoped kinds such as `sometimes`; an `always` is already in `outcome`.
    pub observed: Vec<(String, u64, u64)>,
}

/// A finding, once, however many runs reached it.
#[derive(Clone, Debug)]
pub struct Finding {
    /// The run that first reached it.
    pub digest: Digest,
    /// What the oracle called it.
    pub kind: String,
    /// The tape that produces it.
    pub seed: Seed,
    /// The prefix that produces it.
    pub prefix: Vec<u8>,
    /// The generation it was first seen in.
    pub generation: u64,
}

/// What one generation changed.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Progress {
    /// Runs folded in.
    pub runs: u64,
    /// Items that reached somewhere new and were kept.
    pub kept: u64,
    /// Findings nobody had reached before.
    pub fresh_findings: u64,
}

/// The planned work, by the index its result will name.
///
/// Built once per generation, so folding is not quadratic in its size.
fn index(planned: &[WorkItem]) -> std::collections::BTreeMap<u32, &WorkItem> {
    planned.iter().map(|item| (item.index, item)).collect()
}

/// What a run offers the corpus: the tape it was given and what came back from it.
fn offer_of<'a>(item: &'a WorkItem, result: &'a RunResult) -> Offer<'a> {
    Offer {
        seed: item.seed,
        prefix: &item.prefix,
        coverage: &result.coverage,
        draws: &result.journal,
        origin: item.origin,
    }
}

/// The state a campaign carries between generations.
#[derive(Clone, Debug)]
pub struct Campaign {
    pub(crate) corpus: Corpus,
    pub(crate) master: Seed,
    pub(crate) generation: u64,
    pub(crate) runs: u64,
    /// Faults that landed, across every run.
    pub(crate) adversity: u64,
    pub(crate) guest_edges: u64,
    /// Findings by run digest, so one run reached twice is one finding; iterated in digest order.
    pub(crate) findings: std::collections::BTreeMap<Digest, Finding>,
    /// Corpus entries each operator has contributed.
    pub(crate) credit: std::collections::BTreeMap<Operator, u64>,
    /// What the guests promised, counted across every run.
    pub(crate) properties: crate::properties::Properties,
}

impl Campaign {
    /// A campaign that has run nothing.
    #[must_use]
    pub fn new(master: Seed) -> Self {
        Self {
            corpus: Corpus::new(),
            master,
            generation: 0,
            runs: 0,
            adversity: 0,
            guest_edges: 0,
            findings: std::collections::BTreeMap::new(),
            credit: std::collections::BTreeMap::new(),
            properties: crate::properties::Properties::new(),
        }
    }

    /// The corpus as it stands.
    #[must_use]
    pub const fn corpus(&self) -> &Corpus {
        &self.corpus
    }

    /// Which generation is next.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Runs folded in so far.
    #[must_use]
    pub const fn runs(&self) -> u64 {
        self.runs
    }

    /// How much adversity actually landed across the campaign.
    ///
    /// Zero with no findings means the campaign injected nothing.
    #[must_use]
    pub const fn adversity(&self) -> u64 {
        self.adversity
    }

    /// How many edges the guest has, as the largest any run reported.
    ///
    /// The largest, since a guest reports only the code it has loaded so far.
    #[must_use]
    pub const fn guest_edges(&self) -> u64 {
        self.guest_edges
    }

    /// What has been found, in digest order.
    #[must_use]
    pub const fn findings(&self) -> &std::collections::BTreeMap<Digest, Finding> {
        &self.findings
    }

    /// How many entries each operator contributed.
    #[must_use]
    pub const fn credit(&self) -> &std::collections::BTreeMap<Operator, u64> {
        &self.credit
    }

    /// What the guests promised about themselves, and how often it held.
    #[must_use]
    pub const fn properties(&self) -> &crate::properties::Properties {
        &self.properties
    }

    /// The work for the next generation.
    ///
    /// A pure function of the corpus, master seed and generation, so a dead worker's items can be
    /// reissued by planning again.
    #[must_use]
    pub fn plan(&self, size: usize) -> Vec<WorkItem> {
        // Computed once; the corpus does not change while a generation is planned.
        let order = self.corpus.order();
        let pick = |nth: usize| order.get(nth.checked_rem(order.len())?).copied();

        (0..size)
            .map(|nth| {
                // Each item draws from its own stream: master seed, generation and position.
                let mut choices = Choices::new(self.item_seed(nth));
                let index = nth as u32;

                match pick(nth) {
                    None => {
                        // An empty corpus: the item is a fresh seed.
                        WorkItem {
                            index,
                            seed: self.item_seed(nth),
                            prefix: Vec::new(),
                            origin: Origin::Seeded,
                        }
                    }
                    Some(parent) => {
                        let entry = &self.corpus.entries()[parent];
                        let parent = entry.id;
                        let donor =
                            pick(nth + 1).map(|i| self.corpus.entries()[i].prefix.as_slice());
                        let mut prefix = entry.prefix.clone();
                        // The parent's draws let a mutation aim at one kind of decision.
                        let context = Context { donor, journal: Some(entry.draws()) };
                        let operator = mutate(&mut prefix, context, &mut choices);
                        WorkItem {
                            index,
                            seed: entry.seed,
                            prefix,
                            origin: Origin::Mutated { parent, operator },
                        }
                    }
                }
            })
            .collect()
    }

    /// Folds a generation's results in and moves to the next.
    ///
    /// Results are sorted by index first, which makes `-j 8` and `-j 1` agree.
    pub fn fold(&mut self, planned: &[WorkItem], mut results: Vec<RunResult>) -> Progress {
        #[cfg(not(feature = "chaos-worker-order"))]
        results.sort_by_key(|r| r.index);
        #[cfg(feature = "chaos-worker-order")]
        crate::chaos::in_arrival_order(&mut results);

        // A reissued item can answer twice; the first answer in sorted order is kept.
        let mut seen_index = std::collections::BTreeSet::new();
        results.retain(|result| seen_index.insert(result.index));

        let by_index = index(planned);
        let mut progress = Progress::default();

        // Visits are noted for the whole generation first, against the corpus the plan saw.
        for result in &results {
            let Some(item) = by_index.get(&result.index) else {
                continue;
            };
            if let Origin::Mutated { parent, .. } = item.origin {
                self.corpus.note_visit(parent);
            }
        }

        for result in &results {
            let Some(item) = by_index.get(&result.index) else {
                // Unplanned work cannot be attributed and is dropped.
                continue;
            };
            progress.runs += 1;
            self.runs += 1;
            self.adversity += u64::from(result.adversity);
            self.guest_edges = self.guest_edges.max(result.guest_edges);

            for (name, evaluations, held) in &result.observed {
                self.properties.observe_tally(name, *evaluations, *held, result.digest);
            }

            if let Some(kind) = &result.outcome {
                let finding = Finding {
                    digest: result.digest,
                    kind: kind.clone(),
                    seed: item.seed,
                    prefix: item.prefix.clone(),
                    generation: self.generation,
                };
                // The first run to reach a finding is kept: a later one of the same digest is the
                // same run, met again.
                if let std::collections::btree_map::Entry::Vacant(slot) =
                    self.findings.entry(result.digest)
                {
                    slot.insert(finding);
                    progress.fresh_findings += 1;
                }
            }

            if self.corpus.consider(offer_of(item, result)) {
                progress.kept += 1;
                if let Origin::Mutated { operator, .. } = item.origin {
                    *self.credit.entry(operator).or_default() += 1;
                }
            }
        }

        self.generation += 1;
        progress
    }

    /// Takes in tapes a previous campaign left, with what they turned out to cover.
    ///
    /// Advances no counter, so a resumed campaign's budget counts the same as a fresh one's.
    pub fn adopt(&mut self, carried: &[WorkItem], mut results: Vec<RunResult>) {
        results.sort_by_key(|r| r.index);
        let by_index = index(carried);
        for result in &results {
            let Some(item) = by_index.get(&result.index) else {
                continue;
            };
            self.corpus.consider(offer_of(item, result));
        }
    }

    /// The seed for one item of the current generation.
    ///
    /// Derived from position, so an item's tape is the same with any worker count.
    fn item_seed(&self, nth: usize) -> Seed {
        let mixed = self
            .master
            .get()
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(self.generation.wrapping_mul(0x0000_0100_0000_01B3))
            .wrapping_add(nth as u64);
        Seed(mixed)
    }
}

#[cfg(test)]
mod tests {
    use bohr_types::{EventId, TapeOffset};

    use super::*;

    fn covering(edges: &[usize]) -> CoverageMap {
        let mut map = CoverageMap::new();
        for at in edges {
            map.as_bytes_mut()[*at % bohr_types::DEFAULT_COVERAGE_MAP_BYTES] = 1;
        }
        map
    }

    /// The draws a stand-in run made: two sites, four bytes each, at the head of the tape.
    fn drew(prefix: &[u8]) -> Journal {
        let mut journal = Journal::new();
        if prefix.len() >= 8 {
            journal.push(TapeOffset(0), EventId(1), bohr_tape::DrawSite::NetDeliver, 4, 0);
            journal.push(TapeOffset(4), EventId(2), bohr_tape::DrawSite::SchedPick, 4, 0);
        }
        journal
    }

    /// A stand-in run: the tape alone decides coverage and finding.
    fn answer(item: &WorkItem) -> RunResult {
        let signature = item.prefix.iter().map(|b| usize::from(*b)).sum::<usize>();
        RunResult {
            index: item.index,
            digest: Digest::of(&item.prefix),
            outcome: (signature % 97 == 0 && !item.prefix.is_empty())
                .then(|| "property violated".to_owned()),
            coverage: covering(&[signature % 512, (signature / 3) % 512]),
            guest_edges: 0,
            journal: drew(&item.prefix),
            adversity: 0,
            observed: Vec::new(),
        }
    }

    fn campaign_over(generations: usize, size: usize, shuffle: bool) -> Campaign {
        let mut campaign = Campaign::new(Seed(42));
        for g in 0..generations {
            let planned = campaign.plan(size);
            let mut results: Vec<RunResult> = planned.iter().map(answer).collect();
            if shuffle {
                // Any arrival order will do; the fold must ignore it.
                results.reverse();
                results.rotate_left(g % size.max(1));
            }
            campaign.fold(&planned, results);
        }
        campaign
    }

    /// Arrival order does not reach the campaign.
    #[test]
    fn results_arriving_in_any_order_produce_the_same_campaign() {
        let ordered = campaign_over(12, 16, false);
        let jumbled = campaign_over(12, 16, true);

        assert_eq!(ordered.runs(), jumbled.runs());
        assert_eq!(ordered.generation(), jumbled.generation());
        assert_eq!(
            ordered.findings().keys().collect::<Vec<_>>(),
            jumbled.findings().keys().collect::<Vec<_>>(),
            "the set of findings depended on which worker was quicker"
        );
        assert_eq!(
            ordered.corpus().entries().iter().map(|e| &e.prefix).collect::<Vec<_>>(),
            jumbled.corpus().entries().iter().map(|e| &e.prefix).collect::<Vec<_>>(),
            "the corpus depended on arrival order"
        );
        assert_eq!(ordered.corpus().seen().digest(), jumbled.corpus().seen().digest());
    }

    /// One master seed and the same answers give the same campaign.
    #[test]
    fn one_master_seed_is_one_campaign() {
        let first = campaign_over(8, 12, false);
        let again = campaign_over(8, 12, false);
        assert_eq!(
            first.corpus().entries().iter().map(|e| &e.prefix).collect::<Vec<_>>(),
            again.corpus().entries().iter().map(|e| &e.prefix).collect::<Vec<_>>()
        );
        assert_eq!(first.findings().len(), again.findings().len());
    }

    /// Planning is idempotent between folds.
    #[test]
    fn planning_twice_without_folding_plans_the_same_work() {
        let campaign = campaign_over(4, 8, false);
        let first = campaign.plan(8);
        let again = campaign.plan(8);
        assert_eq!(first.len(), again.len());
        for (a, b) in first.iter().zip(again.iter()) {
            assert_eq!(a.prefix, b.prefix);
            assert_eq!(a.seed.get(), b.seed.get());
        }
    }

    /// The corpus grows.
    #[test]
    fn a_campaign_finds_more_than_it_started_with() {
        let campaign = campaign_over(10, 16, false);
        assert!(campaign.corpus().len() > 1, "the corpus never grew past its first entry");
        assert!(campaign.runs() == 160);
    }

    /// The site-burst mutation is reached, which needs the parent's draws.
    #[test]
    fn a_campaign_reaches_the_mutation_that_aims_at_one_draw_site() {
        let mut campaign = Campaign::new(Seed(9));
        let mut aimed = false;
        for _ in 0..12 {
            let planned = campaign.plan(16);
            aimed |= planned.iter().any(|item| {
                matches!(item.origin, Origin::Mutated { operator: Operator::SiteBurst, .. })
            });
            let results = planned.iter().map(answer).collect();
            campaign.fold(&planned, results);
        }
        assert!(aimed, "the parent's draws never reached the mutator");
    }

    /// One run reached twice is one finding.
    #[test]
    fn the_same_run_found_twice_is_one_finding() {
        let mut campaign = Campaign::new(Seed(1));
        let planned = campaign.plan(2);
        let shared = Digest::of(b"one run");
        let results = planned
            .iter()
            .map(|item| RunResult {
                index: item.index,
                digest: shared,
                outcome: Some("guest crashed".to_owned()),
                coverage: covering(&[1]),
                guest_edges: 0,
                journal: Journal::new(),
                adversity: 0,
                observed: Vec::new(),
            })
            .collect();
        let progress = campaign.fold(&planned, results);
        assert_eq!(progress.runs, 2);
        assert_eq!(progress.fresh_findings, 1);
        assert_eq!(campaign.findings().len(), 1);
    }

    /// A `sometimes` no run satisfied is reported by the campaign.
    #[test]
    fn a_promise_no_run_ever_kept_is_what_the_campaign_reports() {
        let mut campaign = Campaign::new(Seed(3));
        let planned = campaign.plan(2);
        let results = planned
            .iter()
            .map(|item| RunResult {
                observed: vec![
                    ("a leader was elected".to_owned(), 1, u64::from(item.index == 0)),
                    ("a leader changed under a partition".to_owned(), 1, 0),
                ],
                ..answer(item)
            })
            .collect();
        campaign.fold(&planned, results);

        let properties = campaign.properties();
        assert_eq!(properties.unsatisfied(), vec!["a leader changed under a partition"]);
        assert_eq!(properties.tallies()["a leader was elected"].evaluations, 2);
        assert_eq!(properties.tallies()["a leader was elected"].satisfied, 1);
    }

    /// A finding reached again in a later generation keeps the generation it was first reached in.
    #[test]
    fn a_finding_keeps_the_generation_it_was_first_reached_in() {
        let mut campaign = Campaign::new(Seed(9));
        let crash = |item: &WorkItem| RunResult {
            digest: Digest::of(b"the same run"),
            outcome: Some("crashed".to_owned()),
            ..answer(item)
        };
        for _ in 0..3 {
            let planned = campaign.plan(2);
            let results = planned.iter().map(crash).collect();
            campaign.fold(&planned, results);
        }
        let found = &campaign.findings()[&Digest::of(b"the same run")];
        assert_eq!(found.generation, 0, "the generation moved to a later meeting");
    }

    /// A result answered twice by a reissued item is folded once.
    #[test]
    fn a_result_folded_twice_is_folded_once() {
        let mut campaign = Campaign::new(Seed(5));
        let planned = campaign.plan(2);
        let answers: Vec<RunResult> = planned.iter().map(answer).collect();

        // The first item answered twice.
        let mut doubled = answers.clone();
        doubled.push(answers[0].clone());

        let progress = campaign.fold(&planned, doubled);
        assert_eq!(progress.runs, 2, "a repeated answer was counted as another run");
        assert_eq!(campaign.runs(), 2);
    }

    /// A result for unplanned work is dropped.
    #[test]
    fn a_result_for_unplanned_work_is_dropped_rather_than_guessed_at() {
        let mut campaign = Campaign::new(Seed(1));
        let planned = campaign.plan(1);
        let stray = RunResult {
            index: 99,
            digest: Digest::of(b"stray"),
            outcome: None,
            coverage: covering(&[5]),
            guest_edges: 0,
            journal: Journal::new(),
            adversity: 0,
            observed: Vec::new(),
        };
        let progress = campaign.fold(&planned, vec![stray]);
        assert_eq!(progress.runs, 0, "work nobody planned was folded in");
    }
}
