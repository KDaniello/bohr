//! What the search has found worth keeping.
//!
//! An entry is a tape: a seed and the prefix bytes a run reads before the keystream takes over.
//! Kilobytes, not gigabytes — the trace that a tape produces is a pure function of it, so keeping
//! the trace as well would be keeping a cache of a deterministic computation. Anyone who wants to
//! look at a run in the corpus replays it, which costs seconds and happens only when someone
//! actually looks.
//!
//! The corpus keeps an entry when it reached somewhere nothing before it had. That is the only
//! question it asks, and asking a narrower one — "did it crash", "was it fast" — is how a search
//! ends up exploring what it already understands.

use bohr_backend::CoverageMap;
use bohr_tape::Journal;
use bohr_types::Seed;

use crate::mutate::Operator;

/// How an entry came to exist.
///
/// Kept so a campaign can report which operators are earning their place, and so an entry that
/// found something can be traced back to the one it came from.
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

/// A corpus entry, named by something that does not move.
///
/// **A position was not that.** Eviction removes an entry and shifts every position after it, so
/// the parent recorded in an [`Origin`] stopped naming the entry it came from — the traceback this
/// crate's own documentation promises went quietly wrong after the first eviction, and `adopt`
/// carried positions in from a previous campaign's corpus entirely. Handed out in order and never
/// reused.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EntryId(pub u64);

/// What a run offers the corpus: the tape that produced it, and what came back.
///
/// The coverage map is borrowed rather than given. Answering "is this worth keeping" is all the
/// corpus does with it, and that answer is reached before the entry exists — so keeping the map
/// afterwards would be four thousand entries holding sixty-four kilobytes each of counters nothing
/// reads again.
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
    /// The search prefers what it has visited least, which is what stops a campaign from spending
    /// itself on the first entry that happened to look promising.
    pub visits: u64,
    /// How many edges the coverage held.
    ///
    /// Counted once, when the entry is made, and never again. Counting it on demand means walking
    /// sixty-four kilobytes, and the two places that want it — choosing what to mutate and
    /// choosing what to drop — both want it inside a sort, where a linear scan per comparison
    /// turns a campaign into a program that spends its time counting rather than running guests.
    pub(crate) edges: usize,
    /// The draws this entry's run made inside the prefix, for the mutation that aims at one site.
    pub(crate) draws: Journal,
}

/// Draws an entry remembers, at most.
///
/// A run makes one draw per decision — hundreds of thousands of them for a traced cluster — and a
/// corpus holds thousands of entries, so the whole journal is not something a campaign can carry.
/// What a mutation can use is far smaller, and the earliest draws are the ones the rest of the run
/// follows from.
const REMEMBERED_DRAWS: usize = 256;

impl Entry {
    /// The entry an offer becomes, once the corpus has decided to keep it.
    fn from(offer: Offer<'_>, id: EntryId) -> Self {
        // Only the draws a mutation could reach: one that read past the end of the prefix took its
        // bytes from the keystream, where there is nothing to overwrite, and one that consumed
        // nothing names an empty range.
        let draws: Vec<_> = offer
            .draws
            .records()
            .iter()
            .filter(|r| r.len > 0 && r.offset < offer.prefix.len() as u64)
            .take(REMEMBERED_DRAWS)
            .copied()
            .collect();
        Self {
            id,
            seed: offer.seed,
            prefix: offer.prefix.to_vec(),
            origin: offer.origin,
            visits: 0,
            edges: offer.coverage.edges(),
            draws: Journal::from_records(draws),
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
    /// Every bucket any run has reached. The question "is this new" is one comparison against it.
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
/// A bound rather than a preference: a campaign that runs overnight will offer millions of inputs,
/// and a corpus that kept every one of them would spend the morning choosing between them.
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
    /// Kept when it reached a bucket nothing before it reached. Note the order: the entry is
    /// compared against what was seen *before* it, then folded in — comparing after folding would
    /// mean every run had already been seen, and the corpus would never grow.
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
    /// By name rather than by position: a visit noted between two folds is credited to the entry
    /// that earned it, not to whichever entry has since taken its place.
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
    /// Least-visited first, and among equals the one that covered most: a broad entry has more
    /// neighbours worth reaching. Handed out whole rather than one position at a time, because
    /// planning a generation asks for dozens of positions from one unchanged corpus, and sorting
    /// four thousand entries for each of them is a campaign that spends its time ordering a list
    /// it already ordered.
    #[must_use]
    pub fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.entries.len()).collect();
        order.sort_by_key(|i| {
            let entry = &self.entries[*i];
            (entry.visits, std::cmp::Reverse(entry.edges), *i)
        });
        order
    }

    /// Drops the entry that reached least, which is the cheapest sound answer to "which one is the
    /// rest of the corpus most nearly a copy of".
    ///
    /// Cheapest and not exact: knowing which entry contributes coverage no other entry holds would
    /// mean keeping every entry's map, and four thousand of those is a quarter of a gigabyte held
    /// to answer one question about eviction.
    ///
    /// Ties are broken towards the longer prefix and then the later position, so the choice is a
    /// function of the corpus and never of the order two equal entries were offered in.
    fn evict(&mut self) {
        let Some(worst) = (0..self.entries.len()).min_by_key(|i| {
            let entry = &self.entries[*i];
            (entry.edges, std::cmp::Reverse(entry.prefix.len()), std::cmp::Reverse(*i))
        }) else {
            return;
        };
        self.entries.remove(worst);
        self.evicted += 1;
        // `seen` is deliberately not recomputed. It records what the campaign has covered, which
        // does not become untrue because the tape that covered it is no longer kept — and a corpus
        // that forgot would re-admit the same input every time it was offered again.
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

    /// A journal is kept so a mutation can aim at one draw site — and kept bounded, because a run
    /// makes more draws than a corpus can hold and only those inside the prefix can be aimed at.
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

    /// What a campaign has covered is a fact about the campaign, not about which tapes it still
    /// holds. A corpus that forgot on eviction would keep re-admitting the input it just dropped.
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

    /// Eviction has to be a function of the corpus. If it depended on the order two equal entries
    /// arrived in, two campaigns with one master seed could end up holding different tapes.
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

    /// An entry keeps its name while the corpus drops others around it. A position did not: the
    /// parent recorded in an `Origin` stopped naming the entry it came from at the first eviction,
    /// and a visit noted afterwards was credited to whoever had moved into that slot.
    #[test]
    fn an_entry_keeps_its_name_while_the_corpus_moves_under_it() {
        let mut corpus = Corpus::with_capacity(2);
        consider(&mut corpus, &[1], 8);
        let wanted = corpus.entries()[0].id;

        consider(&mut corpus, &[1, 2, 3], 8);
        consider(&mut corpus, &[1, 2, 3, 4, 5], 8);
        assert_eq!(corpus.evicted(), 1, "nothing was dropped, so nothing moved");

        // The narrowest entry was the one dropped, so the one asked about is still held -- at a
        // different position, under the same name.
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
