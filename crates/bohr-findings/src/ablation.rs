//! What an ablation found: which faults a finding needed, and how real they are.
//!
//! The trials are run by the command line, which can run a bundle; what they amount to, and which
//! faults landed or were written down, is decided here.

use bohr_trace::{EventKind, FaultGrade, FaultKind, PacketFate};

/// What removing one kind of fault did to the finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Without {
    /// The same failure: the kind was not needed.
    Same,
    /// No failure at all: the kind was needed.
    Clean,
    /// Another failure: the kind was needed for this one.
    Other(String),
}

/// One kind of fault, and what the run did without it.
#[derive(Clone, Debug)]
pub struct Trial {
    /// The kind removed.
    pub kind: FaultKind,
    /// What the run came to without it.
    pub without: Without,
}

/// What an ablation found.
#[derive(Clone, Debug)]
pub struct Ablation {
    /// The finding's signature, which every trial is compared against.
    pub signature: String,
    /// One trial per kind that landed.
    pub trials: Vec<Trial>,
    /// Where no single kind was needed and removing every drawn kind at once makes the failure go:
    /// the kinds of which any one suffices.
    pub alternatives: Vec<FaultKind>,
    /// Kinds that landed and that a scenario wrote down, which an ablation does not remove.
    pub scheduled: Vec<FaultKind>,
}

impl Ablation {
    /// The kinds the finding needed, every one of them.
    pub fn necessary(&self) -> Vec<FaultKind> {
        self.trials.iter().filter(|t| t.without != Without::Same).map(|t| t.kind).collect()
    }

    /// The finding's grade among its drawn faults: the weakest of the kinds it needed all of, or,
    /// where any one of several sufficed, the most real of those.
    pub fn grade(&self) -> Option<FaultGrade> {
        let needed = self.necessary();
        if needed.is_empty() {
            self.alternatives.iter().map(|k| k.grade()).min()
        } else {
            needed.into_iter().map(FaultKind::grade).max()
        }
    }
}

/// The kinds of fault that landed in a run, in the order they first landed.
///
/// A thaw is the end of a freeze and no fault of its own. A datagram dropped, duplicated,
/// reordered or corrupted by a draw lands as the packet's fate rather than as a fired fault, and
/// counts as its kind.
pub fn landed(events: &[bohr_trace::Event]) -> Vec<FaultKind> {
    let mut kinds = Vec::new();
    for event in events {
        let kind = match event.kind {
            EventKind::FaultFired { fault, .. } if fault != FaultKind::NodeThaw => Some(fault),
            EventKind::PacketFate { fate, .. } => match fate {
                PacketFate::Dropped => Some(FaultKind::NetDrop),
                PacketFate::Duplicated => Some(FaultKind::NetDuplicate),
                PacketFate::Reordered => Some(FaultKind::NetReorder),
                PacketFate::Corrupted => Some(FaultKind::NetCorrupt),
                PacketFate::Delivered => None,
            },
            _ => None,
        };
        if let Some(kind) = kind
            && !kinds.contains(&kind)
        {
            kinds.push(kind);
        }
    }
    kinds
}

/// The kinds a scenario writes down, which land whatever the draws say.
pub fn scheduled(loaded: Option<&bohr_world::LoadedConfig>) -> Vec<FaultKind> {
    let Some(loaded) = loaded else {
        return Vec::new();
    };
    let mut kinds = Vec::new();
    if !loaded.network.partitions.is_empty() {
        kinds.push(FaultKind::NetPartition);
    }
    for kind in loaded.node_faults.iter().filter_map(|fault| fault.kind.fired()) {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    kinds
}

/// The kinds a scenario wrote down that fired in the run.
///
/// A fault written down counts only where it fired: one set for a moment after the run ended
/// changed nothing, and a verdict read from it would be read from nothing.
pub fn scheduled_that_fired(
    loaded: Option<&bohr_world::LoadedConfig>,
    fired: &[FaultKind],
) -> Vec<FaultKind> {
    scheduled(loaded).into_iter().filter(|kind| fired.contains(kind)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A restore the scenario set for after the run ended never fired, and is not counted.
    #[test]
    fn a_fault_written_down_counts_only_where_it_fired() {
        let loaded = bohr_world::config::parse(
            "[cluster]\nnodes = 1\n\n[[fault]]\nkind = \"node_restart_from_snapshot\"\nnode = 0\n\
             at_tick = 9\nsnapshot_back = 0\n",
        )
        .expect("a configuration");
        let fired = |kinds: &[FaultKind]| scheduled_that_fired(Some(&loaded), kinds);
        assert!(fired(&[]).is_empty(), "an unfired restore was counted");
        assert_eq!(fired(&[FaultKind::NodeRestart]), vec![FaultKind::NodeRestart]);
    }
}
