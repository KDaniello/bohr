//! What the search has found worth keeping.
//!
//! An entry is a tape: a seed and the prefix a run reads before the keystream. The trace is a pure
//! function of the tape, so only the tape is kept; a run is replayed to look at it.
//!
//! An entry is kept when it reaches coverage nothing before it reached.

use bohr_tape::{Journal, Keep};
use bohr_types::{CoverageMap, Seed};

use crate::mutate::Operator;

/// How an entry came to exist.
///
/// Used to credit operators and to trace a finding back to its parent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    /// One of the tapes the campaign started from.
    Seeded,
    /// Produced by mutating the named entry, with the given operator.
    Mutated {
        /// Which entry it came from.
        parent: EntryId,
        /// What was done to it.
        operator: Operator,
    },
}

/// A corpus entry's stable name, handed out in order and never reused.
///
/// Positions shift on eviction; a name does not.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EntryId(pub u64);

/// What a run offers the corpus: the tape that produced it, and what came back.
///
/// The coverage map is borrowed: it is only needed to decide whether to keep the entry.
#[derive(Clone, Copy, Debug)]
pub struct Offer<'a> {
    /// The seed the tape extends from.
    pub seed: Seed,
    /// The bytes the run read before the keystream.
    pub prefix: &'a [u8],
    /// What the guest executed.
    pub coverage: &'a CoverageMap,
    /// What the run drew, and where.
    pub draws: &'a Journal,
    /// How the tape came to exist.
    pub origin: Origin,
}

/// One tape the search thinks is worth returning to.
#[derive(Clone, Debug)]
pub struct Entry {
    /// What names this entry for as long as the campaign runs.
    pub id: EntryId,
    /// The seed the tape extends from.
    pub seed: Seed,
    /// The bytes the run reads before the keystream.
    pub prefix: Vec<u8>,
    /// How it came to exist.
    pub origin: Origin,
    /// How often it has been chosen to mutate from.
    ///
    /// The search prefers the least visited entry.
    pub visits: u64,
    /// How many edges the coverage held.
    ///
    /// Counted once at creation, since both sorts that use it would otherwise scan the map.
    pub(crate) edges: usize,
    /// The draws this entry's run made inside the prefix, for the mutation that aims at one site.
    pub(crate) draws: Journal,
}

/// Draws an entry remembers, at most.
///
/// A traced cluster makes hundreds of thousands of draws; the earliest ones matter most.
const REMEMBERED_DRAWS: usize = 256;

/// What a run's journal need keep for the corpus: the draws [`corpus_draws`] takes, so a run
/// that keeps no log holds 256 records instead of one per draw.
#[must_use]
pub const fn corpus_keep(prefix_len: usize) -> Keep {
    Keep::Prefix { prefix: prefix_len as u64, at_most: REMEMBERED_DRAWS }
}

/// The part of a run's journal the corpus can use: the first draws inside the prefix that
/// consumed bytes, at most `REMEMBERED_DRAWS`.
///
/// A worker sends only this, so a campaign never holds a whole journal: a cluster's run draws
/// millions of times, and a generation of whole journals held a campaign past 3 GB.
#[must_use]
pub fn corpus_draws(journal: &Journal, prefix_len: usize) -> Journal {
    let draws: Vec<_> = journal
        .records()
        .iter()
        .filter(|r| r.len > 0 && r.offset < prefix_len as u64)
        .take(REMEMBERED_DRAWS)
        .copied()
        .collect();
    Journal::from_records(draws)
}

impl Entry {
    /// The entry an offer becomes, once the corpus has decided to keep it.
    fn from(offer: Offer<'_>, id: EntryId) -> Self {
        Self {
            id,
            seed: offer.seed,
            prefix: offer.prefix.to_vec(),
            origin: offer.origin,
            visits: 0,
            edges: offer.coverage.edges(),
            draws: corpus_draws(offer.draws, offer.prefix.len()),
        }
    }

    /// How many edges this entry's run reached.
    #[must_use]
    pub const fn edges(&self) -> usize {
        self.edges
    }

    /// The draws this entry's run made inside its prefix.
    #[must_use]
    pub const fn draws(&self) -> &Journal {
        &self.draws
    }
}

/// The tapes kept, and everything the campaign has ever covered.
#[derive(Clone, Debug)]
pub struct Corpus {
    pub(crate) entries: Vec<Entry>,
    /// Every bucket any run has reached.
    pub(crate) seen: CoverageMap,
    /// Entries dropped because the corpus was full.
    pub(crate) evicted: u64,
    /// The most entries to keep.
    pub(crate) capacity: usize,
    /// The name the next kept entry is given.
    pub(crate) next_id: u64,
}

/// Entries a corpus holds before it starts dropping the least useful.
///
/// A long campaign offers millions of inputs, so the corpus is bounded.
pub const DEFAULT_CAPACITY: usize = 4096;

impl Corpus {
    /// An empty corpus of the default size.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// An empty corpus that keeps at most `capacity` entries.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            seen: CoverageMap::new(),
            evicted: 0,
            capacity: capacity.max(1),
            next_id: 0,
        }
    }

    /// The entries, in the order they were kept.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// How many are kept.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing has been kept yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Everything the campaign has covered so far.
    #[must_use]
    pub const fn seen(&self) -> &CoverageMap {
        &self.seen
    }

    /// How many entries were dropped to stay within the bound.
    #[must_use]
    pub const fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Offers a run to the corpus, and says whether it was kept.
    ///
    /// Kept when it reached a new bucket. It is compared against `seen` before being folded in.
    pub fn consider(&mut self, offer: Offer<'_>) -> bool {
        if !offer.coverage.has_new_bits(&self.seen) {
            return false;
        }
        self.seen.merge(offer.coverage);
        let id = EntryId(self.next_id);
        self.next_id += 1;
        self.entries.push(Entry::from(offer, id));
        if self.entries.len() > self.capacity {
            self.evict();
        }
        true
    }

    /// Marks an entry as having been mutated from.
    ///
    /// By name, so the visit reaches the right entry after evictions.
    pub fn note_visit(&mut self, id: EntryId) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.visits += 1;
        }
    }

    /// The entry of that name, where the corpus still holds it.
    #[must_use]
    pub fn entry(&self, id: EntryId) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// The order the search returns to entries in.
    ///
    /// Least visited first, then widest coverage. Returned whole, so a generation sorts once.
    #[must_use]
    pub fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.entries.len()).collect();
        order.sort_by_key(|i| {
            let entry = &self.entries[*i];
            (entry.visits, std::cmp::Reverse(entry.edges), *i)
        });
        order
    }

    /// Drops the entry with the least coverage, an approximation that keeps no maps.
    ///
    /// Ties go to the longer prefix, then the later position, so the choice depends only on the
    /// corpus.
    fn evict(&mut self) {
        let Some(worst) = (0..self.entries.len()).min_by_key(|i| {
            let entry = &self.entries[*i];
            (entry.edges, std::cmp::Reverse(entry.prefix.len()), std::cmp::Reverse(*i))
        }) else {
            return;
        };
        self.entries.remove(worst);
        self.evicted += 1;
        // `seen` keeps what was covered, so a dropped input is not re-admitted.
    }
}

impl Default for Corpus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use bohr_types::{EventId, TapeOffset};

    use super::*;

    fn covering(edges: &[usize]) -> CoverageMap {
        let mut map = CoverageMap::new();
        for at in edges {
            map.as_bytes_mut()[*at] = 1;
        }
        map
    }

    /// A run asked to keep only what the corpus takes keeps exactly that, and still counts every
    /// draw it made.
    #[test]
    fn a_bounded_journal_keeps_what_the_corpus_takes() {
        const PREFIX: usize = 4096;
        let mut whole = Journal::new();
        let mut bounded = Journal::keeping(corpus_keep(PREFIX));
        for i in 0..2_000u64 {
            let offset = TapeOffset((i * 7) % (2 * PREFIX as u64));
            let len = (i % 5) as u32;
            for journal in [&mut whole, &mut bounded] {
                journal.push(offset, EventId(i), bohr_tape::DrawSite::NetDelay, len, i);
            }
        }
        assert_eq!(bounded.records(), corpus_draws(&whole, PREFIX).records());
        assert_eq!(bounded.records().len(), REMEMBERED_DRAWS);
        assert_eq!(bounded.len(), whole.len(), "every draw is counted");
    }

    fn offer(corpus: &mut Corpus, coverage: &CoverageMap, prefix: &[u8], draws: &Journal) -> bool {
        corpus.consider(Offer { seed: Seed(0), prefix, coverage, draws, origin: Origin::Seeded })
    }

    /// A run that covered `edges`, read `prefix` bytes and drew nothing.
    fn consider(corpus: &mut Corpus, edges: &[usize], prefix: usize) -> bool {
        offer(corpus, &covering(edges), &vec![0u8; prefix], &Journal::new())
    }

    #[test]
    fn an_input_that_went_somewhere_new_is_kept_and_one_that_did_not_is_not() {
        let mut corpus = Corpus::new();
        assert!(consider(&mut corpus, &[1, 2], 8), "the first input is always new");
        assert!(!consider(&mut corpus, &[1, 2], 8), "the same path is not worth keeping twice");
        assert!(consider(&mut corpus, &[1, 2, 3], 8), "one more edge is somewhere new");
        assert_eq!(corpus.len(), 2);
    }

    /// An entry keeps only the bounded draws inside its prefix.
    #[test]
    fn an_entry_remembers_the_draws_a_mutation_could_reach_and_no_others() {
        let mut journal = Journal::new();
        journal.push(TapeOffset(0), EventId(1), bohr_tape::DrawSite::NetDeliver, 4, 0);
        // Past the end of the prefix: those bytes came from the keystream.
        journal.push(TapeOffset(64), EventId(2), bohr_tape::DrawSite::NetDelay, 4, 0);
        // Consumed nothing, so it names an empty range.
        journal.push(TapeOffset(8), EventId(3), bohr_tape::DrawSite::SchedPick, 0, 0);

        let mut corpus = Corpus::new();
        assert!(offer(&mut corpus, &covering(&[1]), &[0u8; 16], &journal));

        let kept = corpus.entries()[0].draws().records();
        assert_eq!(kept.len(), 1, "a draw a mutation cannot reach was kept anyway");
        assert_eq!(kept[0].offset, 0);
    }

    /// Coverage survives eviction, so a dropped input is not re-admitted.
    #[test]
    fn what_was_covered_survives_the_tape_that_covered_it() {
        let mut corpus = Corpus::with_capacity(2);
        consider(&mut corpus, &[1], 8);
        consider(&mut corpus, &[1, 2], 8);
        consider(&mut corpus, &[1, 2, 3], 8);

        assert_eq!(corpus.len(), 2, "the bound was not held");
        assert_eq!(corpus.evicted(), 1);
        assert!(!consider(&mut corpus, &[1], 8), "the dropped path was admitted again");
    }

    /// Eviction ignores the order equal entries arrived in.
    #[test]
    fn eviction_drops_the_same_entry_whatever_order_equals_arrived_in() {
        let build = |order: [usize; 3]| {
            let mut corpus = Corpus::with_capacity(2);
            let offered: [&[usize]; 3] = [&[1], &[1, 2], &[1, 2, 3, 4]];
            for i in order {
                consider(&mut corpus, offered[i], 8);
            }
            corpus.entries().iter().map(Entry::edges).collect::<Vec<_>>()
        };
        assert_eq!(build([0, 1, 2]), vec![2, 4], "the narrowest entry is the one to drop");
    }

    #[test]
    fn the_least_visited_entry_is_the_one_to_return_to() {
        let mut corpus = Corpus::new();
        consider(&mut corpus, &[1], 8);
        consider(&mut corpus, &[1, 2], 8);

        let first = corpus.order()[0];
        corpus.note_visit(corpus.entries()[first].id);
        let second = corpus.order()[0];
        assert_ne!(first, second, "a visited entry stayed at the front of the queue");
    }

    /// An entry keeps its name while others are dropped.
    #[test]
    fn an_entry_keeps_its_name_while_the_corpus_moves_under_it() {
        let mut corpus = Corpus::with_capacity(2);
        consider(&mut corpus, &[1], 8);
        let wanted = corpus.entries()[0].id;

        consider(&mut corpus, &[1, 2, 3], 8);
        consider(&mut corpus, &[1, 2, 3, 4, 5], 8);
        assert_eq!(corpus.evicted(), 1, "nothing was dropped, so nothing moved");

        // The narrowest entry was dropped; this one moved but kept its name.
        let held = corpus.entry(wanted);
        assert!(held.is_none() || held.expect("held").id == wanted);

        let survivor = corpus.entries()[0].id;
        corpus.note_visit(survivor);
        assert_eq!(corpus.entry(survivor).expect("held").visits, 1, "the visit went elsewhere");
    }

    #[test]
    fn an_empty_corpus_offers_nothing_rather_than_pretending() {
        assert!(Corpus::new().order().is_empty());
        assert!(Corpus::new().is_empty());
    }
}
