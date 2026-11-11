//! R7: a refused resource refuses the operation, never the data, and the node comes back with the
//! resource.
//!
//! The world tells the watch each disk write, `fsync` or allocation it refused a node. A loss R1.1
//! proves, of an operation that saw such a refusal between its begin and its acknowledgement, read
//! in the start that acknowledged it, is R7.1: with no crash between, the refused call is what took
//! the data. A loss across a crash has other causes, and stays R1.1's. A node that comes back from
//! a start the resource refused and does not answer is R7.2; a start refused itself owes nothing,
//! since stopping is a correct answer to a full disk. Neither law claims the refused call was on
//! the file the operation wrote: a store's records name keys, not descriptors.

use std::collections::{BTreeMap, BTreeSet};

use super::Broken;

/// What was refused to whom, and which operations saw it.
#[derive(Debug, Default)]
pub(crate) struct Refusals {
    /// Each node's refusals so far, and what the latest was.
    seen: BTreeMap<u32, (u64, &'static str)>,
    /// Each node's subjects' operations begun and not acknowledged: the operation's sequence and
    /// the refusal count when it first began.
    began_at: BTreeMap<(u32, String), (u64, u64)>,
    /// Each node's refusal count at its latest acknowledgement, for an operation with no begin.
    acked_at: BTreeMap<u32, u64>,
    /// Subjects whose latest acknowledgement came after a refusal in its operation: the refusal,
    /// and the start that acknowledged it.
    after_refusal: BTreeMap<String, (&'static str, u64)>,
    /// The starts that saw a refusal.
    starts: BTreeSet<(u32, u64)>,
}

impl Refusals {
    /// The world refused `node`, in `start`, `what`: a write, an `fsync` or an allocation.
    pub(crate) fn refused(&mut self, node: u32, start: u64, what: &'static str) {
        let entry = self.seen.entry(node).or_insert((0, what));
        *entry = (entry.0 + 1, what);
        self.starts.insert((node, start));
    }

    /// Operation `seq` on `subject` began on `node`; begun again under the same sequence, a retry,
    /// it counts from the first begin, and under another, from this one.
    pub(crate) fn begun(&mut self, node: u32, subject: &str, seq: u64) {
        let count = self.seen.get(&node).map_or(0, |(n, _)| *n);
        let mark = self.began_at.entry((node, subject.to_owned())).or_insert((seq, count));
        if mark.0 != seq {
            *mark = (seq, count);
        }
    }

    /// `node` started again: what it began before is not this start's to acknowledge.
    pub(crate) fn restarted(&mut self, node: u32) {
        self.began_at.retain(|(n, _), _| *n != node);
        self.acked_at.insert(node, self.seen.get(&node).map_or(0, |(n, _)| *n));
    }

    /// An operation on `subject` was acknowledged by `node` in `start`: whether a refusal came
    /// between.
    pub(crate) fn acked(&mut self, node: u32, start: u64, subject: &str) {
        let (count, what) = self.seen.get(&node).copied().unwrap_or((0, ""));
        let mark = self
            .began_at
            .remove(&(node, subject.to_owned()))
            .map(|(_, count)| count)
            .unwrap_or_else(|| self.acked_at.get(&node).copied().unwrap_or(0));
        if count > mark {
            self.after_refusal.insert(subject.to_owned(), (what, start));
        } else {
            self.after_refusal.remove(subject);
        }
    }

    /// The acknowledgements of one node are all in: the next one with no begin counts from here.
    pub(crate) fn acked_through(&mut self, node: u32) {
        let count = self.seen.get(&node).map_or(0, |(n, _)| *n);
        self.acked_at.insert(node, count);
    }

    /// R7.1 for a read of `subject` in `start` that broke R1.1, where its acknowledgement came
    /// after a refusal in the same start.
    pub(crate) fn explain(&self, subject: &str, start: u64, broken: &[Broken]) -> Option<Broken> {
        let (what, acked_in) = self.after_refusal.get(subject)?;
        if *acked_in != start {
            return None;
        }
        let lost = broken.iter().find(|b| b.code == bohr_trace::ACKED_SURVIVES_CODE)?;
        Some(Broken {
            code: bohr_trace::FAILED_ACKED_CODE,
            node: lost.node,
            message: format!(
                "R7.1: {subject} was acknowledged by node {} after {what} between the \
                 operation's begin and its acknowledgement, and lost: {}",
                lost.node, lost.message
            ),
        })
    }

    /// Whether `start` of `node` saw a refusal.
    pub(crate) fn refused_in(&self, node: u32, start: u64) -> bool {
        self.starts.contains(&(node, start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lost(subject: &str) -> Broken {
        Broken {
            code: bohr_trace::ACKED_SURVIVES_CODE,
            node: 0,
            message: format!("R1.1: {subject} read back absent"),
        }
    }

    #[test]
    fn a_loss_after_a_refusal_in_its_operation_is_r7_1_and_one_before_it_is_not() {
        let mut refusals = Refusals::default();
        refusals.begun(0, "a", 1);
        refusals.refused(0, 1, "an fsync failed with EIO");
        // Begun again by a retry: still counted from the first begin.
        refusals.begun(0, "a", 1);
        refusals.acked(0, 1, "a");
        refusals.acked_through(0);
        refusals.begun(0, "b", 2);
        refusals.acked(0, 1, "b");
        let r71 = refusals.explain("a", 1, &[lost("a")]).expect("a saw the refusal");
        assert_eq!(r71.code, bohr_trace::FAILED_ACKED_CODE);
        assert!(r71.message.contains("an fsync failed with EIO"), "{}", r71.message);
        assert!(r71.message.contains("R1.1: a"), "{}", r71.message);
        assert!(refusals.explain("b", 1, &[lost("b")]).is_none());
        // No loss, no R7.1: a retry that rewrote the data is a correct answer.
        assert!(refusals.explain("a", 1, &[]).is_none());
        // Lost across a crash, the loss has other causes.
        assert!(refusals.explain("a", 2, &[lost("a")]).is_none());
        assert!(refusals.refused_in(0, 1) && !refusals.refused_in(0, 2));
    }

    #[test]
    fn a_refusal_of_an_operation_given_up_is_not_another_operation_s() {
        let mut refusals = Refusals::default();
        refusals.begun(0, "a", 1);
        refusals.refused(0, 1, "a write failed with ENOSPC");
        // Given up; a new operation on the same key, acknowledged with nothing refused.
        refusals.begun(0, "a", 2);
        refusals.acked(0, 1, "a");
        assert!(refusals.explain("a", 1, &[lost("a")]).is_none());
        // Begun before a restart and acknowledged after it: counted from the restart.
        refusals.begun(0, "b", 3);
        refusals.refused(0, 1, "a write failed with ENOSPC");
        refusals.restarted(0);
        refusals.begun(0, "b", 3);
        refusals.acked(0, 2, "b");
        assert!(refusals.explain("b", 2, &[lost("b")]).is_none());
    }

    #[test]
    fn an_acknowledgement_with_no_begin_counts_from_the_one_before() {
        let mut refusals = Refusals::default();
        refusals.acked(0, 1, "a");
        refusals.acked_through(0);
        refusals.refused(0, 1, "a write failed with ENOSPC");
        refusals.acked(0, 1, "b");
        assert!(refusals.explain("a", 1, &[lost("a")]).is_none());
        assert!(refusals.explain("b", 1, &[lost("b")]).is_some());
    }
}
