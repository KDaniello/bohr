//! What the guest promised, counted across a whole campaign.
//!
//! `always` is answered by one run: it either held or it did not. `sometimes` and `reachable` are
//! questions about a campaign — "did this *ever* happen" — and one run cannot answer them. They do
//! not find crashes. They produce the map of what was never tested, which is the half of the SDK
//! teams underestimate: "across ten million runs we never once reached a leader change with a full
//! disk" is a sentence no crash report contains.
//!
//! # Why the counter adds
//!
//! Campaigns are resumed, merged, and run on several workers at once, so the counter has to be a
//! thing that can be combined in any order and give one answer. Addition is: per property, the
//! evaluations and the true ones are summed, and the run that first satisfied it is whichever came
//! earlier. Nothing here depends on the order the results were handed over, which is what lets
//! `-j 1` and `-j 8` produce the same map.
//!
//! Recomputing this from the findings was never available, and it is worth saying why: a satisfied
//! `sometimes` produces no finding, and an unsatisfied one produces nothing at all. There would be
//! nothing to recompute from. See `docs/ARCHITECTURE.md` §14, question 5.

use bohr_types::Digest;

/// What a campaign learned about one property.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Tally {
    /// Times the guest reached the line and evaluated it.
    pub evaluations: u64,
    /// Times it was true.
    pub satisfied: u64,
    /// The first run that satisfied it, by digest — the tape to look at when asking how.
    ///
    /// The *first* by digest order rather than by time, because time is not a thing two workers
    /// agree on and a digest is.
    pub first_satisfied: Option<Digest>,
}

impl Tally {
    /// Folds another account of the same property in.
    ///
    /// Commutative and associative, which is the whole requirement: two campaigns merged in either
    /// order, or one campaign's workers finishing in any order, give one answer.
    pub fn merge(&mut self, other: &Self) {
        self.evaluations += other.evaluations;
        self.satisfied += other.satisfied;
        self.first_satisfied = match (self.first_satisfied, other.first_satisfied) {
            (Some(mine), Some(theirs)) => Some(mine.min(theirs)),
            (mine, theirs) => mine.or(theirs),
        };
    }

    /// Whether the campaign ever saw this hold.
    #[must_use]
    pub const fn is_satisfied(&self) -> bool {
        self.satisfied > 0
    }
}

/// Names a campaign will hold before it stops taking new ones.
///
/// **A bound, because the names are the guest's to choose.** A property named after something that
/// changes — a tick, a term, an index — is an ordinary mistake in using the SDK, and it produces
/// one name per evaluation: an overnight campaign of a million runs then holds a million tallies
/// in supervisor memory, which §4.4 says is the one thing that may not be what runs out first.
/// Generous enough that no deliberate set of properties comes near it.
pub const MOST_PROPERTIES: usize = 4096;

/// Every property a campaign has seen, by the name the guest gave it.
///
/// A map ordered by name, so a report reads the same twice and two campaigns can be compared line
/// by line.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Properties {
    by_name: std::collections::BTreeMap<String, Tally>,
    /// Evaluations of names that arrived after the bound was reached.
    ///
    /// Counted and reported rather than dropped in silence: a map of the untested that had quietly
    /// stopped growing would read as a guest with few promises.
    refused: u64,
}

impl Properties {
    /// Nothing seen yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one evaluation of one property.
    ///
    /// A name beyond [`MOST_PROPERTIES`] is counted and refused; see the constant.
    pub fn observe(&mut self, name: &str, held: bool, run: Digest) {
        if self.by_name.len() >= MOST_PROPERTIES && !self.by_name.contains_key(name) {
            self.refused += 1;
            return;
        }
        let tally = self.by_name.entry(name.to_owned()).or_default();
        tally.evaluations += 1;
        if held {
            tally.satisfied += 1;
            tally.first_satisfied = Some(match tally.first_satisfied {
                Some(known) => known.min(run),
                None => run,
            });
        }
    }

    /// Folds another campaign's account in.
    pub fn merge(&mut self, other: &Self) {
        for (name, theirs) in &other.by_name {
            if self.by_name.len() >= MOST_PROPERTIES && !self.by_name.contains_key(name) {
                self.refused += theirs.evaluations;
                continue;
            }
            self.by_name.entry(name.clone()).or_default().merge(theirs);
        }
        self.refused += other.refused;
    }

    /// Evaluations of names this campaign had no room for.
    ///
    /// Nought for every campaign whose guest names its promises rather than numbering them.
    #[must_use]
    pub const fn refused(&self) -> u64 {
        self.refused
    }

    /// What was seen, by name.
    #[must_use]
    pub const fn tallies(&self) -> &std::collections::BTreeMap<String, Tally> {
        &self.by_name
    }

    /// The properties the guest stated and the campaign never satisfied.
    ///
    /// The map of the untested. Every name here is a thing somebody thought was worth claiming and
    /// the search never managed to bring about — which is a gap in the testing, not in the guest,
    /// and is the more useful of the two to be told about.
    #[must_use]
    pub fn unsatisfied(&self) -> Vec<&str> {
        self.by_name
            .iter()
            .filter(|(_, tally)| !tally.is_satisfied())
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Whether anything was seen at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(n: u8) -> Digest {
        Digest::of(&[n])
    }

    #[test]
    fn a_property_never_true_is_the_one_worth_reporting() {
        let mut properties = Properties::new();
        properties.observe("a leader changed", false, run(1));
        properties.observe("a leader changed", false, run(2));
        properties.observe("an entry committed", true, run(1));

        assert_eq!(properties.unsatisfied(), vec!["a leader changed"]);
        assert_eq!(properties.tallies()["a leader changed"].evaluations, 2);
        assert!(properties.tallies()["an entry committed"].is_satisfied());
    }

    /// The requirement the whole design turns on: workers finish in whatever order they like.
    #[test]
    fn merging_gives_one_answer_whatever_order_it_happens_in() {
        let build = |names: &[(&str, bool, u8)]| {
            let mut p = Properties::new();
            for (name, held, r) in names {
                p.observe(name, *held, run(*r));
            }
            p
        };
        let a = build(&[("x", true, 3), ("y", false, 1)]);
        let b = build(&[("x", false, 2), ("z", true, 9)]);

        let mut forwards = a.clone();
        forwards.merge(&b);
        let mut backwards = b.clone();
        backwards.merge(&a);

        assert_eq!(forwards, backwards, "the map depended on which half was folded first");
        assert_eq!(forwards.tallies()["x"].evaluations, 2);
        assert_eq!(forwards.tallies()["x"].satisfied, 1);
        assert_eq!(forwards.unsatisfied(), vec!["y"]);
    }

    /// Which run first satisfied a property must be a fact about the runs, not about the clock:
    /// two workers do not agree on time, and both agree on a digest.
    #[test]
    fn the_first_satisfying_run_is_chosen_by_digest_not_by_arrival() {
        let mut early = Properties::new();
        early.observe("p", true, run(9));
        early.observe("p", true, run(1));

        let mut late = Properties::new();
        late.observe("p", true, run(1));
        late.observe("p", true, run(9));

        assert_eq!(early.tallies()["p"].first_satisfied, late.tallies()["p"].first_satisfied);
    }

    /// The names are the guest's, so their number is too. A property named after a tick produces
    /// one name per evaluation, and an overnight campaign would hold a million tallies.
    #[test]
    fn a_guest_that_numbers_its_promises_cannot_grow_the_map_without_end() {
        let mut properties = Properties::new();
        for at in 0..(MOST_PROPERTIES + 100) {
            properties.observe(&format!("a leader was elected at tick {at}"), true, run(1));
        }
        assert_eq!(properties.tallies().len(), MOST_PROPERTIES, "the bound was not held");
        assert_eq!(properties.refused(), 100, "what was refused was not counted");

        // And a name already held is still counted, however full the map is.
        let held = properties.tallies().keys().next().expect("a name").clone();
        properties.observe(&held, true, run(2));
        assert_eq!(properties.tallies()[&held].evaluations, 2);
    }

    #[test]
    fn a_campaign_that_saw_nothing_claims_nothing() {
        let properties = Properties::new();
        assert!(properties.is_empty());
        assert!(properties.unsatisfied().is_empty());
    }
}
