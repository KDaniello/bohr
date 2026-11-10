//! The one queue everything scheduled for later lives in.

use std::collections::BTreeMap;

use bohr_types::{NodeId, TaskId, Tick};

/// What a timer does when it fires.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Due {
    /// A sleeping task's deadline passed.
    SleepExpiry {
        /// The task that slept.
        task: TaskId,
    },
    /// A timed futex wait ran out.
    FutexTimeout {
        /// The node whose futex space holds the wait.
        node: NodeId,
        /// The task that waited.
        task: TaskId,
        /// The word it waited on.
        word: u64,
        /// Which of the task's waits armed it.
        seq: u64,
    },
    /// A bounded `epoll_wait` ran out with nothing to report.
    PollTimeout {
        /// The node whose readiness set holds the wait.
        node: NodeId,
        /// The instance waited on.
        epfd: i64,
        /// The task that waited.
        task: TaskId,
    },
    /// A packet on the wire reached its delivery tick.
    PacketDelivery {
        /// The in-flight identifier the network handed out at send.
        id: u64,
    },
    /// A scheduled partition entry is raised or healed.
    PartitionChange {
        /// Index into the configured partition schedule.
        index: usize,
        /// True to raise the cut, false to heal it.
        raise: bool,
    },
    /// A scheduled grey window opens or closes.
    GreyChange {
        /// Index into the configured window list.
        index: usize,
        /// True to open the window, false to close it.
        open: bool,
    },
    /// A stalled `fsync` completes.
    DiskStallDone {
        /// The node whose disk was stalling.
        node: NodeId,
        /// The task whose sync this timer belongs to.
        ///
        /// Named, since a node can have several stalls at once.
        task: TaskId,
    },
    /// A scheduled node fault fires.
    NodeFault {
        /// Index into the configured fault list.
        index: usize,
    },
    /// A liveness window opened by a heal is due an answer.
    ///
    /// A timer, since the finding is an absence.
    LivenessWindow,
    /// Time to ask which nodes have stopped making calls.
    ///
    /// Re-armed each time it fires.
    QuietCheck,
}

/// Timers ordered by `(tick, node, seq)` and never by insertion order.
///
/// Entries never tie: the per-run sequence number orders timers at one tick. See
/// `docs/ARCHITECTURE.md` §8.1.
#[derive(Debug, Default)]
pub(crate) struct TimerQueue {
    entries: BTreeMap<(Tick, NodeId, u64), Due>,
    seq: u64,
}

impl TimerQueue {
    /// Arms `due` to fire at `at` on `node`.
    pub(crate) fn arm(&mut self, at: Tick, node: NodeId, due: Due) {
        self.entries.insert((at, node, self.seq), due);
        self.seq += 1;
    }

    /// The earliest armed tick, if any.
    pub(crate) fn next(&self) -> Option<Tick> {
        self.entries.keys().next().map(|(at, _, _)| *at)
    }

    /// Removes and returns everything due at or before `now`, in firing order.
    pub(crate) fn pop_due(&mut self, now: Tick) -> Vec<Due> {
        let mut due = Vec::new();
        while let Some((&key, _)) = self.entries.first_key_value() {
            if key.0 > now {
                break;
            }
            let entry = self.entries.remove(&key).expect("the key was just observed");
            due.push(entry);
        }
        #[cfg(feature = "chaos-deadline-tie")]
        crate::chaos::scramble_due(&mut due);
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firing_order_is_tick_then_node_then_arming_order() {
        let mut q = TimerQueue::default();
        let t = Tick(5);
        q.arm(t, NodeId(2), Due::SleepExpiry { task: TaskId(20) });
        q.arm(t, NodeId(1), Due::SleepExpiry { task: TaskId(10) });
        q.arm(t, NodeId(1), Due::SleepExpiry { task: TaskId(11) });
        q.arm(Tick(4), NodeId(3), Due::SleepExpiry { task: TaskId(30) });

        let fired = q.pop_due(Tick(5));
        let tasks: Vec<u32> = fired
            .iter()
            .map(|d| match d {
                Due::SleepExpiry { task } => task.get(),
                Due::FutexTimeout { task, .. } => task.get(),
                _ => unreachable!("only sleep timers are armed in this test"),
            })
            .collect();
        assert_eq!(tasks, [30, 10, 11, 20], "earlier tick, then lower node, then arming order");
    }

    #[test]
    fn nothing_past_now_fires_and_the_rest_stays_armed() {
        let mut q = TimerQueue::default();
        q.arm(Tick(10), NodeId(0), Due::SleepExpiry { task: TaskId(1) });
        q.arm(Tick(20), NodeId(0), Due::SleepExpiry { task: TaskId(2) });

        assert_eq!(q.pop_due(Tick(15)).len(), 1);
        assert_eq!(q.next(), Some(Tick(20)));
    }
}
