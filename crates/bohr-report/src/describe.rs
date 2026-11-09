//! What a run recorded, in the words someone who has never used this tool would use.
//!
//! The vocabulary a trace is written in is precise and unhelpful: `AlwaysOrUnreachable`,
//! `NetDeliver`, `PacketFate::Reordered`. Every one of those is the right name for the thing and
//! the wrong name for a reader who arrived from a bug report. This module is the one place that
//! translation happens, so a report and a text dump never explain the same event differently.

use bohr_trace::{
    AssertionKind, DiskOpKind, FaultKind, FaultTarget, NodeState, Outcome, PacketFate,
};

/// One word for what a run amounted to.
#[must_use]
pub const fn outcome_word(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Clean => "clean",
        Outcome::PropertyViolated(_) => "property violated",
        Outcome::GuestCrashed(_) => "guest crashed",
        Outcome::GuestHung(_) => "guest hung",
        Outcome::LivenessViolated(_) => "cluster stopped committing",
        Outcome::DurabilityViolated(_) => "acknowledged write lost",
        Outcome::ProbeViolated(_) => "a node's own answers broke a property",
        Outcome::UnsupportedSyscall(_) => "unsupported syscall",
        Outcome::ImplementationsDisagreed(_) => "implementations disagreed",
        Outcome::InternalError(_) => "internal error",
        Outcome::StoppedOnRequest(_) => "stopped on request",
    }
}

/// What the run's own words about the outcome were, where it had any.
#[must_use]
pub fn outcome_detail(outcome: &Outcome) -> Option<&str> {
    match outcome {
        Outcome::Clean => None,
        Outcome::PropertyViolated(detail)
        | Outcome::GuestCrashed(detail)
        | Outcome::GuestHung(detail)
        | Outcome::LivenessViolated(detail)
        | Outcome::DurabilityViolated(detail)
        | Outcome::ProbeViolated(detail)
        | Outcome::UnsupportedSyscall(detail)
        | Outcome::ImplementationsDisagreed(detail)
        | Outcome::InternalError(detail)
        | Outcome::StoppedOnRequest(detail) => Some(detail),
    }
}

/// What an assertion promises, said rather than named.
///
/// A reader meeting `always_or_unreachable` for the first time has to be told that it is a promise
/// about every time the line is reached and not about the line being reached at all — which is the
/// distinction the name compresses and nobody unpacks correctly on first reading.
#[must_use]
pub const fn assertion_promise(kind: AssertionKind) -> &'static str {
    match kind {
        AssertionKind::Always => "must hold every time it is evaluated",
        AssertionKind::AlwaysOrUnreachable => {
            "must hold whenever it is reached, and need never be reached"
        }
        AssertionKind::Sometimes => "must hold at least once across a campaign",
        AssertionKind::Reachable => "must be reached at least once across a campaign",
        AssertionKind::Unreachable => "must never be reached",
    }
}

/// What a fault did, as a sentence rather than a variant.
#[must_use]
pub const fn fault_action(fault: FaultKind) -> &'static str {
    match fault {
        FaultKind::NetDelay => "a link was slowed",
        FaultKind::NetDrop => "a packet was dropped",
        FaultKind::NetDuplicate => "a packet was duplicated",
        FaultKind::NetReorder => "a packet was reordered",
        FaultKind::NetPartition => "a link was cut",
        FaultKind::DiskTear => "a write was torn by a crash",
        FaultKind::DiskReorder => "unsynced writes were reordered",
        FaultKind::DiskStall => "a disk operation stalled",
        FaultKind::DiskFull => "the disk reported no space",
        FaultKind::DiskError => "the disk reported an error",
        FaultKind::NodeKill => "a node was killed",
        FaultKind::NodeFreeze => "a node was frozen",
        FaultKind::NodeThaw => "a node was let run again",
        FaultKind::NodeRestart => "a node was restarted",
        FaultKind::ClockStep => "a clock was stepped",
        FaultKind::ClockDrift => "a clock was set drifting",
        FaultKind::ChainReorg => "history was rewound and run again",
        FaultKind::MemoryExhausted => "memory ran out",
        FaultKind::NetCorrupt => "a datagram was altered on the wire",
    }
}

/// What a fault was done to.
#[must_use]
pub fn fault_target(target: &FaultTarget) -> String {
    match target {
        FaultTarget::Node(node) => format!("node {}", node.get()),
        FaultTarget::Link { from, to } => format!("the link from {} to {}", from.get(), to.get()),
        FaultTarget::Descriptor(fd) => format!("descriptor {fd}"),
        FaultTarget::World => "the whole world".to_owned(),
    }
}

/// What happened to a packet.
#[must_use]
pub const fn packet_fate(fate: PacketFate) -> &'static str {
    match fate {
        PacketFate::Delivered => "delivered",
        PacketFate::Dropped => "dropped",
        PacketFate::Duplicated => "duplicated",
        PacketFate::Reordered => "reordered",
        PacketFate::Corrupted => "delivered altered",
    }
}

/// What a node is doing.
#[must_use]
pub const fn node_state(state: NodeState) -> &'static str {
    match state {
        NodeState::Running => "running",
        NodeState::Frozen => "frozen",
        NodeState::Dead => "dead",
        NodeState::Restarting => "restarting",
    }
}

/// What a disk call was.
#[must_use]
pub const fn disk_op(op: DiskOpKind) -> &'static str {
    match op {
        DiskOpKind::Read => "read",
        DiskOpKind::Write => "write",
        DiskOpKind::Fsync => "sync",
        DiskOpKind::Truncate => "truncate",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the module: nothing it produces should read like the identifier it came from.
    #[test]
    fn nothing_reads_like_the_variant_it_came_from() {
        let said = [
            outcome_word(&Outcome::PropertyViolated(String::new())),
            assertion_promise(AssertionKind::AlwaysOrUnreachable),
            fault_action(FaultKind::NodeKill),
            packet_fate(PacketFate::Reordered),
            disk_op(DiskOpKind::Fsync),
        ];
        for phrase in said {
            assert!(
                phrase.chars().all(|c| c.is_lowercase() || c == ' ' || c == ','),
                "{phrase} still looks like the identifier it describes"
            );
        }
    }

    #[test]
    fn a_clean_run_has_nothing_to_add() {
        assert_eq!(outcome_detail(&Outcome::Clean), None);
        assert_eq!(outcome_detail(&Outcome::GuestHung("stuck".to_owned())), Some("stuck"));
    }
}
