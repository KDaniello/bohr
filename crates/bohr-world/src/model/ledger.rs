//! The ledger: what a store's counters should hold, from the operations it began and acknowledged,
//! never from what it says it holds.
//!
//! Each counter has a base, its last acknowledged put, and the effects of every operation on it by
//! its id, each id once. A read is judged against the two bounds those allow: every begun
//! operation applied once is the most it can hold, every acknowledged one applied, with any begun
//! one's debit, the least. Moves form a closed set whose total is the bases' sum, read whole within
//! one start. Nothing here knows a store from a chain: it reads the vocabulary alone, which lint
//! 14o holds it to.

use std::collections::{BTreeMap, BTreeSet};

use super::{Broken, Read};

/// What a node said and held of one subject at one position, for R6.1: reported there, or held
/// there.
#[derive(Debug, Default)]
pub(crate) struct Api {
    /// Where each node reported each subject: `(node, subject)` to the position.
    reported: BTreeMap<(u32, String), u64>,
    /// What each node holds at each position, where it was asked.
    held: BTreeMap<(u32, u64), BTreeSet<String>>,
    reported_broken: BTreeSet<(u32, String)>,
}

impl Api {
    /// A node reported `subject` at `position`.
    pub(crate) fn reported(&mut self, node: u32, subject: &str, position: u64) -> Option<Broken> {
        self.reported.insert((node, subject.to_owned()), position);
        self.judge(node, subject)
    }

    /// A node holds `subjects` at `position`, and only those.
    pub(crate) fn held(&mut self, node: u32, position: u64, subjects: Vec<String>) -> Vec<Broken> {
        self.held.insert((node, position), subjects.into_iter().collect());
        let named: Vec<String> = self
            .reported
            .iter()
            .filter(|((n, _), p)| *n == node && **p == position)
            .map(|((_, s), _)| s.clone())
            .collect();
        named.iter().filter_map(|subject| self.judge(node, subject)).collect()
    }

    /// R6.1: what a node reported at a position is what it holds there.
    fn judge(&mut self, node: u32, subject: &str) -> Option<Broken> {
        let position = *self.reported.get(&(node, subject.to_owned()))?;
        let held = self.held.get(&(node, position))?;
        if held.contains(subject) || !self.reported_broken.insert((node, subject.to_owned())) {
            return None;
        }
        Some(Broken {
            code: bohr_trace::INCLUDED_CODE,
            node,
            message: format!(
                "R6.1: node {node} reported {subject} at position {position}, and its own \
                 position {position} does not hold it"
            ),
        })
    }
}

/// One operation on counters: its id, what it does to each, whether acknowledged, and where.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Counted {
    /// The node whose store it acts on.
    pub(crate) node: u32,
    /// The operation's identity, retried under the same id until acknowledged.
    pub(crate) id: String,
    /// The change to each counter it touches.
    pub(crate) effects: Vec<(String, i64)>,
    /// Whether the store acknowledged it.
    pub(crate) acked: bool,
    /// Whether it moves an amount between counters, which keeps their total.
    pub(crate) moves: bool,
    /// Where the guest made it, as `file:line`.
    pub(crate) at: Option<String>,
}

/// What one counter is owed.
#[derive(Debug, Default)]
struct Counter {
    /// Its last acknowledged put.
    base: Option<i64>,
    /// A put begun and not acknowledged: the base is the old one or the new, and is not judged.
    open: bool,
    /// Each operation's change to it since its base, by id: the change and whether acknowledged.
    ops: BTreeMap<String, (i64, bool)>,
}

/// The counters one node's moves touch: a closed set, whose total the moves keep.
#[derive(Debug, Default)]
struct Closed {
    counters: BTreeSet<String>,
    /// The counters an `add` touches: where one is in the set, its total is open.
    adds: BTreeSet<String>,
    /// Moves counted on the node so far: a read is of the set as it stood at that many.
    moves: u64,
    /// What each start read of each counter of the set, and at how many moves.
    reads: BTreeMap<u64, BTreeMap<String, (i64, u64)>>,
}

/// The ledger of a store's counters.
#[derive(Debug, Default)]
pub(crate) struct Ledger {
    counters: BTreeMap<String, Counter>,
    /// Where each operation was made, by id.
    made_at: BTreeMap<String, String>,
    /// Each node's closed set.
    closed: BTreeMap<u32, Closed>,
    /// One diagnostic per law and subject.
    reported: BTreeSet<(&'static str, String)>,
}

impl Ledger {
    /// A counter's base, from an acknowledged put of a number; what came before it is superseded.
    pub(crate) fn base(&mut self, subject: &str, value: i64) {
        let counter = self.counters.entry(subject.to_owned()).or_default();
        counter.base = Some(value);
        counter.open = false;
        counter.ops.clear();
    }

    /// A put of a number begun on `subject`, not yet acknowledged.
    pub(crate) fn put_begun(&mut self, subject: &str) {
        if let Some(counter) = self.counters.get_mut(subject) {
            counter.open = true;
        }
    }

    /// An operation begun or acknowledged; an id seen again is the same operation.
    pub(crate) fn counted(&mut self, op: &Counted) {
        if let Some(at) = &op.at {
            self.made_at.entry(op.id.clone()).or_insert_with(|| at.clone());
        }
        let closed = self.closed.entry(op.node).or_default();
        if op.moves {
            closed.moves += 1;
        }
        for (subject, effect) in &op.effects {
            let entry = self.counters.entry(subject.clone()).or_default().ops.entry(op.id.clone());
            let held = entry.or_insert((*effect, false));
            held.1 |= op.acked;
            if op.moves {
                closed.counters.insert(subject.clone());
            }
        }
        if !op.moves {
            closed.adds.extend(op.effects.iter().map(|(s, _)| s.clone()));
        }
    }

    /// Whether `subject` is a counter the ledger holds.
    pub(crate) fn knows(&self, subject: &str) -> bool {
        self.counters.get(subject).is_some_and(|c| c.base.is_some() || !c.ops.is_empty())
    }

    /// One read of a counter, judged by R4.1 and R5.1; every law it breaks.
    pub(crate) fn read(&mut self, read: &Read) -> Vec<Broken> {
        let mut broken = Vec::new();
        let Some(value) = read.value.as_deref().and_then(|v| v.parse::<i64>().ok()) else {
            return broken;
        };
        if let Some(closed) = self.closed.get_mut(&read.node)
            && closed.counters.contains(&read.subject)
        {
            let at = closed.moves;
            closed.reads.entry(read.start).or_default().insert(read.subject.clone(), (value, at));
            broken.extend(self.conserved(read.node, read.start));
        }
        // Only a counter something operated on since its base: a key only ever put is R1's.
        let Some(counter) =
            self.counters.get(&read.subject).filter(|c| !c.ops.is_empty() && !c.open)
        else {
            return broken;
        };
        let base = counter.base.unwrap_or(0);
        let most = base + counter.ops.values().map(|(e, _)| (*e).max(0)).sum::<i64>();
        let least = base
            + counter
                .ops
                .values()
                .map(|(effect, acked)| if *acked { *effect } else { (*effect).min(0) })
                .sum::<i64>();
        if value > most && self.reported.insert((bohr_trace::ONCE_CODE, read.subject.clone())) {
            let at = self.where_made(&read.subject);
            broken.push(Broken {
                code: bohr_trace::ONCE_CODE,
                node: read.node,
                message: format!(
                    "R4.1: {} read back as {value} by node {} (start {}), above the {most} every \
                     operation begun on it makes applied once each{at}",
                    read.subject, read.node, read.start
                ),
            });
        }
        if value < least
            && self.reported.insert((bohr_trace::ACKED_SURVIVES_CODE, read.subject.clone()))
        {
            let at = self.where_made(&read.subject);
            broken.push(Broken {
                code: bohr_trace::ACKED_SURVIVES_CODE,
                node: read.node,
                message: format!(
                    "R1.1: {} read back as {value} by node {} (start {}), below the {least} its \
                     acknowledged operations make{at}",
                    read.subject, read.node, read.start
                ),
            });
        }
        broken
    }

    /// R5.1, once one start has read every counter of a node's closed set with no move counted
    /// between the reads: their sum is the bases'.
    fn conserved(&mut self, node: u32, start: u64) -> Option<Broken> {
        let closed = self.closed.get(&node)?;
        // A set an `add` touches, or one whose base a put left unknown, has no fixed total.
        if closed.counters.iter().any(|c| closed.adds.contains(c))
            || closed.counters.iter().any(|c| self.counters.get(c).is_some_and(|c| c.open))
        {
            return None;
        }
        let read = closed.reads.get(&start)?;
        if closed.counters.iter().any(|c| read.get(c).is_none_or(|(_, at)| *at != closed.moves)) {
            return None;
        }
        let total: i64 = closed
            .counters
            .iter()
            .map(|c| self.counters.get(c).and_then(|c| c.base).unwrap_or(0))
            .sum();
        let held: i64 = read.values().map(|(v, _)| v).sum();
        let subject = format!("node {node} start {start}");
        if held == total || !self.reported.insert((bohr_trace::CONSERVED_CODE, subject)) {
            return None;
        }
        let named: Vec<String> = read.iter().map(|(c, (v, _))| format!("{c} = {v}")).collect();
        let at = closed.counters.iter().next().map(|c| self.where_made(c)).unwrap_or_default();
        Some(Broken {
            code: bohr_trace::CONSERVED_CODE,
            node,
            message: format!(
                "R5.1: the counters the moves keep sum to {held} on node {node} (start {start}), \
                 where they began at {total}: {}{at}",
                named.join(", ")
            ),
        })
    }

    /// `, made at <file:line>` of the first operation on the counter whose place is known.
    fn where_made(&self, subject: &str) -> String {
        self.counters
            .get(subject)
            .and_then(|c| c.ops.keys().find_map(|id| self.made_at.get(id)))
            .map(|at| format!(", acknowledged at {at}"))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(id: &str, counter: &str, delta: i64, acked: bool) -> Counted {
        Counted {
            node: 0,
            id: id.to_owned(),
            effects: vec![(counter.to_owned(), delta)],
            acked,
            moves: false,
            at: Some("s.c:7".to_owned()),
        }
    }

    fn read(subject: &str, value: i64, start: u64) -> Read {
        Read {
            subject: subject.to_owned(),
            value: Some(value.to_string()),
            node: 0,
            start,
            owes: true,
        }
    }

    #[test]
    fn an_operation_applied_twice_breaks_r4_1_and_one_lost_r1_1() {
        let mut ledger = Ledger::default();
        ledger.counted(&add("a", "c", 1, true));
        ledger.counted(&add("b", "c", 1, false));
        // Retried under the same id: still one operation.
        ledger.counted(&add("b", "c", 1, false));
        assert!(ledger.read(&read("c", 2, 2)).is_empty());
        assert!(ledger.read(&read("c", 1, 2)).is_empty());
        let twice = ledger.read(&read("c", 3, 3));
        assert_eq!(twice.iter().map(|b| b.code).collect::<Vec<_>>(), [bohr_trace::ONCE_CODE]);
        assert!(twice[0].message.contains("s.c:7"), "{}", twice[0].message);
        assert!(!twice[0].message.contains('\\'), "{}", twice[0].message);
        let lost = ledger.read(&read("c", 0, 3));
        assert_eq!(
            lost.iter().map(|b| b.code).collect::<Vec<_>>(),
            [bohr_trace::ACKED_SURVIVES_CODE]
        );
    }

    #[test]
    fn a_move_that_debits_without_crediting_breaks_r5_1() {
        let mut ledger = Ledger::default();
        ledger.base("a", 100);
        ledger.base("b", 0);
        let mv = Counted {
            node: 0,
            id: "m1".to_owned(),
            effects: vec![("a".to_owned(), -10), ("b".to_owned(), 10)],
            acked: false,
            moves: true,
            at: None,
        };
        ledger.counted(&mv);
        assert!(ledger.read(&read("a", 90, 2)).is_empty());
        let broken = ledger.read(&read("b", 0, 2));
        assert_eq!(broken.iter().map(|b| b.code).collect::<Vec<_>>(), [bohr_trace::CONSERVED_CODE]);
        // Whole in another start: nothing.
        assert!(ledger.read(&read("a", 90, 3)).is_empty());
        assert!(ledger.read(&read("b", 10, 3)).is_empty());
    }

    fn mv(id: &str, acked: bool) -> Counted {
        Counted {
            node: 0,
            id: id.to_owned(),
            effects: vec![("a".to_owned(), -10), ("b".to_owned(), 10)],
            acked,
            moves: true,
            at: None,
        }
    }

    #[test]
    fn a_read_from_before_a_move_is_not_summed_with_one_after_it() {
        let mut ledger = Ledger::default();
        ledger.base("a", 100);
        ledger.base("b", 0);
        for (id, a, b) in [("m0", 90, 10), ("m1", 80, 20)] {
            ledger.counted(&mv(id, false));
            ledger.counted(&mv(id, true));
            assert!(ledger.read(&read("a", a, 1)).is_empty());
            assert!(ledger.read(&read("b", b, 1)).is_empty());
        }
    }

    #[test]
    fn a_put_again_supersedes_what_came_before_it() {
        let mut ledger = Ledger::default();
        ledger.base("c", 0);
        ledger.counted(&add("x", "c", 5, true));
        ledger.base("c", 0);
        assert!(ledger.read(&read("c", 0, 2)).is_empty());
        // A put begun and lost leaves the base unknown: nothing is judged.
        ledger.counted(&add("y", "c", 1, true));
        ledger.put_begun("c");
        assert!(ledger.read(&read("c", 7, 3)).is_empty());
    }

    #[test]
    fn each_node_keeps_its_own_closed_set() {
        let mut ledger = Ledger::default();
        ledger.base("a", 100);
        ledger.base("b", 0);
        ledger.base("n1 c", 3);
        ledger.counted(&mv("m0", false));
        ledger.counted(&Counted { node: 1, ..add("z", "n1 c", 1, true) });
        assert!(ledger.read(&read("a", 90, 2)).is_empty());
        let broken = ledger.read(&read("b", 0, 2));
        assert_eq!(broken.iter().map(|b| b.code).collect::<Vec<_>>(), [bohr_trace::CONSERVED_CODE]);
    }
}
