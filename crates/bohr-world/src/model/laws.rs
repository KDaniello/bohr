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

use super::{Ack, Broken, Decided, Read};

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

/// What the laws of agreement and liveness remember: each position's decisions by node, each
/// node's final decisions, each round's deciders, and how far each node has come.
///
/// Nothing here knows a block from a log entry: a position is decided as a value, and a state is
/// what deciding it left behind, where the family reports one.
#[derive(Debug, Default)]
pub(crate) struct Agreement {
    /// Each position's decision by each node: its value and the state it left, where reported.
    decided: BTreeMap<u64, BTreeMap<u32, (String, Option<String>)>>,
    /// What each node reported final at each position.
    finals: BTreeMap<(u32, u64), String>,
    /// Each round's deciders.
    claims: BTreeMap<u64, BTreeSet<u32>>,
    /// The highest position each node has decided or applied.
    reached: BTreeMap<u32, u64>,
    /// The highest position any node had decided when the fault window closed.
    top_at_close: Option<u64>,
    /// One diagnostic per law and position, round or node.
    reported: BTreeSet<(&'static str, u64)>,
}

impl Agreement {
    /// One decision, judged by R2.1, R2.2 and R2.3; every law it breaks.
    pub(crate) fn decided(&mut self, decision: &Decided) -> Vec<Broken> {
        let mut broken = Vec::new();
        self.reached(decision.node, decision.position);
        // R2.3: a node never decides again a position it reported final, a restart included.
        if let Some(earlier) = self.finals.get(&(decision.node, decision.position))
            && *earlier != decision.value
            && self.reported.insert((bohr_trace::NEVER_REVERTS_CODE, decision.position))
        {
            broken.push(Broken {
                code: bohr_trace::NEVER_REVERTS_CODE,
                node: decision.node,
                message: format!(
                    "R2.3: node {} decided position {} as {} after reporting it final as {earlier}",
                    decision.node, decision.position, decision.value
                ),
            });
        }
        if decision.final_ {
            self.finals
                .entry((decision.node, decision.position))
                .or_insert_with(|| decision.value.clone());
        }
        let at = self.decided.entry(decision.position).or_default();
        // R2.1 and R2.2 compare only replicas that answered this one position.
        for (other, (value, state)) in at.iter() {
            if *other == decision.node {
                continue;
            }
            if *value != decision.value
                && self.reported.insert((bohr_trace::AGREES_CODE, decision.position))
            {
                broken.push(Broken {
                    code: bohr_trace::AGREES_CODE,
                    node: decision.node,
                    message: format!(
                        "R2.1: position {} decided as {value} by node {other} and as {} by node {}",
                        decision.position, decision.value, decision.node
                    ),
                });
            }
            if let (Some(theirs), Some(ours)) = (state, &decision.state)
                && theirs != ours
                && self.reported.insert((bohr_trace::STATE_AGREES_CODE, decision.position))
            {
                broken.push(Broken {
                    code: bohr_trace::STATE_AGREES_CODE,
                    node: decision.node,
                    message: format!(
                        "R2.2: state at position {} is {theirs} on node {other} and {ours} on node {}",
                        decision.position, decision.node
                    ),
                });
            }
        }
        at.insert(decision.node, (decision.value.clone(), decision.state.clone()));
        broken
    }

    /// R2.4: a round with two deciders.
    pub(crate) fn claimed(&mut self, round: u64, node: u32) -> Option<Broken> {
        let deciders = self.claims.entry(round).or_default();
        deciders.insert(node);
        if deciders.len() < 2 || !self.reported.insert((bohr_trace::UNIQUE_CODE, round)) {
            return None;
        }
        let named: Vec<String> = deciders.iter().map(u32::to_string).collect();
        Some(Broken {
            code: bohr_trace::UNIQUE_CODE,
            node,
            message: format!("R2.4: round {round} has two deciders, nodes {}", named.join(" and ")),
        })
    }

    /// A node has come as far as `position`.
    pub(crate) fn reached(&mut self, node: u32, position: u64) {
        let at = self.reached.entry(node).or_default();
        *at = (*at).max(position);
    }

    /// The fault window closed: what was decided by now is what the system must move past.
    pub(crate) fn window_closed(&mut self) {
        self.top_at_close = Some(self.top());
    }

    /// R3.1 and R3.2, `bound` ticks after the window closed: the system decided past what it had,
    /// where `moves` asks it to, and every replica in `owing` reached it.
    pub(crate) fn window_due(&mut self, owing: &[u32], bound: u64, moves: bool) -> Vec<Broken> {
        let Some(then) = self.top_at_close else { return Vec::new() };
        let mut broken = Vec::new();
        let now = self.top();
        if moves && now <= then && self.reported.insert((bohr_trace::ADVANCES_CODE, then)) {
            broken.push(Broken {
                code: bohr_trace::ADVANCES_CODE,
                node: owing.first().copied().unwrap_or(0),
                message: format!(
                    "R3.1: nothing was decided past position {then} within {bound} ticks of the \
                     fault window closing"
                ),
            });
        }
        for node in owing {
            let at = self.reached.get(node).copied().unwrap_or(0);
            if at < then && self.reported.insert((bohr_trace::RESTARTS_CODE, u64::from(*node))) {
                broken.push(Broken {
                    code: bohr_trace::RESTARTS_CODE,
                    node: *node,
                    message: format!(
                        "R3.2: node {node} reached position {at} within {bound} ticks of the fault \
                         window closing, where position {then} was decided before it closed"
                    ),
                });
            }
        }
        broken
    }

    fn top(&self) -> u64 {
        self.reached.values().copied().max().unwrap_or(0)
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

    fn decided(node: u32, position: u64, value: &str, state: Option<&str>) -> Decided {
        Decided {
            position,
            value: value.to_owned(),
            state: state.map(str::to_owned),
            node,
            final_: true,
        }
    }

    #[test]
    fn two_replicas_deciding_one_position_differently_break_r2_1_and_r2_2() {
        let mut laws = Agreement::default();
        assert!(laws.decided(&decided(0, 5, "A", Some("s"))).is_empty());
        assert!(laws.decided(&decided(1, 5, "A", Some("s"))).is_empty());
        let broken = laws.decided(&decided(2, 5, "B", Some("t")));
        let codes: Vec<&str> = broken.iter().map(|b| b.code).collect();
        assert_eq!(codes, [bohr_trace::AGREES_CODE, bohr_trace::STATE_AGREES_CODE]);
        // A replica behind is not a disagreement: position 6 has one decider.
        assert!(laws.decided(&decided(0, 6, "C", None)).is_empty());
    }

    #[test]
    fn a_final_decision_taken_back_breaks_r2_3_and_two_deciders_break_r2_4() {
        let mut laws = Agreement::default();
        assert!(laws.decided(&decided(0, 3, "A", None)).is_empty());
        let broken = laws.decided(&decided(0, 3, "B", None));
        assert_eq!(
            broken.iter().map(|b| b.code).collect::<Vec<_>>(),
            [bohr_trace::NEVER_REVERTS_CODE]
        );
        assert!(laws.claimed(7, 0).is_none());
        assert!(laws.claimed(7, 0).is_none());
        assert_eq!(laws.claimed(7, 1).map(|b| b.code), Some(bohr_trace::UNIQUE_CODE));
    }

    #[test]
    fn after_the_window_the_system_moves_and_every_replica_catches_up() {
        let mut laws = Agreement::default();
        laws.decided(&decided(0, 10, "A", None));
        laws.decided(&decided(1, 10, "A", None));
        laws.reached(2, 4);
        laws.window_closed();
        laws.decided(&decided(0, 11, "B", None));
        let broken = laws.window_due(&[0, 1, 2], 100, true);
        assert_eq!(
            broken.iter().map(|b| (b.code, b.node)).collect::<Vec<_>>(),
            [(bohr_trace::RESTARTS_CODE, 2)]
        );
        let mut stuck = Agreement::default();
        stuck.decided(&decided(0, 10, "A", None));
        stuck.window_closed();
        assert_eq!(
            stuck.window_due(&[0], 100, true).iter().map(|b| b.code).collect::<Vec<_>>(),
            [bohr_trace::ADVANCES_CODE]
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
