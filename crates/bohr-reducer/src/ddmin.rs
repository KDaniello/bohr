//! Delta debugging over a list of elements, as something that yields one probe at a time.
//!
//! Zeller and Hildebrandt's ddmin, with one structural difference: the published algorithm is
//! written as a loop that calls a test function, and this one cannot — running a candidate here
//! means running a guest, which is the thing this crate is built not to do. So the loop is turned
//! inside out into a state machine that answers "try this next" and is told what happened.
//!
//! The algorithm itself is unchanged. At granularity `n` the elements are cut into `n` parts;
//! every part is tried on its own, and if none of them still reproduces, every *complement* is
//! tried. Whatever reproduces becomes the new set, at a granularity that keeps the parts the same
//! size where it can. When `n` reaches the number of elements left and nothing worked, the set is
//! one-minimal: no single element can be removed.

/// A minimisation in progress over a list of positions.
///
/// Positions rather than elements: what is being minimised may be entries in a document, draws on
/// a tape or messages on a link, and the algorithm has no business knowing which.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DdMin {
    kept: Vec<usize>,
    granularity: usize,
    /// Subsets still to try at this granularity, each as the positions it *keeps*.
    queue: Vec<Vec<usize>>,
    /// Whether the queue holds complements. Parts are tried first; complements only if no part on
    /// its own reproduced.
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
            // **One element still has a subset worth trying: the empty one.** Finishing at a count
            // of one left a single entry in every reduced scenario whether or not the finding
            // needed it, which is a set that is not one-minimal -- the property the module opens
            // by promising. Nought elements is the only set with nothing left to ask about.
            done: count == 0,
        };
        ddmin.refill();
        ddmin
    }

    /// The next subset to try, or nothing where the set is one-minimal.
    ///
    /// A pure function of the state: asking twice without answering gives the same subset, which
    /// is what a candidate whose run was lost is reissued from.
    #[must_use]
    pub fn next(&self) -> Option<&[usize]> {
        self.outstanding.as_deref()
    }

    /// Folds in what happened to the subset [`Self::next`] proposed.
    ///
    /// # Panics
    ///
    /// Panics if nothing was proposed. An answer to a question nobody asked would minimise
    /// towards a set that was never tried.
    pub fn accept(&mut self, reproduced: bool) {
        let tried = self.outstanding.take().expect("an answer arrived for no proposal");
        if reproduced {
            // The granularity drops back to two on a part and stays as fine as it can on a
            // complement: removing one part leaves the rest cut the same way, and starting over
            // coarsely would re-try what has already failed.
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
        // A set of one has no finer granularity to reach, so its complement round is where the
        // work ends rather than a step towards a wider one.
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
                // The empty complement is kept only where it is the one question left. For a
                // larger set, removing everything at once is a subset successive halving reaches
                // on its own; for a set of one, it is the only thing still worth asking, and
                // dropping it left a single element in every result whether or not it was needed.
                .filter(|complement: &Vec<usize>| !complement.is_empty() || self.kept.len() == 1)
                .collect()
        } else {
            // At granularity two a part and a complement are the same two subsets, so trying the
            // parts alone is the whole of that round.
            parts.into_iter().filter(|part| !part.is_empty()).collect()
        };
        self.queue.reverse();
        self.take_next();
    }

    /// Takes the next subset off the queue, advancing until there is one or the work is over.
    fn take_next(&mut self) {
        while self.outstanding.is_none() {
            if let Some(subset) = self.queue.pop() {
                // A subset that changes nothing is not a question: it would spend a replay to be
                // told what is already known.
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

    /// The case the algorithm exists for: one element is to blame and the rest are noise.
    #[test]
    fn one_guilty_element_is_found_among_many() {
        let (kept, probes) = minimise(64, |subset| subset.contains(&37));
        assert_eq!(kept, vec![37]);
        assert!(probes < 64, "{probes} probes for what a linear scan does in 64");
    }

    /// Two elements that only reproduce together, which is what makes this delta debugging rather
    /// than a scan: neither can be removed, and neither alone is the answer.
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

    /// Asking twice without answering asks the same thing, which is what a lost run is reissued
    /// from.
    #[test]
    fn proposing_twice_without_answering_proposes_the_same_subset() {
        let ddmin = DdMin::new(16);
        assert_eq!(ddmin.next(), ddmin.next());
    }

    /// A minimisation never proposes the set it already holds: a probe that changes nothing spends
    /// a replay to learn what is known.
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

    /// A set of one has exactly one question left: is the one needed at all?
    ///
    /// A scenario carrying a single entry keeps it only if the finding needs it; anything else is
    /// not one-minimal, in the module's own words.
    #[test]
    fn a_set_of_one_is_asked_whether_it_is_needed() {
        let mut ddmin = DdMin::new(1);
        assert!(!ddmin.is_done(), "the one question was never asked");
        assert_eq!(ddmin.next(), Some(&[][..]), "the question is whether nothing reproduces");

        // Answered no: the element is needed, and there is nothing further to ask.
        ddmin.accept(false);
        assert!(ddmin.is_done());
        assert_eq!(ddmin.kept(), &[0]);
    }

    /// And answered yes, the element goes and the set is empty.
    #[test]
    fn a_lone_element_that_is_not_needed_is_dropped() {
        let mut ddmin = DdMin::new(1);
        assert_eq!(ddmin.next(), Some(&[][..]));
        ddmin.accept(true);
        assert!(ddmin.is_done());
        assert!(ddmin.kept().is_empty(), "the element nothing needed was kept");
    }

    /// Nothing at all is nothing to ask about, which is the one set that starts finished.
    #[test]
    fn an_empty_set_has_nothing_to_minimise() {
        let ddmin = DdMin::new(0);
        assert!(ddmin.is_done());
        assert!(ddmin.next().is_none());
        assert!(ddmin.kept().is_empty());
    }
}
