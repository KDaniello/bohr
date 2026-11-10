//! Delta debugging over a list of elements, as something that yields one probe at a time.
//!
//! Zeller and Hildebrandt's ddmin as a state machine: it proposes a subset and is told the result,
//! since this crate never runs a guest.
//!
//! At granularity `n` the set is cut into `n` parts. Each part is tried, then each complement. What
//! reproduces becomes the new set. When `n` reaches the set size and nothing works, the set is
//! one-minimal.

/// A minimisation in progress over a list of positions.
///
/// Positions, so the same code minimises entries, draws or messages.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DdMin {
    kept: Vec<usize>,
    granularity: usize,
    /// Subsets still to try at this granularity, each as the positions it *keeps*.
    queue: Vec<Vec<usize>>,
    /// Whether the queue holds complements, tried after no part reproduced.
    complements: bool,
    outstanding: Option<Vec<usize>>,
    done: bool,
}

impl DdMin {
    /// A minimisation over `count` elements, all kept.
    #[must_use]
    pub fn new(count: usize) -> Self {
        let mut ddmin = Self {
            kept: (0..count).collect(),
            granularity: 2,
            queue: Vec::new(),
            complements: false,
            outstanding: None,
            // A set of one still tries the empty set; only an empty set starts finished.
            done: count == 0,
        };
        ddmin.refill();
        ddmin
    }

    /// The next subset to try, or nothing where the set is one-minimal.
    ///
    /// A pure function of the state, so a lost run can be reissued.
    #[must_use]
    pub fn next(&self) -> Option<&[usize]> {
        self.outstanding.as_deref()
    }

    /// Folds in what happened to the subset [`Self::next`] proposed.
    ///
    /// # Panics
    ///
    /// Panics if nothing was proposed.
    pub fn accept(&mut self, reproduced: bool) {
        let tried = self.outstanding.take().expect("an answer arrived for no proposal");
        if reproduced {
            // A part resets granularity to two; a complement keeps it as fine as it can.
            self.granularity = if self.complements { (self.granularity - 1).max(2) } else { 2 };
            self.kept = tried;
            self.complements = false;
            self.queue.clear();
            self.done = self.kept.is_empty();
            self.refill();
            return;
        }
        if self.queue.is_empty() {
            self.advance();
        }
        self.take_next();
    }

    /// The smallest set proven to reproduce.
    #[must_use]
    pub fn kept(&self) -> &[usize] {
        &self.kept
    }

    /// Whether there is nothing left to try.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }

    /// Moves to complements, or to a finer granularity, or finishes.
    fn advance(&mut self) {
        // A set of one ends after its complement round.
        if !self.complements && (self.granularity > 2 || self.kept.len() == 1) {
            self.complements = true;
        } else if self.granularity < self.kept.len() {
            self.granularity = (self.granularity * 2).min(self.kept.len());
            self.complements = false;
        } else {
            self.done = true;
            return;
        }
        self.refill();
    }

    /// Builds the subsets to try at the current granularity.
    fn refill(&mut self) {
        if self.done {
            self.outstanding = None;
            return;
        }
        let parts = partition(&self.kept, self.granularity);
        self.queue = if self.complements {
            parts
                .iter()
                .map(|part| self.kept.iter().copied().filter(|at| !part.contains(at)).collect())
                // The empty complement is kept only for a set of one; halving reaches it otherwise.
                .filter(|complement: &Vec<usize>| !complement.is_empty() || self.kept.len() == 1)
                .collect()
        } else {
            // At granularity two the parts are the complements.
            parts.into_iter().filter(|part| !part.is_empty()).collect()
        };
        self.queue.reverse();
        self.take_next();
    }

    /// Takes the next subset off the queue, advancing until there is one or the work is over.
    fn take_next(&mut self) {
        while self.outstanding.is_none() {
            if let Some(subset) = self.queue.pop() {
                // A subset equal to the current set is skipped.
                if subset != self.kept {
                    self.outstanding = Some(subset);
                }
                continue;
            }
            self.advance();
            if self.done {
                self.outstanding = None;
                return;
            }
        }
    }
}

/// Cuts a list into `parts` pieces of as equal a size as the list allows.
fn partition(elements: &[usize], parts: usize) -> Vec<Vec<usize>> {
    if parts == 0 || elements.is_empty() {
        return Vec::new();
    }
    let each = elements.len() / parts;
    let over = elements.len() % parts;
    let mut cut = Vec::with_capacity(parts);
    let mut at = 0;
    for part in 0..parts {
        let size = each + usize::from(part < over);
        cut.push(elements[at..at + size].to_vec());
        at += size;
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a minimisation against a rule about which sets still reproduce, and returns what it
    /// settled on together with how many probes it spent.
    fn minimise(count: usize, reproduces: impl Fn(&[usize]) -> bool) -> (Vec<usize>, usize) {
        let mut ddmin = DdMin::new(count);
        let mut probes = 0;
        while let Some(subset) = ddmin.next() {
            let answer = reproduces(subset);
            probes += 1;
            assert!(probes < 10_000, "a minimisation of {count} elements did not converge");
            ddmin.accept(answer);
        }
        (ddmin.kept().to_vec(), probes)
    }

    /// One element to blame among noise.
    #[test]
    fn one_guilty_element_is_found_among_many() {
        let (kept, probes) = minimise(64, |subset| subset.contains(&37));
        assert_eq!(kept, vec![37]);
        assert!(probes < 64, "{probes} probes for what a linear scan does in 64");
    }

    /// Two elements that reproduce only together are both kept.
    #[test]
    fn a_pair_that_only_fails_together_survives_together() {
        let (kept, _) = minimise(32, |subset| subset.contains(&5) && subset.contains(&29));
        assert_eq!(kept, vec![5, 29]);
    }

    /// Nothing is to blame, so nothing is kept.
    #[test]
    fn a_set_that_always_reproduces_is_emptied() {
        let (kept, _) = minimise(16, |_| true);
        assert!(kept.len() <= 1, "kept {kept:?} of a set nothing depended on");
    }

    /// Everything is to blame, so nothing can be removed.
    #[test]
    fn a_set_that_only_reproduces_whole_is_left_whole() {
        let (kept, _) = minimise(8, |subset| subset.len() == 8);
        assert_eq!(kept.len(), 8);
    }

    /// Asking twice without answering proposes the same subset.
    #[test]
    fn proposing_twice_without_answering_proposes_the_same_subset() {
        let ddmin = DdMin::new(16);
        assert_eq!(ddmin.next(), ddmin.next());
    }

    /// A minimisation never proposes the set it already holds.
    #[test]
    fn nothing_proposed_is_the_set_already_held() {
        let mut ddmin = DdMin::new(9);
        let mut probes = 0;
        while let Some(subset) = ddmin.next() {
            assert_ne!(subset, ddmin.kept(), "the set already held was proposed again");
            probes += 1;
            assert!(probes < 1000);
            let keep = subset.contains(&0);
            ddmin.accept(keep);
        }
    }

    /// A set of one asks once whether its element is needed.
    #[test]
    fn a_set_of_one_is_asked_whether_it_is_needed() {
        let mut ddmin = DdMin::new(1);
        assert!(!ddmin.is_done(), "the one question was never asked");
        assert_eq!(ddmin.next(), Some(&[][..]), "the question is whether nothing reproduces");

        // Answered no: the element is needed.
        ddmin.accept(false);
        assert!(ddmin.is_done());
        assert_eq!(ddmin.kept(), &[0]);
    }

    /// Answered yes, the element goes.
    #[test]
    fn a_lone_element_that_is_not_needed_is_dropped() {
        let mut ddmin = DdMin::new(1);
        assert_eq!(ddmin.next(), Some(&[][..]));
        ddmin.accept(true);
        assert!(ddmin.is_done());
        assert!(ddmin.kept().is_empty(), "the element nothing needed was kept");
    }

    /// An empty set starts finished.
    #[test]
    fn an_empty_set_has_nothing_to_minimise() {
        let ddmin = DdMin::new(0);
        assert!(ddmin.is_done());
        assert!(ddmin.next().is_none());
        assert!(ddmin.kept().is_empty());
    }
}
