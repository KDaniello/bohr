//! Watching for the things a guest never says about itself.
//!
//! Every oracle before this one read either an assertion the guest wrote or the status it exited
//! with. A blockchain client offers neither: it does not adopt an SDK, it does not exit, and when
//! it is wrong it stays alive and keeps answering. What is left to judge it by is the boundary this
//! crate already owns — the calls it made and the moments the world chose for it.
//!
//! **The condition is watched here rather than read off the timeline afterwards, and that is a fact
//! about the code rather than a preference.** A stalled `fsync` emits its `DiskOp` at the *start*
//! of the stall and nothing at all when it completes, so "this node made something durable" is not
//! a predicate over the event stream whenever `stall_probability > 0` — a reader of the log counts
//! an interrupted sync as durability and misses the real one. Durability exists in exactly one
//! place, which is the moment `NodeDisk::commit` runs. Fed there, a watcher is right by
//! construction; reading the log is wrong on precisely the runs a durability search cares about.
//!
//! The division of labour, so it cannot blur: **this states a fact and gives it a code; the oracle
//! decides the fact is a finding.** Nothing here says a run is a bug.

use std::collections::{BTreeMap, BTreeSet};

use bohr_trace::NodeState;
use bohr_types::{NodeId, Tick};

/// What counts as the cluster making progress.
///
/// One definition, and the alternative is worth naming because it is the obvious one and it is
/// wrong: a cluster that has stopped committing still gossips, so counting delivered packets as
/// progress would read a dead cluster as a healthy one forever.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LivenessConfig {
    /// How long a silence has to last, in ticks after the last cut lifts.
    pub after_heal_ticks: Tick,
    /// How long a node may make no call at all before it is reported. Zero watches for nothing.
    ///
    /// **A server that is idle is not quiet.** It polls: an execution client with nothing to do
    /// spends a million clock reads and half a million waits on saying so. A node that has stopped
    /// making calls entirely has stopped, and the two are told apart by this and by nothing else —
    /// a task blocked for ever and a task waiting on a timer look the same from the run loop,
    /// which is why a horizon rescues both and why this is watched rather than inferred.
    ///
    /// The span belongs to the scenario because what counts as too long belongs to the guest.
    pub quiet_ticks: Tick,
}

/// Watches for a cluster that could talk to itself again and went on committing nothing.
///
/// Armed by a heal and answered by a timer, because the finding is the *absence* of something and
/// nothing in a stream of events marks an absence. The timer is what turns "nothing happened" into
/// a moment, which is also what puts a positive event on the timeline for a reader.
#[derive(Debug, Default)]
pub(crate) struct Liveness {
    config: LivenessConfig,
    /// Completed syncs per node, counted for the life of the run.
    ///
    /// A counter rather than a flag, so a window opened while an earlier one is still open reads
    /// the progress made since *it* opened rather than since the run began.
    commits: BTreeMap<NodeId, u64>,
    /// What each node had committed when the window opened, and when that was.
    open: Option<Window>,
    /// When each node was last seen doing anything at all.
    seen: BTreeMap<NodeId, Tick>,
    /// Nodes the world has taken away, which are silent by our doing rather than their own.
    down: BTreeSet<NodeId>,
    /// Nodes already reported quiet, so a run says it once rather than at every check.
    reported: BTreeMap<NodeId, ()>,
}

/// One span of watching, from a heal to the moment the answer is due.
#[derive(Clone, Debug)]
struct Window {
    /// When the last cut lifted.
    healed_at: Tick,
    /// Commits per node at that moment.
    was: BTreeMap<NodeId, u64>,
}

/// What a closed window found.
#[derive(Clone, Debug)]
pub(crate) struct Silence {
    /// When the heal that opened it happened.
    pub(crate) healed_at: Tick,
    /// How long the window stayed open.
    pub(crate) span: Tick,
    /// Nodes that committed nothing while it was.
    pub(crate) nodes: Vec<NodeId>,
}

impl Liveness {
    /// A watcher for the span a scenario asked for, or nothing where it asked for none.
    pub(crate) fn new(config: Option<LivenessConfig>) -> Option<Self> {
        config.map(|config| Self { config, ..Self::default() })
    }

    /// Records that a node completed a sync.
    pub(crate) fn committed(&mut self, node: NodeId) {
        *self.commits.entry(node).or_default() += 1;
    }

    /// A node the cluster has, whether or not it has ever committed.
    ///
    /// Called when a node is first seen, so a node that never syncs at all is counted as silent
    /// rather than as absent — which is the case the whole oracle is about.
    pub(crate) fn knows(&mut self, node: NodeId) {
        self.commits.entry(node).or_default();
    }

    /// How long a node may go without making a call, or nothing where the scenario asked for none.
    pub(crate) const fn quiet_span(&self) -> Option<Tick> {
        if self.config.quiet_ticks.get() == 0 { None } else { Some(self.config.quiet_ticks) }
    }

    /// Records that a node did something.
    pub(crate) fn stirred(&mut self, node: NodeId, now: Tick) {
        self.seen.insert(node, now);
    }

    /// Records what the world did to a node.
    ///
    /// **A node this world killed or froze makes no calls, and that is our doing.** From here it is
    /// indistinguishable from a node that stopped on its own, so without this a campaign built on
    /// crashes would report a stopped node at every fault whose outage outlasts the span — a false
    /// finding on precisely the runs such a campaign exists to produce. Watching resumes when the
    /// node does, timed from that moment, and a node reported before may be reported again: what it
    /// did in an earlier life is not evidence about this one.
    pub(crate) fn node_became(&mut self, node: NodeId, state: NodeState, now: Tick) {
        match state {
            NodeState::Dead | NodeState::Frozen => {
                self.down.insert(node);
            }
            NodeState::Running | NodeState::Restarting => {
                self.down.remove(&node);
                self.seen.insert(node, now);
                self.reported.remove(&node);
            }
        }
    }

    /// Records that a node's last task exited.
    ///
    /// A program that finished is silent for the rest of the run and is not a program that stopped:
    /// a caller that has asked everything it came to ask leaves, and accusing it of having gone
    /// quiet would turn every scenario whose driver completes into a finding. Told apart from a
    /// node the world killed only by who ended it, and both are answered the same way.
    pub(crate) fn ended(&mut self, node: NodeId) {
        self.down.insert(node);
    }

    /// The nodes that have made no call for the whole span, each named once.
    ///
    /// A node never seen at all is not reported: it has not gone quiet, it has not started, and
    /// accusing it would turn a cluster whose nodes come up in order into a finding. Nor is a node
    /// the world is holding down — see [`Self::node_became`].
    pub(crate) fn gone_quiet(&mut self, now: Tick) -> Vec<(NodeId, Tick)> {
        let Some(span) = self.quiet_span() else {
            return Vec::new();
        };
        let mut quiet = Vec::new();
        for (node, last) in &self.seen {
            let silent_for = now.get().saturating_sub(last.get());
            let answerable = !self.down.contains(node) && !self.reported.contains_key(node);
            if silent_for >= span.get() && answerable {
                quiet.push((*node, Tick(silent_for)));
            }
        }
        for (node, _) in &quiet {
            self.reported.insert(*node, ());
        }
        quiet
    }

    /// Opens a window at a heal, and says when the answer is due.
    ///
    /// Called only when the *last* cut lifts: a partition that healed one direction while another
    /// stays cut has not healed, and a window opened there would be measuring silence the cluster
    /// has every reason to keep.
    pub(crate) fn healed(&mut self, now: Tick) -> Tick {
        self.open = Some(Window { healed_at: now, was: self.commits.clone() });
        Tick(now.get().saturating_add(self.config.after_heal_ticks.get()))
    }

    /// Closes the open window and says which nodes committed nothing while it was open.
    ///
    /// Answers `None` where the window was already closed — a second heal within one span replaces
    /// the first, so the timer the first armed fires into nothing.
    pub(crate) fn due(&mut self, now: Tick) -> Option<Silence> {
        let window = self.open.take()?;
        let nodes: Vec<NodeId> = self
            .commits
            .iter()
            .filter(|(node, count)| window.was.get(node).copied().unwrap_or(0) == **count)
            .map(|(node, _)| *node)
            .collect();

        Some(Silence {
            healed_at: window.healed_at,
            span: Tick(now.get().saturating_sub(window.healed_at.get())),
            nodes,
        })
    }
}

/// What a node telling somebody something means, where the world has no word for an answer.
///
/// The hard part said plainly, so nobody mistakes it for a definition: this model has no notion of
/// an acknowledgement, and the milestone forbids parsing the guest's protocol. What it has is the
/// moment — **an outbound datagram sent while the node's disk is dirty**. The node told somebody
/// something at an instant when what it had written was not yet durable, which is the same instant
/// guided fault targeting already aims at, and is exactly the window `fsyncgate` names.
///
/// It is an approximation in one direction only: a node that answers nothing is never accused, and
/// a node that answers after syncing is never accused. What it can over-report is a node that sent
/// an unrelated datagram in the window — which is why the finding also requires the data to be
/// *missing afterwards*, and a node that lost nothing is not a finding however much it sent.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DurabilityConfig;

/// Watches for a node that answered before it synced and came back without the data.
///
/// **What this can and cannot see.** A mark is a file *length*, so the loss it detects is a file
/// that came back shorter than what was answered for. That is faithful for a store whose data file
/// grows monotonically and is never rewritten in place — an append-structured log — and it is blind
/// to a store that commits by writing into a region and updating a header, which is what a database
/// does: such a store legitimately discards a partial transaction on recovery without the file
/// changing length at all.
///
/// Comparing content instead would be worse, not better. A store recovering correctly rewrites its
/// header and drops what it never promised, so a content comparison would report a difference on
/// nearly every crash — and an oracle that fires on nearly every seed is a machine for false
/// findings, by the same argument that governs projections (`docs/ARCHITECTURE.md` §5.20).
///
/// The property "an acknowledged write survived" is not expressible over the bytes of a file at
/// all. It exists only through the store's own read path after the store's own recovery, which is
/// where every crash-consistency tool for applications puts it and where a client puts it here.
/// What this watcher contributes to such a hunt is not the verdict but [`Self::outstanding`]: what
/// was at risk when the crash landed.
#[derive(Debug, Default)]
pub(crate) struct Durability {
    /// The longest each file was when its node last answered somebody with it unsynced.
    ///
    /// A high-water mark rather than a log of writes: what a restart has to be measured against is
    /// the most the node ever implied it had, and marks only rise while the disk stays dirty.
    ///
    /// Nested by node rather than keyed by the pair, because what a restart asks for is one node's
    /// whole set: a flat map answers that by walking every key and rebuilding the names it just
    /// read, and a commit answers it by building a key per file at every `fsync`.
    marks: BTreeMap<NodeId, BTreeMap<String, u64>>,
}

/// One file a node answered with and came back without.
#[derive(Clone, Debug)]
pub(crate) struct Lost {
    /// The file that went short.
    pub(crate) file: String,
    /// How long it was when the node answered.
    pub(crate) answered: u64,
    /// How long it was when the node came back.
    pub(crate) came_back: u64,
}

impl Durability {
    /// A watcher, or nothing where the scenario asked for none.
    pub(crate) fn new(config: Option<DurabilityConfig>) -> Option<Self> {
        config.map(|_| Self::default())
    }

    /// Records that a node answered somebody while these files were as long as this.
    ///
    /// Called at a send made with the node's disk dirty. Every file is marked, not the one being
    /// written: the node has told somebody something, and what stands behind that is whatever it
    /// had written and not synced.
    pub(crate) fn answered(&mut self, node: NodeId, files: &[(String, u64)]) {
        let mine = self.marks.entry(node).or_default();
        for (name, len) in files {
            match mine.get_mut(name) {
                Some(mark) => *mark = (*mark).max(*len),
                None => {
                    mine.insert(name.clone(), *len);
                }
            }
        }
    }

    /// Clears what a completed sync made good.
    ///
    /// A mark at or below what is now durable is a promise the node kept, and keeping it around
    /// would accuse a node of losing something it had synced.
    pub(crate) fn committed(&mut self, node: NodeId, files: &[(String, u64)]) {
        let Some(mine) = self.marks.get_mut(&node) else {
            return;
        };
        for (name, durable) in files {
            if mine.get(name).is_some_and(|mark| mark <= durable) {
                mine.remove(name);
            }
        }
    }

    /// What this node has answered for and not yet made durable: how many files, and how much.
    ///
    /// The exposure at a moment, and never a verdict. A crash on a node with nothing outstanding
    /// tests that the store survives being killed; a crash on a node with something outstanding
    /// tests whether it keeps what it said. Only the second kind can cost anything, and a campaign
    /// that cannot tell how often it landed the second kind cannot say what its clean runs mean.
    pub(crate) fn outstanding(&self, node: NodeId) -> (u32, u64) {
        let Some(mine) = self.marks.get(&node) else {
            return (0, 0);
        };
        (mine.len() as u32, mine.values().sum())
    }

    /// What a node came back without, measured against what it had answered with.
    ///
    /// Called once the restarted node's disk is in place. The marks are consumed either way: a node
    /// that came back whole kept its promise, and one that did not has had it reported.
    pub(crate) fn restarted(&mut self, node: NodeId, files: &[(String, u64)]) -> Vec<Lost> {
        let mut lost = Vec::new();
        for (name, answered) in self.marks.remove(&node).unwrap_or_default() {
            let came_back = files.iter().find(|(file, _)| *file == name).map_or(0, |(_, len)| *len);
            if came_back < answered {
                lost.push(Lost { file: name, answered, came_back });
            }
        }
        lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watching(after: u64) -> Liveness {
        Liveness::new(Some(LivenessConfig { after_heal_ticks: Tick(after), quiet_ticks: Tick(0) }))
            .expect("configured")
    }

    #[test]
    fn a_node_that_committed_nothing_while_the_window_was_open_is_silent() {
        let mut watch = watching(100);
        watch.knows(NodeId(0));
        watch.knows(NodeId(1));
        watch.committed(NodeId(0));

        watch.healed(Tick(10));
        watch.committed(NodeId(0));
        let found = watch.due(Tick(110)).expect("a window was open");

        assert_eq!(found.nodes, vec![NodeId(1)], "node 0 committed, node 1 did not");
        assert_eq!(found.healed_at, Tick(10));
        assert_eq!(found.span, Tick(100));
    }

    /// Progress is counted from the heal, not from the start of the run: a node that committed
    /// heavily before the partition and nothing after it is silent.
    #[test]
    fn what_happened_before_the_heal_does_not_count_as_progress_after_it() {
        let mut watch = watching(100);
        for _ in 0..50 {
            watch.committed(NodeId(0));
        }
        watch.healed(Tick(10));
        let found = watch.due(Tick(110)).expect("a window was open");

        assert_eq!(found.nodes, vec![NodeId(0)]);
    }

    /// A second heal replaces the first, so the timer the first armed finds nothing to answer.
    #[test]
    fn a_timer_whose_window_was_replaced_fires_into_nothing() {
        let mut watch = watching(100);
        watch.knows(NodeId(0));
        watch.healed(Tick(10));
        watch.healed(Tick(20));

        assert!(watch.due(Tick(110)).is_some(), "the second window answers");
        assert!(watch.due(Tick(120)).is_none(), "and there is nothing left for the first");
    }

    #[test]
    fn a_cluster_that_kept_committing_is_not_silent() {
        let mut watch = watching(100);
        watch.knows(NodeId(0));
        watch.healed(Tick(10));
        watch.committed(NodeId(0));

        assert!(watch.due(Tick(110)).expect("a window was open").nodes.is_empty());
    }

    fn files(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
        pairs.iter().map(|(name, len)| ((*name).to_owned(), *len)).collect()
    }

    fn durability() -> Durability {
        Durability::new(Some(DurabilityConfig)).expect("configured")
    }

    /// The defect this oracle exists for: the node told somebody it had eight records, died before
    /// syncing them, and came back with four.
    #[test]
    fn what_a_node_answered_with_and_came_back_without_is_lost() {
        let mut watch = durability();
        watch.answered(NodeId(0), &files(&[("/store.db", 512)]));
        let lost = watch.restarted(NodeId(0), &files(&[("/store.db", 256)]));

        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].file, "/store.db");
        assert_eq!((lost[0].answered, lost[0].came_back), (512, 256));
    }

    /// A node that synced what it had answered with kept its promise, and a crash afterwards is
    /// not this oracle's business however much it destroys.
    #[test]
    fn what_a_sync_made_good_is_not_lost() {
        let mut watch = durability();
        watch.answered(NodeId(0), &files(&[("/store.db", 512)]));
        watch.committed(NodeId(0), &files(&[("/store.db", 512)]));

        assert!(watch.restarted(NodeId(0), &files(&[("/store.db", 0)])).is_empty());
    }

    /// A sync that covered only part of what was answered leaves the rest owed.
    #[test]
    fn a_sync_that_did_not_reach_the_mark_leaves_it_standing() {
        let mut watch = durability();
        watch.answered(NodeId(0), &files(&[("/store.db", 512)]));
        watch.committed(NodeId(0), &files(&[("/store.db", 256)]));

        assert_eq!(watch.restarted(NodeId(0), &files(&[("/store.db", 256)])).len(), 1);
    }

    /// The mark is the most the node ever implied it had, so a later answer about a shorter file
    /// does not let it off.
    #[test]
    fn a_mark_rises_and_does_not_fall() {
        let mut watch = durability();
        watch.answered(NodeId(0), &files(&[("/store.db", 512)]));
        watch.answered(NodeId(0), &files(&[("/store.db", 128)]));

        assert_eq!(watch.restarted(NodeId(0), &files(&[("/store.db", 256)])).len(), 1);
    }

    /// A node that came back whole is not a finding, and its marks are spent either way: a second
    /// restart must not report the first one's promises again.
    #[test]
    fn a_node_that_came_back_whole_owes_nothing_afterwards() {
        let mut watch = durability();
        watch.answered(NodeId(0), &files(&[("/store.db", 512)]));

        assert!(watch.restarted(NodeId(0), &files(&[("/store.db", 512)])).is_empty());
        assert!(watch.restarted(NodeId(0), &files(&[("/store.db", 0)])).is_empty());
    }

    /// One node's promises are its own.
    #[test]
    fn a_node_is_not_accused_of_what_another_answered() {
        let mut watch = durability();
        watch.answered(NodeId(0), &files(&[("/store.db", 512)]));

        assert!(watch.restarted(NodeId(1), &files(&[("/store.db", 0)])).is_empty());
    }
}
