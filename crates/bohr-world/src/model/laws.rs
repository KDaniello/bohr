//! The laws of durability and recovery, once, over the family layer's vocabulary.
//!
//! Nothing here knows a chain from a store: an acknowledgement is a subject and a value, a read is
//! a subject, a value and whether the reader owes it. A law that reads a field of one family is an
//! architectural error, and `scripts/lint-rules.sh` refuses this file naming a translator.
//!
//! What each law refuses to claim: a value never acknowledged may be a write in flight at a crash,
//! so it is never a loss; a reader that does not owe an acknowledgement is not asked; an operation
//! begun and not acknowledged excuses what it could have done.

use std::collections::{BTreeMap, BTreeSet};

use super::{Ack, Broken, Read};

/// What one start of one node read of a batch: the subjects read with the batch's values, and those
/// read with older ones.
type BatchRead = (BTreeSet<String>, BTreeSet<String>);

/// What the laws remember: every operation begun or acknowledged on each subject, each batch, and
/// what was already reported.
#[derive(Debug, Default)]
pub(crate) struct Laws {
    /// Acknowledged operations per subject, in the order of their sequence.
    acked: BTreeMap<String, Vec<Ack>>,
    /// Operations begun per subject and not known to be acknowledged.
    begun: BTreeMap<String, Vec<Ack>>,
    /// Every batch begun: its operations, whether acknowledged or not.
    batches: BTreeMap<u64, Vec<Ack>>,
    /// The batches each start of each node has read part of.
    batch_reads: BTreeMap<(u32, u64, u64), BatchRead>,
    /// One diagnostic per law, node and subject.
    reported: BTreeSet<(&'static str, u32, String)>,
}

impl Laws {
    /// An operation begun, which may land or not.
    pub(crate) fn begun(&mut self, ack: Ack) {
        if let Some(batch) = ack.batch {
            self.batches.entry(batch).or_default().push(ack.clone());
        }
        self.begun.entry(ack.subject.clone()).or_default().push(ack);
    }

    /// An operation acknowledged.
    pub(crate) fn acked(&mut self, ack: Ack) {
        if let Some(batch) = ack.batch {
            let ops = self.batches.entry(batch).or_default();
            if !ops.iter().any(|op| op.subject == ack.subject) {
                ops.push(ack.clone());
            }
        }
        let history = self.acked.entry(ack.subject.clone()).or_default();
        let at = history.partition_point(|op| op.seq <= ack.seq);
        history.insert(at, ack);
    }

    /// One read, judged by R1.1, R1.2 and R1.3; every law it breaks.
    pub(crate) fn read(&mut self, read: &Read) -> Vec<Broken> {
        let mut broken = Vec::new();
        if read.owes {
            broken.extend(self.survives(read));
            broken.extend(self.stays_deleted(read));
        }
        broken.extend(self.whole_batches(read));
        broken
    }

    /// R1.1: the last acknowledged operation on the subject is a put, and the read answers absent
    /// or a value acknowledged before it and since superseded.
    fn survives(&mut self, read: &Read) -> Option<Broken> {
        let history = self.acked.get(&read.subject)?;
        let last = history.last()?;
        let wanted = last.value.as_ref()?;
        let lost = match &read.value {
            None => !self.begun_after(&read.subject, last.seq, |op| op.value.is_none()),
            Some(value) if value == wanted => false,
            Some(value) => {
                let superseded =
                    history[..history.len() - 1].iter().any(|op| op.value.as_ref() == Some(value));
                superseded
                    && !self
                        .begun_after(&read.subject, last.seq, |op| op.value.as_ref() == Some(value))
            }
        };
        if !lost {
            return None;
        }
        let seen = read.value.as_deref().map_or_else(|| "absent".to_owned(), |v| format!("{v:?}"));
        self.report(
            bohr_trace::ACKED_SURVIVES_CODE,
            read,
            format!(
                "R1.1: {} acknowledged as {wanted:?} read back {seen} by node {} (start {}){}",
                read.subject,
                read.node,
                read.start,
                where_made(last)
            ),
        )
    }

    /// R1.2: the last acknowledged operation on the subject removed it, and the read answers a
    /// value acknowledged before the removal.
    fn stays_deleted(&mut self, read: &Read) -> Option<Broken> {
        let history = self.acked.get(&read.subject)?;
        let last = history.last()?;
        if last.value.is_some() {
            return None;
        }
        let value = read.value.as_ref()?;
        let older = history.iter().any(|op| op.seq < last.seq && op.value.as_ref() == Some(value));
        if !older
            || self.begun_after(&read.subject, last.seq, |op| op.value.as_ref() == Some(value))
        {
            return None;
        }
        let message = format!(
            "R1.2: {} acknowledged as removed read back as {value:?} by node {} (start {}){}",
            read.subject,
            read.node,
            read.start,
            where_made(last)
        );
        self.report(bohr_trace::DELETED_STAYS_CODE, read, message)
    }

    /// R1.3: within one start, the batch's subjects read back some with its values and some with
    /// older ones. Judged for every batch begun, acknowledged or not: one in flight at a crash
    /// lands whole or not at all.
    fn whole_batches(&mut self, read: &Read) -> Vec<Broken> {
        let mut broken = Vec::new();
        let touched: Vec<(u64, bool)> = self
            .batches
            .iter()
            .filter_map(|(batch, ops)| {
                let op = ops.iter().find(|op| op.subject == read.subject)?;
                // A subject changed after the batch says nothing about whether the batch landed.
                if self.changed_after(&read.subject, op.seq) {
                    return None;
                }
                let new = read.value == op.value;
                let older = self.predates(&read.subject, op.seq, read.value.as_ref());
                // A value both the batch's and an older one tells neither.
                (new != older).then_some((*batch, new))
            })
            .collect();
        for (batch, new) in touched {
            let seen = self.batch_reads.entry((read.node, read.start, batch)).or_default();
            if new {
                seen.0.insert(read.subject.clone());
            } else {
                seen.1.insert(read.subject.clone());
            }
            if seen.0.is_empty() || seen.1.is_empty() {
                continue;
            }
            let landed: Vec<String> = seen.0.iter().cloned().collect();
            let missed: Vec<String> = seen.1.iter().cloned().collect();
            let at = self.batches.get(&batch).and_then(|ops| ops.first()).map(where_made);
            let mut subject = read.clone();
            subject.subject = format!("batch {batch}");
            broken.extend(self.report(
                bohr_trace::BATCH_WHOLE_CODE,
                &subject,
                format!(
                    "R1.3: batch {batch} read back in part by node {} (start {}): {} landed, {} \
                     did not{}",
                    read.node,
                    read.start,
                    landed.join(", "),
                    missed.join(", "),
                    at.unwrap_or_default()
                ),
            ));
        }
        broken
    }

    /// Whether `value` is what the subject held before the operation at `seq`: absent where nothing
    /// was acknowledged before it, or a value acknowledged before it.
    fn predates(&self, subject: &str, seq: u64, value: Option<&String>) -> bool {
        let before: Vec<&Ack> = self
            .acked
            .get(subject)
            .map(|h| h.iter().filter(|op| op.seq < seq).collect())
            .unwrap_or_default();
        match value {
            None => before.last().is_none_or(|op| op.value.is_none()),
            Some(value) => before.iter().any(|op| op.value.as_ref() == Some(value)),
        }
    }

    /// Whether an operation on the subject was begun or acknowledged after `seq`.
    fn changed_after(&self, subject: &str, seq: u64) -> bool {
        let later =
            |ops: Option<&Vec<Ack>>| ops.is_some_and(|ops| ops.iter().any(|op| op.seq > seq));
        later(self.acked.get(subject)) || later(self.begun.get(subject))
    }

    /// Whether an operation begun after `seq` on the subject, and matching, could explain the read.
    fn begun_after(&self, subject: &str, seq: u64, matches: impl Fn(&Ack) -> bool) -> bool {
        self.begun.get(subject).is_some_and(|ops| ops.iter().any(|op| op.seq > seq && matches(op)))
    }

    fn report(&mut self, code: &'static str, read: &Read, message: String) -> Option<Broken> {
        self.reported.insert((code, read.node, read.subject.clone())).then_some(Broken {
            code,
            node: read.node,
            message,
        })
    }
}

/// `, acknowledged at <file:line>` where the record named one.
fn where_made(ack: &Ack) -> String {
    ack.at.as_ref().map(|at| format!(", acknowledged at {at}")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(subject: &str, value: Option<&str>, seq: u64, batch: Option<u64>) -> Ack {
        Ack {
            subject: subject.to_owned(),
            value: value.map(str::to_owned),
            batch,
            seq,
            at: Some("store.c:42".to_owned()),
        }
    }

    fn read(subject: &str, value: Option<&str>, start: u64) -> Read {
        Read {
            subject: subject.to_owned(),
            value: value.map(str::to_owned),
            node: 0,
            start,
            owes: true,
        }
    }

    #[test]
    fn an_acknowledged_put_read_back_absent_or_superseded_breaks_r1_1() {
        let mut laws = Laws::default();
        laws.acked(op("k", Some("1"), 1, None));
        laws.acked(op("k", Some("2"), 2, None));
        assert!(laws.read(&read("k", Some("2"), 2)).is_empty());
        let broken = laws.read(&read("k", Some("1"), 2));
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].code, bohr_trace::ACKED_SURVIVES_CODE);
        assert!(broken[0].message.contains("store.c:42"), "{}", broken[0].message);
        // Once per law, node and subject.
        assert!(laws.read(&read("k", None, 3)).is_empty());
        let mut absent = Laws::default();
        absent.acked(op("j", Some("1"), 1, None));
        assert_eq!(absent.read(&read("j", None, 2)).len(), 1);
    }

    #[test]
    fn a_value_never_acknowledged_and_a_reader_that_does_not_owe_are_not_judged() {
        let mut laws = Laws::default();
        laws.acked(op("k", Some("1"), 1, None));
        assert!(laws.read(&read("k", Some("in-flight"), 2)).is_empty());
        let mut behind = read("k", None, 2);
        behind.owes = false;
        assert!(laws.read(&behind).is_empty());
        // A removal begun after the put may have landed.
        let mut removed = Laws::default();
        removed.acked(op("k", Some("1"), 1, None));
        removed.begun(op("k", None, 2, None));
        assert!(removed.read(&read("k", None, 2)).is_empty());
    }

    #[test]
    fn a_removed_value_read_back_breaks_r1_2() {
        let mut laws = Laws::default();
        laws.acked(op("k", Some("1"), 1, None));
        laws.acked(op("k", None, 2, None));
        assert!(laws.read(&read("k", None, 2)).is_empty());
        let broken = laws.read(&read("k", Some("1"), 2));
        assert_eq!(
            broken.iter().map(|b| b.code).collect::<Vec<_>>(),
            [bohr_trace::DELETED_STAYS_CODE]
        );
    }

    #[test]
    fn a_batch_is_not_judged_by_a_subject_changed_after_it_or_by_a_value_it_shares() {
        let mut later = Laws::default();
        later.acked(op("a", Some("1"), 1, Some(1)));
        later.acked(op("b", Some("1"), 1, Some(1)));
        later.acked(op("a", None, 2, None));
        assert!(later.read(&read("a", None, 2)).is_empty());
        assert!(later.read(&read("b", Some("1"), 2)).is_empty());
        let mut shared = Laws::default();
        shared.acked(op("a", Some("1"), 1, None));
        shared.begun(op("a", Some("1"), 2, Some(2)));
        shared.begun(op("b", Some("1"), 2, Some(2)));
        assert!(shared.read(&read("a", Some("1"), 2)).is_empty());
        assert!(shared.read(&read("b", None, 2)).is_empty());
    }

    #[test]
    fn a_batch_read_back_in_part_breaks_r1_3_acknowledged_or_not() {
        let mut laws = Laws::default();
        laws.begun(op("a", Some("1"), 1, Some(7)));
        laws.begun(op("b", Some("1"), 1, Some(7)));
        assert!(laws.read(&read("a", Some("1"), 2)).is_empty());
        let broken = laws.read(&read("b", None, 2));
        assert_eq!(
            broken.iter().map(|b| b.code).collect::<Vec<_>>(),
            [bohr_trace::BATCH_WHOLE_CODE]
        );
        // Read whole in another start: nothing more.
        assert!(laws.read(&read("a", Some("1"), 3)).is_empty());
        assert!(laws.read(&read("b", Some("1"), 3)).is_empty());
    }
}
