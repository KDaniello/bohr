//! What the guest promised, counted across a whole campaign.
//!
//! One run answers `always`. `sometimes` and `reachable` ask whether something ever happened, so
//! they are answered by the campaign and map what was never tested.
//!
//! # Merging
//!
//! Tallies add: evaluations and satisfactions are summed, and the first satisfying run is the
//! smallest digest. The result is independent of order, so resumed, merged and parallel campaigns
//! agree.
//!
//! Neither outcome of a `sometimes` produces a finding, so the tally cannot be rebuilt from
//! findings. See `docs/ARCHITECTURE.md` §14, question 5.

use bohr_types::Digest;

/// What a campaign learned about one property.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Tally {
    /// Times the guest reached the line and evaluated it.
    pub evaluations: u64,
    /// Times it was true.
    pub satisfied: u64,
    /// The satisfying run with the smallest digest, which workers agree on.
    pub first_satisfied: Option<Digest>,
}

impl Tally {
    /// Folds another account of the same property in.
    ///
    /// Commutative and associative, so merge order never matters.
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
/// The guest chooses names, and one named after a tick makes a name per evaluation. The bound
/// keeps supervisor memory flat (§4.4) and is far above any deliberate set.
pub const MOST_PROPERTIES: usize = 4096;

/// Every property a campaign has seen, by the name the guest gave it.
///
/// Ordered by name, so reports compare line by line.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Properties {
    pub(crate) by_name: std::collections::BTreeMap<String, Tally>,
    /// Evaluations of names that arrived after the bound was reached.
    ///
    /// Counted and reported, so a full map is visible.
    pub(crate) refused: u64,
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
        self.observe_tally(name, 1, u64::from(held), run);
    }

    /// Records one run's evaluations of one property at once: `evaluations` of them, `held` true.
    ///
    /// The same as that many calls to [`Self::observe`], in any order.
    pub fn observe_tally(&mut self, name: &str, evaluations: u64, held: u64, run: Digest) {
        if self.by_name.len() >= MOST_PROPERTIES && !self.by_name.contains_key(name) {
            self.refused += evaluations;
            return;
        }
        let tally = self.by_name.entry(name.to_owned()).or_default();
        tally.evaluations += evaluations;
        if held > 0 {
            tally.satisfied += held;
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
    /// Zero unless the guest generates names.
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
    /// The map of the untested: claims the search never brought about.
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

    /// The order results arrive in does not matter.
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

    /// The first satisfying run is chosen by digest.
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

    /// New names past the bound are counted and refused.
    #[test]
    fn a_guest_that_numbers_its_promises_cannot_grow_the_map_without_end() {
        let mut properties = Properties::new();
        for at in 0..(MOST_PROPERTIES + 100) {
            properties.observe(&format!("a leader was elected at tick {at}"), true, run(1));
        }
        assert_eq!(properties.tallies().len(), MOST_PROPERTIES, "the bound was not held");
        assert_eq!(properties.refused(), 100, "what was refused was not counted");

        // A name already held is still counted.
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

    /// A run's tally of one property is the same as its evaluations one by one, the bound on
    /// names included.
    #[test]
    fn a_tally_is_its_evaluations_at_once() {
        let mut one_by_one = Properties::new();
        let mut at_once = Properties::new();
        for held in [true, false, false, true, false] {
            one_by_one.observe("p", held, run(3));
        }
        at_once.observe_tally("p", 5, 2, run(3));
        assert_eq!(one_by_one, at_once);
    }
}
