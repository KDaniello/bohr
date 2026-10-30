//! Turning one tape into another.
//!
//! A corpus entry is a tape prefix: the bytes a run reads before the keystream takes over. So
//! mutating an input means mutating those bytes, and every operator here is a rule about how.
//!
//! **The rule they all obey: an operator overwrites or appends, never inserts or removes.** The
//! draw ABI's founding property is that a draw's byte consumption depends on the request and never
//! on the value, which is what makes one flipped byte change exactly one decision. Inserting a
//! byte would break that from the other side — every later draw would read from a shifted offset,
//! and a single mutation would perturb the whole run. Appending is safe for the same reason:
//! bytes past the old end were coming from the keystream anyway, so writing them explicitly moves
//! nothing.
//!
//! The operators are AFL's, with one addition nothing else can offer: the draw journal says which
//! byte ranges a run's *network* decisions consumed, or its *disk* ones, so a mutation can aim at
//! them. Every other fuzzer needs a grammar to do that; here it falls out of recording the draws.

use bohr_tape::Journal;

use crate::choices::Choices;

/// Values that sit on the edge of something: a boundary, a sign change, a maximum.
///
/// Bugs cluster at them, which is the whole reason a mutator carries a table of constants rather
/// than trusting a uniform byte to land on 255 by chance.
const INTERESTING: [u8; 8] = [0x00, 0x01, 0x02, 0x0F, 0x10, 0x7F, 0x80, 0xFF];

/// Deltas an arithmetic mutation applies. Small, because a bug at a boundary is usually one step
/// away from it rather than a hundred.
const DELTAS: [i16; 8] = [-16, -8, -4, -1, 1, 4, 8, 16];

/// Bytes an [`Operator::Extend`] adds at a time.
const EXTEND_BY: usize = 32;

/// The largest prefix a corpus entry may grow to.
///
/// A bound rather than a policy: without it, a campaign that keeps extending its best entry turns
/// a corpus of tapes into a corpus of one enormous tape, and every run pays to read it.
pub const MAX_PREFIX_BYTES: usize = 64 * 1024;

/// What was done to a prefix.
///
/// Reported so a campaign can say which kinds of mutation are finding anything. An operator that
/// never contributes an entry is one to reconsider, and that is only visible if it is named.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Operator {
    /// One bit inverted.
    BitFlip,
    /// One byte inverted.
    ByteFlip,
    /// One byte stepped by a small amount.
    Arithmetic,
    /// One byte replaced by a boundary value.
    Interesting,
    /// A run of bytes overwritten from another entry.
    Splice,
    /// Fresh bytes appended, making explicit what the keystream was supplying.
    Extend,
    /// Every byte one draw site consumed, replaced at once.
    ///
    /// The journal-guided one: "change what the network did and leave the schedule alone" is a
    /// sentence no byte-blind mutator can say.
    SiteBurst,
    /// Several of the above, one after another.
    Havoc,
}

impl Operator {
    /// The name a report uses.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::BitFlip => "bit-flip",
            Self::ByteFlip => "byte-flip",
            Self::Arithmetic => "arithmetic",
            Self::Interesting => "interesting",
            Self::Splice => "splice",
            Self::Extend => "extend",
            Self::SiteBurst => "site-burst",
            Self::Havoc => "havoc",
        }
    }

    /// Every operator, for a report that must name the ones that found nothing as well.
    #[must_use]
    pub const fn all() -> [Self; 8] {
        [
            Self::BitFlip,
            Self::ByteFlip,
            Self::Arithmetic,
            Self::Interesting,
            Self::Splice,
            Self::Extend,
            Self::SiteBurst,
            Self::Havoc,
        ]
    }
}

/// What a mutation may draw on besides the prefix itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct Context<'a> {
    /// Another entry's prefix, for an operator that copies between them.
    pub donor: Option<&'a [u8]>,
    /// What the parent run drew, and where. Absent for an entry that has never been run.
    pub journal: Option<&'a Journal>,
}

/// Applies one mutation, chosen from the stream, and says which it was.
///
/// An empty prefix is extended rather than mutated: there is nothing to flip, and a search that
/// answered "nothing to do" would stall on its first generation.
pub fn mutate(prefix: &mut Vec<u8>, context: Context<'_>, choices: &mut Choices) -> Operator {
    if prefix.is_empty() {
        extend(prefix, choices);
        return Operator::Extend;
    }

    let available = available_operators(prefix, context);
    let chosen = *choices.pick(&available).unwrap_or(&Operator::BitFlip);
    apply(chosen, prefix, context, choices);
    chosen
}

/// The operators that can do something here.
///
/// An operator with nothing to work on — a splice with no donor, a site burst with no journal —
/// is left out rather than chosen and skipped: a mutation that changes nothing wastes a whole run.
fn available_operators(prefix: &[u8], context: Context<'_>) -> Vec<Operator> {
    let mut available =
        vec![Operator::BitFlip, Operator::ByteFlip, Operator::Arithmetic, Operator::Interesting];
    if prefix.len() < MAX_PREFIX_BYTES {
        available.push(Operator::Extend);
    }
    if context.donor.is_some_and(|d| !d.is_empty()) {
        available.push(Operator::Splice);
    }
    if context.journal.is_some_and(|j| !j.records().is_empty()) {
        available.push(Operator::SiteBurst);
    }
    available.push(Operator::Havoc);
    available
}

fn apply(op: Operator, prefix: &mut Vec<u8>, context: Context<'_>, choices: &mut Choices) {
    match op {
        Operator::BitFlip => {
            let at = choices.below(prefix.len());
            let bit = choices.below(8);
            prefix[at] ^= 1 << bit;
        }
        Operator::ByteFlip => {
            let at = choices.below(prefix.len());
            prefix[at] = !prefix[at];
        }
        Operator::Arithmetic => {
            let at = choices.below(prefix.len());
            let delta = DELTAS[choices.below(DELTAS.len())];
            prefix[at] = prefix[at].wrapping_add_signed(delta as i8);
        }
        Operator::Interesting => {
            let at = choices.below(prefix.len());
            prefix[at] = INTERESTING[choices.below(INTERESTING.len())];
        }
        Operator::Splice => {
            if let Some(donor) = context.donor.filter(|d| !d.is_empty()) {
                let from = choices.below(donor.len());
                let into = choices.below(prefix.len());
                let span = (donor.len() - from).min(prefix.len() - into);
                let span = 1 + choices.below(span.max(1));
                let span = span.min(donor.len() - from).min(prefix.len() - into);
                prefix[into..into + span].copy_from_slice(&donor[from..from + span]);
            }
        }
        Operator::Extend => extend(prefix, choices),
        Operator::SiteBurst => {
            if let Some(journal) = context.journal {
                burst(prefix, journal, choices);
            }
        }
        Operator::Havoc => {
            // Between two and five in a row. One mutation reaches a neighbour; a handful reaches
            // somewhere a chain of single steps would take a whole campaign to walk to.
            let rounds = 2 + choices.below(4);
            for _ in 0..rounds {
                let available = available_operators(prefix, context)
                    .into_iter()
                    .filter(|o| *o != Operator::Havoc)
                    .collect::<Vec<_>>();
                let step = *choices.pick(&available).unwrap_or(&Operator::BitFlip);
                apply(step, prefix, context, choices);
            }
        }
    }
}

/// Appends fresh bytes, up to the bound.
fn extend(prefix: &mut Vec<u8>, choices: &mut Choices) {
    let room = MAX_PREFIX_BYTES.saturating_sub(prefix.len()).min(EXTEND_BY);
    for _ in 0..room {
        prefix.push(choices.byte());
    }
}

/// Replaces every byte one draw site consumed.
///
/// The site is chosen among those the parent run actually used, so the mutation lands on decisions
/// that happened rather than on decisions that might have.
fn burst(prefix: &mut [u8], journal: &Journal, choices: &mut Choices) {
    // Grouped by the code the journal stored rather than by a decoded name: a record written by a
    // newer build can name a site this one has no case for, and a mutation aimed at every draw
    // that shares a code is right whether or not this build knows what to call it.
    let mut sites: Vec<u16> = journal.records().iter().map(|r| r.site).collect();
    sites.sort_unstable();
    sites.dedup();
    let Some(site) = choices.pick(&sites).copied() else {
        return;
    };

    for record in journal.records() {
        if record.site != site {
            continue;
        }
        let from = record.offset as usize;
        let to = (from + record.len as usize).min(prefix.len());
        for byte in prefix.iter_mut().take(to).skip(from) {
            *byte = choices.byte();
        }
    }
}

#[cfg(test)]
mod tests {
    use bohr_types::Seed;

    use super::*;

    fn seeded(n: u64) -> Choices {
        Choices::new(Seed(n))
    }

    /// The property the whole crate rests on: a mutation may overwrite and may append, and may
    /// never move a byte that was already there. If it could, one mutation would shift every later
    /// draw and the search would be perturbing runs wholesale instead of one decision at a time.
    #[test]
    fn no_operator_moves_a_byte_that_was_already_there() {
        for seed in 0..200u64 {
            let original: Vec<u8> = (0..128).map(|i| (i * 7 % 251) as u8).collect();
            let donor: Vec<u8> = (0..128).map(|i| (i * 13 % 241) as u8).collect();
            let mut prefix = original.clone();
            let mut choices = seeded(seed);

            let op =
                mutate(&mut prefix, Context { donor: Some(&donor), journal: None }, &mut choices);

            assert!(
                prefix.len() >= original.len(),
                "{} shortened the prefix, which shifts every draw after it",
                op.name()
            );
            if op == Operator::Extend {
                assert_eq!(
                    &prefix[..original.len()],
                    &original[..],
                    "extend must add bytes and disturb none"
                );
            }
        }
    }

    /// A mutation has to change something. One that returns the input unchanged costs a whole run
    /// to learn nothing.
    #[test]
    fn a_mutation_changes_the_input() {
        let mut unchanged = 0;
        for seed in 0..200u64 {
            let original: Vec<u8> = (0..64).map(|i| (i * 5 % 253) as u8).collect();
            let donor: Vec<u8> = vec![0xAB; 64];
            let mut prefix = original.clone();
            mutate(&mut prefix, Context { donor: Some(&donor), journal: None }, &mut seeded(seed));
            if prefix == original {
                unchanged += 1;
            }
        }
        // Not zero: a splice can copy bytes that were already equal, and an arithmetic step can
        // land back where it started. Rare is the requirement; never is not achievable without
        // making the operators dishonest about what they do.
        assert!(unchanged < 20, "{unchanged} of 200 mutations changed nothing");
    }

    /// Same entry, same seed, same mutation — or a campaign cannot be repeated.
    #[test]
    fn one_seed_is_one_mutation() {
        let original: Vec<u8> = (0..96).map(|i| (i * 3 % 249) as u8).collect();
        for seed in 0..32u64 {
            let mut first = original.clone();
            let mut again = original.clone();
            let op_a = mutate(&mut first, Context::default(), &mut seeded(seed));
            let op_b = mutate(&mut again, Context::default(), &mut seeded(seed));
            assert_eq!(op_a, op_b);
            assert_eq!(first, again, "seed {seed} produced two different children");
        }
    }

    #[test]
    fn an_empty_prefix_is_grown_rather_than_refused() {
        let mut prefix = Vec::new();
        let op = mutate(&mut prefix, Context::default(), &mut seeded(1));
        assert_eq!(op, Operator::Extend);
        assert!(!prefix.is_empty(), "a search that cannot start has nowhere to go");
    }

    #[test]
    fn a_prefix_never_grows_past_the_bound() {
        let mut prefix = vec![0u8; MAX_PREFIX_BYTES - 4];
        for seed in 0..40u64 {
            mutate(&mut prefix, Context::default(), &mut seeded(seed));
            assert!(prefix.len() <= MAX_PREFIX_BYTES, "the corpus grew past what it may hold");
        }
    }
}
