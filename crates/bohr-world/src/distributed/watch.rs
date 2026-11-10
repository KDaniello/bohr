//! Watching for the things a guest never says about itself.
//!
//! A blockchain client neither asserts nor exits, so it is judged at the boundary: its calls and
//! the moments the world chose.
//!
//! Durability is fed from `NodeDisk::commit`, since a stalled `fsync`'s `DiskOp` marks the stall's
//! start.
//!
//! This states a fact and gives it a code; the oracle decides whether it is a finding.

use std::collections::{BTreeMap, BTreeSet};

use bohr_trace::NodeState;
use bohr_types::{NodeId, Tick};

/// What counts as the cluster making progress.
///
/// Completed syncs; packets do not count, since a stalled cluster still gossips.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LivenessConfig {
    /// How long a silence has to last, in ticks after the last cut lifts.
    pub after_heal_ticks: Tick,
    /// How long a node may make no call at all before it is reported. Zero watches for nothing.
    ///
    /// An idle server still polls; a stopped one makes no calls. The run loop cannot tell them
    /// apart, so this watches. The span is the scenario's.
    pub quiet_ticks: Tick,
}

/// Watches for a cluster that could talk to itself again and went on committing nothing.
///
/// Armed by a heal and answered by a timer, since the finding is an absence.
#[derive(Debug, Default)]
pub(crate) struct Liveness {
    config: LivenessConfig,
    /// Completed syncs per node, counted for the life of the run.
    ///
    /// A counter, so each window measures progress since it opened.
    commits: BTreeMap<NodeId, u64>,
    /// What each node had committed when the window opened, and when that was.
    open: Option<Window>,
    /// When each node was last seen doing anything at all.
    seen: BTreeMap<NodeId, Tick>,
    /// Nodes the world has taken away, which are silent by our doing.
    down: BTreeSet<NodeId>,
    /// Nodes already reported quiet, so a run says it once.
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
    /// Called at first sight, so a node that never syncs counts as silent.
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
    /// A node the world killed or froze is not watched. Watching resumes, timed afresh, when the
    /// node does.
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
    /// A finished program is not watched, like a killed one.
    pub(crate) fn ended(&mut self, node: NodeId) {
        self.down.insert(node);
    }

    /// The nodes that have made no call for the whole span, each named once.
    ///
    /// Never-seen nodes and nodes the world holds down are not reported; see
    /// [`Self::node_became`].
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
    /// Called only when the last cut lifts.
    pub(crate) fn healed(&mut self, now: Tick) -> Tick {
        self.open = Some(Window { healed_at: now, was: self.commits.clone() });
        Tick(now.get().saturating_add(self.config.after_heal_ticks.get()))
    }

    /// Closes the open window and says which nodes committed nothing while it was open.
    ///
    /// `None` if already closed; a second heal replaces the first window.
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
/// The protocol is not parsed, so an answer is an outbound send while the disk is dirty: the
/// `fsyncgate` window.
///
/// It may over-mark an unrelated send, so a finding also needs the data missing afterwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DurabilityConfig;

/// Watches for a node that answered before it synced and came back without the data.
///
/// A mark is a file length, so this catches files that came back shorter: right for append-only
/// logs, blind to stores that rewrite pages in place.
///
/// Content comparison would fire on nearly every correct recovery (`docs/ARCHITECTURE.md` §5.20).
/// Whether an acknowledged write survived is checked through the store's own read path; this
/// watcher contributes [`Self::outstanding`].
#[derive(Debug, Default)]
pub(crate) struct Durability {
    /// The longest each file was when its node last answered somebody with it unsynced.
    ///
    /// A high-water mark per file, nested by node since restarts and commits ask per node.
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
    /// Called at a send with the disk dirty; every unsynced file is marked.
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
    /// Marks at or below the durable length are kept promises.
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
    /// Exposure: it shows whether a crash could cost anything.
    pub(crate) fn outstanding(&self, node: NodeId) -> (u32, u64) {
        let Some(mine) = self.marks.get(&node) else {
            return (0, 0);
        };
        (mine.len() as u32, mine.values().sum())
    }

    /// What a node came back without, measured against what it had answered with.
    ///
    /// Called once the restarted disk is in place; the marks are consumed.
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

    /// Progress counts from the heal.
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

    /// A second heal replaces the first window.
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

    /// Eight records answered, four after the crash.
    #[test]
    fn what_a_node_answered_with_and_came_back_without_is_lost() {
        let mut watch = durability();
        watch.answered(NodeId(0), &files(&[("/store.db", 512)]));
        let lost = watch.restarted(NodeId(0), &files(&[("/store.db", 256)]));

        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].file, "/store.db");
        assert_eq!((lost[0].answered, lost[0].came_back), (512, 256));
    }

    /// A node that synced what it answered is clean.
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

    /// A later, shorter answer does not lower the mark.
    #[test]
    fn a_mark_rises_and_does_not_fall() {
        let mut watch = durability();
        watch.answered(NodeId(0), &files(&[("/store.db", 512)]));
        watch.answered(NodeId(0), &files(&[("/store.db", 128)]));

        assert_eq!(watch.restarted(NodeId(0), &files(&[("/store.db", 256)])).len(), 1);
    }

    /// A whole node is clean, and its marks are spent.
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
