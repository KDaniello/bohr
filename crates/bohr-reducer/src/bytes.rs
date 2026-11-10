//! The passes that work on the tape, and what "plainest" means for each kind of decision.
//!
//! All are the same shape: a set of decisions the run made, where keeping one means leaving its
//! bytes alone and removing it means overwriting them with the bytes that make the decision go the
//! dull way. They differ only in which decisions they are allowed to touch and what dull means.
//! The fault pass has one more move, [`sooner`]: the same fault, fired at an earlier chance.
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

use bohr_tape::{DrawRecord, DrawSite, Journal};

use crate::scenario::Edit;

/// Which decisions a pass may rewrite.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Target {
    /// Faults the run drew and that fired: a kill, a dropped packet, an I/O error.
    Faults,
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
    for (index, record) in journal.records().iter().enumerate() {
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
            Target::Faults => {
                site.is_some_and(|site| is_odds(site, record.len)) && journal.fired(index)
            }
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

/// Whether a draw is a fixed-width odds draw: a fault that either happens here or does not.
///
/// Read off the site and the width, because a record does not say which primitive drew it. At
/// these sites every two-byte draw is one: a node fault's kind is narrower, a stall's length wider.
fn is_odds(site: DrawSite, len: u32) -> bool {
    len == 2
        && matches!(
            site,
            DrawSite::NetDeliver
                | DrawSite::NetReorder
                | DrawSite::NetCorrupt
                | DrawSite::DiskLatency
                | DrawSite::DiskError
                | DrawSite::NodeFault
                | DrawSite::MemoryExhausted
        )
}

/// Where a fired node fault could have fired instead, earliest first.
///
/// Only node faults are moved: a kill decides how long everything after it takes, so firing it at
/// an earlier chance is what shortens a run, while an earlier dropped packet is a different packet.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Movable {
    /// The node-fault odds draws that did not fire before this one, after the fault before it.
    pub(crate) chances: Vec<u64>,
    /// The bytes the fault drew after deciding to fire — which kind, how bad — at its own event.
    pub(crate) body: Vec<u8>,
}

/// Whether the record at `index` is a node fault's odds draw.
fn node_odds(journal: &Journal, index: usize) -> bool {
    journal.records().get(index).is_some_and(|record| {
        DrawSite::from_code(record.site) == Some(DrawSite::NodeFault)
            && is_odds(DrawSite::NodeFault, record.len)
    })
}

/// How many node faults fired in a run.
#[must_use]
pub(crate) fn fired_node_faults(journal: &Journal) -> usize {
    (0..journal.len()).filter(|&index| node_odds(journal, index) && journal.fired(index)).count()
}

/// Whether a node fault fired at this offset of the run that produced `journal`.
#[must_use]
pub(crate) fn fired_at(journal: &Journal, offset: u64) -> bool {
    journal.records().iter().enumerate().any(|(index, record)| {
        record.offset == offset && node_odds(journal, index) && journal.fired(index)
    })
}

/// The `nth` fired node fault of a journal, and the chances it had before it.
///
/// The chances stop at the fault before it, so a move keeps the order the faults fired in and the
/// run up to the new chance is the run that was.
#[must_use]
pub(crate) fn movable(journal: &Journal, prefix: &[u8], nth: usize) -> Option<Movable> {
    let reachable = |record: &DrawRecord| {
        record.len > 0 && record.offset + u64::from(record.len) <= prefix.len() as u64
    };
    let records = journal.records();
    let mut chances = Vec::new();
    let mut seen = 0usize;
    for (at, record) in records.iter().enumerate() {
        if !node_odds(journal, at) {
            continue;
        }
        if !journal.fired(at) {
            if reachable(record) {
                chances.push(record.offset);
            }
            continue;
        }
        if seen < nth {
            seen += 1;
            chances.clear();
            continue;
        }
        if !reachable(record) {
            return None;
        }
        // What the fault drew next to say what it is: the kind, and for a degradation or a clock
        // fault how much. Nothing else at its event belongs to it — a fault that leaves the call
        // standing goes on to draw the call's own decisions — so the body stops at another site.
        // A one-option kind draws nothing and is passed over.
        let mut body = Vec::new();
        let mut end = record.offset + u64::from(record.len);
        for next in &records[at + 1..] {
            let own = matches!(
                DrawSite::from_code(next.site),
                Some(DrawSite::NodeFault | DrawSite::NetPartition | DrawSite::ClockSkew)
            );
            if next.event != record.event || !own || is_odds(DrawSite::NodeFault, next.len) {
                break;
            }
            if next.len == 0 {
                continue;
            }
            if next.offset != end || !reachable(next) {
                break;
            }
            let from = next.offset as usize;
            body.extend_from_slice(&prefix[from..from + next.len as usize]);
            end += u64::from(next.len);
        }
        return Some(Movable { chances, body });
    }
    None
}

/// The edits that fire a node fault at the chance at `offset`, as what it fired as before.
///
/// Zero is below every threshold a non-zero probability makes, so the draw fires; the body follows
/// it because a fired fault reads its kind from the bytes straight after. What the original fault
/// drew is left alone: the run has already gone another way by then, and those bytes belong to
/// whatever reads them now.
#[must_use]
pub(crate) fn sooner(offset: u64, body: &[u8]) -> Vec<Edit> {
    let mut bytes = vec![0u8, 0u8];
    bytes.extend_from_slice(body);
    vec![Edit { at: offset, bytes }]
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
            // Any odds draw — a kill, a stall, a refused allocation — is quiet at all-ones, which
            // is above every threshold a probability can produce. Decided by width as well as
            // site, because a stall's *length* is drawn at the same site and is quiet at zero.
            (_, site) if is_odds(site, self.len) => 0xFF,
            // Bytes that decide a fault by comparison rather than by odds: all-ones loses it too.
            (
                _,
                DrawSite::NetPartition
                | DrawSite::DiskTear
                | DrawSite::DiskReorder
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

    /// Draws as `(site, offset, len, event, fired)`; a two-byte draw at an odds site is an odds
    /// draw, journalled as the world journals one.
    fn journal_with_values(draws: &[(DrawSite, u64, u32, u64, u64)]) -> Journal {
        let mut journal = Journal::new();
        for (site, offset, len, event, fired) in draws {
            if is_odds(*site, *len) {
                journal.push_odds(TapeOffset(*offset), EventId(*event), *site, 0x4242, *fired == 1);
            } else {
                journal.push(TapeOffset(*offset), EventId(*event), *site, *len, *fired);
            }
        }
        journal
    }

    /// The fault pass sees the faults that fired and nothing else: an odds draw journals whether it
    /// fired, and a draw at a fault site of another width decided something else.
    #[test]
    fn the_fault_pass_aims_at_faults_that_fired() {
        let journal = journal_with_values(&[
            (DrawSite::NodeFault, 0, 2, 1, 0),
            (DrawSite::NodeFault, 2, 2, 2, 1),
            (DrawSite::NodeFault, 4, 1, 2, 1),
            (DrawSite::DiskLatency, 5, 4, 3, 1),
            (DrawSite::DiskError, 9, 2, 4, 1),
        ]);
        let found = decisions(&journal, 64, Target::Faults);
        assert_eq!(found.iter().map(|d| d.offset).collect::<Vec<_>>(), vec![2, 9]);
        for decision in &found {
            assert_eq!(decision.edit(Target::Faults).bytes, vec![0xFF, 0xFF]);
        }
    }

    /// A stall's odds and its length are drawn at one site. All-ones makes the first not happen
    /// and zero makes the second the shortest; one rule for the site would get one of them wrong.
    #[test]
    fn one_site_can_hold_an_odds_draw_and_a_length() {
        let odds = Decision { site: DrawSite::DiskLatency, offset: 0, len: 2 };
        let length = Decision { site: DrawSite::DiskLatency, offset: 2, len: 4 };
        assert_eq!(odds.edit(Target::Everything).bytes, vec![0xFF, 0xFF]);
        assert_eq!(length.edit(Target::Everything).bytes, vec![0, 0, 0, 0]);
        let refused = Decision { site: DrawSite::MemoryExhausted, offset: 0, len: 2 };
        assert_eq!(refused.edit(Target::Everything).bytes, vec![0xFF, 0xFF]);
    }

    /// A node fault's earlier chances stop at the fault before it, and what it drew next at its own
    /// event travels with it so it fires as the same kind.
    #[test]
    fn a_node_fault_moves_with_its_kind_and_never_past_the_fault_before_it() {
        let journal = journal_with_values(&[
            (DrawSite::NodeFault, 0, 2, 1, 0),
            (DrawSite::NodeFault, 2, 2, 2, 1),
            (DrawSite::NodeFault, 4, 1, 2, 3),
            (DrawSite::NodeFault, 5, 2, 3, 0),
            (DrawSite::NetDelay, 7, 4, 3, 0),
            (DrawSite::NodeFault, 11, 2, 4, 0),
            (DrawSite::NodeFault, 13, 2, 5, 1),
            (DrawSite::NodeFault, 15, 1, 5, 2),
        ]);
        let prefix: Vec<u8> = (0..32).collect();
        let first = movable(&journal, &prefix, 0).expect("a first fault");
        assert_eq!(first.chances, vec![0]);
        assert_eq!(first.body, vec![4]);
        let second = movable(&journal, &prefix, 1).expect("a second fault");
        assert_eq!(second.chances, vec![5, 11], "a chance before the first fault was offered");
        assert_eq!(second.body, vec![15]);
        assert!(movable(&journal, &prefix, 2).is_none());

        assert_eq!(sooner(5, &second.body), vec![Edit { at: 5, bytes: vec![0, 0, 15] }]);
        assert!(fired_at(&journal, 13) && !fired_at(&journal, 11));
    }

    /// A fault's body is what says what it is, and nothing the call went on to draw: a kind of one
    /// option is passed over, a degradation's severity is kept, the call's own draws are not.
    #[test]
    fn a_fault_carries_what_it_is_and_not_what_the_call_drew_next() {
        let journal = journal_with_values(&[
            (DrawSite::NodeFault, 0, 2, 1, 1),
            (DrawSite::NodeFault, 2, 0, 1, 0),
            (DrawSite::NetPartition, 2, 1, 1, 0),
            (DrawSite::MemoryExhausted, 3, 2, 1, 0),
            (DrawSite::NetDeliver, 5, 2, 1, 0),
        ]);
        let prefix: Vec<u8> = (0..16).collect();
        let fault = movable(&journal, &prefix, 0).expect("a fault");
        assert_eq!(fault.body, vec![2], "the body took the call's own draws or lost the severity");
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
