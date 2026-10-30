//! The campaign: what to run next, and what the answers meant.
//!
//! # Why this is generation-based
//!
//! A coverage-guided search is a feedback loop: what it explores next depends on what came back.
//! Run those runs on eight workers and what came back *first* depends on which worker was quicker,
//! which is a fact about the machine — so the obvious design makes a campaign unrepeatable, and
//! the promise that a finding from worker seven replays under `-j 1` becomes a hope.
//!
//! So a campaign proceeds in **generations**. The work for generation *N* is computed from the
//! corpus as it stood at the end of *N−1* and from the master seed, and from nothing else. Inside
//! a generation the items carry indices, workers take them in any order at any speed, and the
//! results are folded back **in index order** rather than in arrival order. Every fold is then a
//! pure function of a sorted list, so the campaign is a pure function of its master seed and its
//! budget: worker count and worker timing cannot reach a decision, because they cannot reach the
//! work list.
//!
//! The price is that a generation ends when its slowest item does. That is paid for by making a
//! generation several times larger than the worker count, so a straggler costs a fraction of one
//! rather than a whole one — see `docs/ARCHITECTURE.md` §6.2.

use bohr_backend::CoverageMap;
use bohr_tape::Journal;
use bohr_types::{Digest, Seed};

use crate::{
    choices::Choices,
    corpus::{Corpus, Offer, Origin},
    mutate::{mutate, Context, Operator},
};

/// One run the campaign wants carried out.
#[derive(Clone, Debug)]
pub struct WorkItem {
    /// Position in this generation. What the results are sorted by, and the only thing that
    /// decides the order they are folded in.
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
/// Deliberately not the run report: the explorer needs four facts about a run, and taking the
/// whole report would tie the search to the shape of the kernel's output.
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
    /// Reported because a map with fewer counters than this cannot tell every path apart, and a
    /// search whose feedback has quietly stopped discriminating looks exactly like a search that
    /// has covered everything there is.
    pub guest_edges: u64,
    /// What it drew, and where — what a journal-guided mutation aims with.
    pub journal: Journal,
    /// How many faults landed on the guest in this run.
    ///
    /// Summed across the campaign and reported, because a campaign that injected nothing looks
    /// exactly like one that injected plenty and found nothing. Faults are drawn, so whether any
    /// fired is a fact about the run and not about the configuration.
    pub adversity: u32,
    /// Campaign-scoped properties the guest evaluated, and whether each held this time.
    ///
    /// Only the campaign-scoped kinds: whether an `always` held is this run's verdict and is
    /// already in `outcome`, while whether a `sometimes` has *ever* held is a question one run
    /// cannot answer and this is the material for answering it.
    pub observed: Vec<(String, bool)>,
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
/// Built once per generation. Walking the list per result is quadratic in the generation size,
/// which is a cost that grows with the only knob a long campaign has to turn.
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
    corpus: Corpus,
    master: Seed,
    generation: u64,
    runs: u64,
    /// Faults that landed, across every run.
    adversity: u64,
    guest_edges: u64,
    /// Findings by the digest of the run that produced them, so the same run reached twice is one
    /// finding. A map keyed by digest and iterated in its order, never in discovery order.
    findings: std::collections::BTreeMap<Digest, Finding>,
    /// How many entries each operator has contributed, for a report that can say which are
    /// earning their place.
    credit: std::collections::BTreeMap<Operator, u64>,
    /// What the guests promised, counted across every run.
    properties: crate::properties::Properties,
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
    /// Nought here and no findings is a campaign that asked nothing, which is a different thing
    /// from a campaign that asked and was answered well — and nothing else in a summary tells them
    /// apart.
    #[must_use]
    pub const fn adversity(&self) -> u64 {
        self.adversity
    }

    /// How many edges the guest has, as the largest any run reported.
    ///
    /// The largest rather than the last, because a guest that loads more of itself on one path
    /// than another reports what it had reached; what the map has to hold is the whole of it.
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
    /// A pure function of the corpus, the master seed and the generation number. Calling it twice
    /// without folding anything in between produces the same list, which is what a worker that
    /// died mid-generation is reissued from.
    #[must_use]
    pub fn plan(&self, size: usize) -> Vec<WorkItem> {
        // Asked for once and read many times. It is a function of the corpus, which does not move
        // while a generation is being planned, and rebuilding it per item would have a campaign
        // sorting thousands of entries dozens of times to learn the same answer.
        let order = self.corpus.order();
        let pick = |nth: usize| order.get(nth.checked_rem(order.len())?).copied();

        (0..size)
            .map(|nth| {
                // Every item's decisions come off a stream of its own, derived from the master
                // seed, the generation and the position. Nothing else can reach it — not the wall
                // clock, not which worker will take it, not what any other item did.
                let mut choices = Choices::new(self.item_seed(nth));
                let index = nth as u32;

                match pick(nth) {
                    None => {
                        // Nothing to mutate yet: the first generation is drawn from the master
                        // seed alone, which is what gives a campaign somewhere to start.
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
                        // The parent's own draws, which is what lets a mutation aim at one kind of
                        // decision — what the network did, what the disk did — rather than at a
                        // byte whose meaning it does not know.
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
    /// The results are sorted by index first, whatever order they arrived in. That single line is
    /// what makes `-j 8` and `-j 1` agree, so it is worth being plain about: everything after it
    /// is a fold over a list that does not depend on the machine.
    pub fn fold(&mut self, planned: &[WorkItem], mut results: Vec<RunResult>) -> Progress {
        #[cfg(not(feature = "chaos-worker-order"))]
        results.sort_by_key(|r| r.index);
        #[cfg(feature = "chaos-worker-order")]
        crate::chaos::in_arrival_order(&mut results);

        // **One answer to one question.** The plan is reissued to replace a worker that died
        // mid-generation, so a result arriving from both the dead worker and its replacement is a
        // shape the protocol produces by design. Folded twice, it counted its run, its adversity
        // and its property observations twice: determinism held and the numbers a campaign is
        // judged by did not. The first is kept, which is the only choice a sorted list makes the
        // same way every time.
        let mut seen_index = std::collections::BTreeSet::new();
        results.retain(|result| seen_index.insert(result.index));

        let by_index = index(planned);
        let mut progress = Progress::default();

        // Visits are noted before anything is kept, and for the whole generation at once, so that
        // every item of a generation is credited to the entry the plan was built from rather than
        // to whatever the corpus holds by the time its own result is folded.
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
                // A result for work nobody planned. Dropped rather than folded: it cannot be
                // attributed, and attributing it to the wrong item would corrupt the corpus.
                continue;
            };
            progress.runs += 1;
            self.runs += 1;
            self.adversity += u64::from(result.adversity);
            self.guest_edges = self.guest_edges.max(result.guest_edges);

            for (name, held) in &result.observed {
                self.properties.observe(name, *held, result.digest);
            }

            if let Some(kind) = &result.outcome {
                let finding = Finding {
                    digest: result.digest,
                    kind: kind.clone(),
                    seed: item.seed,
                    prefix: item.prefix.clone(),
                    generation: self.generation,
                };
                if self.findings.insert(result.digest, finding).is_none() {
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
    /// Not a generation: it advances no counter and plans nothing. A carried corpus is what the
    /// campaign starts from, and counting the runs that rebuilt it would make a resumed campaign
    /// and a fresh one disagree about how much work the same budget bought.
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
    /// Derived rather than counted, so that generation five item three is the same tape whether
    /// the campaign reached it in one worker or in eight.
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
            map.as_bytes_mut()[*at % bohr_backend::DEFAULT_COVERAGE_MAP_BYTES] = 1;
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

    /// A stand-in for running a guest: the tape decides what is covered and whether it is a
    /// finding, deterministically, which is all the campaign is entitled to assume.
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
                // Whatever order eight workers might have finished in. Reversing and rotating is
                // enough: what is under test is that the fold does not care, not that we can
                // imitate a scheduler.
                results.reverse();
                results.rotate_left(g % size.max(1));
            }
            campaign.fold(&planned, results);
        }
        campaign
    }

    /// The gate this whole design exists for, proven before a single process is spawned.
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

    /// Planning must not depend on anything the campaign does not hold. Two campaigns given the
    /// same master seed and the same answers are the same campaign.
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

    /// Planning is idempotent between folds, which is what lets a dead worker's items be reissued
    /// without the campaign having moved on underneath them.
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

    /// The corpus has to grow, or the search is a random walk with extra steps.
    #[test]
    fn a_campaign_finds_more_than_it_started_with() {
        let campaign = campaign_over(10, 16, false);
        assert!(campaign.corpus().len() > 1, "the corpus never grew past its first entry");
        assert!(campaign.runs() == 160);
    }

    /// The mutation nothing else can offer: replacing every byte one kind of decision consumed.
    /// It is available only where the parent run's draws are known, so a campaign that did not
    /// carry them would leave the operator implemented, tested in isolation and never reached.
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

    /// One run reached twice is one finding. A campaign that counted it twice would report a
    /// number that grows with how long it ran rather than with what is wrong.
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

    /// A `sometimes` nobody ever satisfied is the campaign's other answer, and it is only ever
    /// assembled here: no single run knows whether a promise was kept somewhere else.
    #[test]
    fn a_promise_no_run_ever_kept_is_what_the_campaign_reports() {
        let mut campaign = Campaign::new(Seed(3));
        let planned = campaign.plan(2);
        let results = planned
            .iter()
            .map(|item| RunResult {
                observed: vec![
                    ("a leader was elected".to_owned(), item.index == 0),
                    ("a leader changed under a partition".to_owned(), false),
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

    /// The plan is reissued to replace a worker that died mid-generation, so a result arriving
    /// from both the dead worker and its replacement is a shape the protocol produces by design.
    /// Folded twice it counted its run, its adversity and its observations twice: the numbers a
    /// campaign is judged by moved with how many workers had failed.
    #[test]
    fn a_result_folded_twice_is_folded_once() {
        let mut campaign = Campaign::new(Seed(5));
        let planned = campaign.plan(2);
        let answers: Vec<RunResult> = planned.iter().map(answer).collect();

        // The first item answered twice, exactly as a reissued plan produces it.
        let mut doubled = answers.clone();
        doubled.push(answers[0].clone());

        let progress = campaign.fold(&planned, doubled);
        assert_eq!(progress.runs, 2, "a repeated answer was counted as another run");
        assert_eq!(campaign.runs(), 2);
    }

    /// A result nobody asked for cannot be attributed, and attributing it anyway would put another
    /// item's tape in the corpus under this one's coverage.
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
