//! The two passes that work on the tape, and what "plainest" means for each kind of decision.
//!
//! Both are the same shape: a set of decisions the run made, where keeping one means leaving its
//! bytes alone and removing it means overwriting them with the bytes that make the decision go the
//! dull way. They differ only in which decisions they are allowed to touch and what dull means.
//!
//! # Why this is a tape edit and not an edit to a list of events
//!
//! There is no stored list of deliveries to strike a line out of. A delivery happened because two
//! bytes on the tape said so, and the event log is a record of that having happened. So "do not
//! deliver this message" is written by putting different bytes at the offset the decision was
//! drawn from — which the draw ABI makes safe, because what a draw consumes is a function of the
//! request and never of the value, so one rewritten decision leaves every later one reading from
//! the same place.
//!
//! # Why offsets are resolved again after every success
//!
//! A decision that goes differently changes what happens next: an undelivered message arms no
//! timer, wakes no receive, and the syscalls that would have followed are not made. The run
//! therefore makes a different *number* of draws, and every offset after the change belongs to
//! something else. The pass consequently addresses decisions by their position among the sites it
//! cares about in the journal of the run that is currently best, and re-reads that journal each
//! time one is accepted.

use bohr_tape::{DrawSite, Journal};

use crate::scenario::Edit;

/// Which decisions a pass may rewrite.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Target {
    /// Deliveries: whether a packet reaches the other side at all.
    Deliveries,
    /// Everything a run decided that a mutation can reach.
    Everything,
}

/// The decisions of interest in a journal, in the order they were drawn.
///
/// Only those a rewrite can reach: a draw that consumed nothing decided something structural
/// rather than something drawn, and a draw past the end of the prefix took its bytes from the
/// keystream, where there is nothing to write.
#[must_use]
pub(crate) fn decisions(journal: &Journal, prefix_len: usize, target: Target) -> Vec<Decision> {
    let mut found = Vec::new();
    let mut deliveries = 0usize;
    for record in journal.records() {
        let reachable =
            record.len > 0 && record.offset + u64::from(record.len) <= prefix_len as u64;
        if !reachable {
            continue;
        }
        let site = DrawSite::from_code(record.site);
        let is_delivery = site == Some(DrawSite::NetDeliver);
        if is_delivery {
            deliveries += 1;
        }
        let wanted = match target {
            // Two draws are made per send — whether to drop, then whether to duplicate — in that
            // order. Only the first decides whether the message arrives at all.
            Target::Deliveries => is_delivery && deliveries % 2 == 1,
            Target::Everything => site.is_some(),
        };
        if wanted && let Some(site) = site {
            found.push(Decision { site, offset: record.offset, len: record.len });
        }
    }
    found
}

/// One decision a pass can rewrite.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Decision {
    /// What kind of decision it was.
    pub(crate) site: DrawSite,
    /// Where its bytes are.
    pub(crate) offset: u64,
    /// How many.
    pub(crate) len: u32,
}

impl Decision {
    /// Whether this decision already reads the way a pass would rewrite it.
    ///
    /// Asked so a probe is never spent proposing a scenario identical to the one it came from.
    #[must_use]
    pub(crate) fn is_already_dull(&self, prefix: &[u8], target: Target) -> bool {
        let at = self.offset as usize;
        let end = at + self.len as usize;
        prefix.get(at..end).is_some_and(|bytes| bytes == self.dull(target).as_slice())
    }

    /// The edit that makes this decision go the dull way.
    #[must_use]
    pub(crate) fn edit(&self, target: Target) -> Edit {
        Edit { at: self.offset, bytes: self.dull(target) }
    }

    /// The bytes that make it dull.
    ///
    /// Dull is not always zero. A boolean draw fires when the drawn value is *below* a threshold,
    /// so all-zero bytes make a fault certain rather than impossible — which is the opposite of a
    /// reduction for everything except a delivery, where "the packet did not arrive" is the thing
    /// being kept. All-ones make a delay the longest rather than the shortest. So each kind of
    /// decision says for itself which end of its range is the quiet one.
    fn dull(&self, target: Target) -> Vec<u8> {
        let quiet = match (target, self.site) {
            // Zero is below every threshold, so the packet is dropped: what the event pass keeps.
            (Target::Deliveries, _) => 0x00,
            // A fault, a duplication, a reordering: all-ones is above every threshold a
            // probability can produce, so it does not happen.
            (
                _,
                DrawSite::NetDeliver
                | DrawSite::NetReorder
                | DrawSite::NetPartition
                | DrawSite::DiskError
                | DrawSite::DiskTear
                | DrawSite::DiskReorder
                | DrawSite::NodeFault
                | DrawSite::SdkFaultPoint,
            ) => 0xFF,
            // A delay, a priority, a choice among waiting tasks, bytes a guest was handed: zero is
            // the shortest, the first and the plainest.
            _ => 0x00,
        };
        vec![quiet; self.len as usize]
    }
}

#[cfg(test)]
mod tests {
    use bohr_types::{EventId, TapeOffset};

    use super::*;

    fn journal_of(draws: &[(DrawSite, u64, u32)]) -> Journal {
        let mut journal = Journal::new();
        for (site, offset, len) in draws {
            journal.push(TapeOffset(*offset), EventId(1), *site, *len, 0);
        }
        journal
    }

    /// Two draws are made per send and only the first decides whether the message arrives. Aiming
    /// at the second would rewrite whether it was duplicated, which is not the same question.
    #[test]
    fn a_delivery_pass_aims_at_the_first_draw_of_each_send() {
        let journal = journal_of(&[
            (DrawSite::NetDeliver, 0, 2),
            (DrawSite::NetDeliver, 2, 2),
            (DrawSite::NetDelay, 4, 4),
            (DrawSite::NetDeliver, 8, 2),
            (DrawSite::NetDeliver, 10, 2),
        ]);
        let found = decisions(&journal, 64, Target::Deliveries);
        assert_eq!(found.iter().map(|d| d.offset).collect::<Vec<_>>(), vec![0, 8]);
    }

    /// A draw that consumed nothing decided something structural, and one past the prefix took its
    /// bytes from the keystream. Neither can be rewritten, so neither is offered.
    #[test]
    fn a_decision_a_rewrite_cannot_reach_is_not_offered() {
        let journal = journal_of(&[
            (DrawSite::SchedPick, 0, 0),
            (DrawSite::NetDelay, 8, 4),
            (DrawSite::NetDelay, 60, 4),
        ]);
        let found = decisions(&journal, 16, Target::Everything);
        assert_eq!(found.iter().map(|d| d.offset).collect::<Vec<_>>(), vec![8]);
    }

    /// A fault fires when what it drew is below a threshold, so zeroing its bytes makes it certain.
    /// A reduction that wrote zeroes everywhere would inject faults rather than remove them.
    #[test]
    fn quiet_is_not_the_same_end_of_the_range_for_every_decision() {
        let fault = Decision { site: DrawSite::NodeFault, offset: 0, len: 2 };
        let delay = Decision { site: DrawSite::NetDelay, offset: 0, len: 4 };
        assert_eq!(fault.edit(Target::Everything).bytes, vec![0xFF, 0xFF]);
        assert_eq!(delay.edit(Target::Everything).bytes, vec![0, 0, 0, 0]);

        // For a delivery, quiet means the packet did not arrive.
        let delivery = Decision { site: DrawSite::NetDeliver, offset: 0, len: 2 };
        assert_eq!(delivery.edit(Target::Deliveries).bytes, vec![0, 0]);
    }

    #[test]
    fn a_decision_already_written_the_dull_way_is_known_to_be() {
        let delay = Decision { site: DrawSite::NetDelay, offset: 4, len: 4 };
        let prefix = vec![0xAA, 0xAA, 0xAA, 0xAA, 0, 0, 0, 0];
        assert!(delay.is_already_dull(&prefix, Target::Everything));

        let loud = Decision { site: DrawSite::NetDelay, offset: 0, len: 4 };
        assert!(!loud.is_already_dull(&prefix, Target::Everything));
    }
}
