//! What made a run a finding, in a form two runs can be compared by.
//!
//! A reduction keeps runs that go wrong the same way. Outcome messages cannot be compared: a hang's
//! message names a tick no two candidates share.
//!
//! The identity is the outcome's class plus a signature from what the run recorded: the promise and
//! its line for a property, the node and status for a crash, the reason for a hang.

use std::collections::BTreeSet;

use bohr_kernel::StopReason;
use bohr_trace::{
    DURABILITY_CODE, Event, EventKind, ExitStatus, LIVENESS_CODE, Outcome, PROBE_CODES,
};
use bohr_types::NodeId;

/// The kind of thing that went wrong, with its message discarded.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Class {
    /// A promise the guest made about itself did not hold.
    PropertyViolated,
    /// A task died.
    GuestCrashed,
    /// The run did not get anywhere.
    GuestHung,
    /// A cluster that could talk to itself again went on committing nothing.
    LivenessViolated,
    /// A node answered while what it had written was not durable, and came back without it.
    DurabilityViolated,
    /// A probe's answers broke the property it watched.
    ProbeViolated,
    /// The guest asked for something the tool does not model.
    UnsupportedSyscall,
    /// Two implementations of one specification disagreed.
    ImplementationsDisagreed,
    /// The tool reached a state it does not define behaviour for.
    InternalError,
    /// Somebody stopped the run to look at it.
    StoppedOnRequest,
}

/// One promise a guest made about itself and did not keep.
///
/// Named and located, since two assertions can share a message.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct FailedAssertion {
    /// What the guest called it.
    pub name: String,
    /// Where it is written, where the run's records said so.
    pub at: Option<String>,
}

/// What a run recorded about the way it went wrong.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Signature {
    /// The promises that did not hold.
    Assertions(BTreeSet<FailedAssertion>),
    /// The nodes whose tasks ended badly, and how.
    ///
    /// By node and status; task ids change between runs.
    Exits(BTreeSet<(NodeId, String)>),
    /// Why the loop gave up, and never the tick it gave up at.
    NoProgress(StopReason),
    /// The values the implementations did not agree on.
    ///
    /// The labels, never the values, which differ between candidates.
    Disagreement(BTreeSet<String>),
    /// The nodes that lost an acknowledged write, and the file each lost it from.
    ///
    /// By file name; the amount lost differs between candidates.
    Lost(BTreeSet<(NodeId, String)>),
    /// The nodes that committed nothing while the window was open.
    ///
    /// Which nodes; the window's timing moves as the reduction brings moments forward.
    Silent(BTreeSet<NodeId>),
    /// The properties a probe saw broken, by code, and the node each was seen on.
    ///
    /// Never heights or hashes, which differ between candidates.
    Probed(BTreeSet<(String, NodeId)>),
    /// The call this build does not model, by name.
    ///
    /// Named, since different unmodelled calls are different findings.
    Refused(String),
    /// A shape with nothing finer to say about itself.
    ClassAlone,
}

impl Signature {
    /// Whether this says nothing that would tell one finding from another of its class.
    ///
    /// `ClassAlone` is a complete identity. An empty set means the run left nothing that says how
    /// it went wrong.
    fn is_empty(&self) -> bool {
        match self {
            Self::Assertions(held) => held.is_empty(),
            Self::Exits(held) | Self::Lost(held) => held.is_empty(),
            Self::Disagreement(held) => held.is_empty(),
            Self::Silent(held) => held.is_empty(),
            Self::Probed(held) => held.is_empty(),
            Self::Refused(name) => name.is_empty(),
            Self::NoProgress(_) | Self::ClassAlone => false,
        }
    }
}

/// The call a refusal named, out of the words the outcome carries.
fn refused_call(outcome: &Outcome) -> String {
    match outcome {
        Outcome::UnsupportedSyscall(said) => said.clone(),
        _ => String::new(),
    }
}

/// What made a run a finding.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Violation {
    class: Class,
    signature: Signature,
}

impl Violation {
    /// What a completed run amounted to, or nothing where it was clean.
    #[must_use]
    pub fn of(outcome: &Outcome, stop_reason: StopReason, events: &[Event]) -> Option<Self> {
        let class = match outcome {
            Outcome::Clean => return None,
            Outcome::PropertyViolated(_) => Class::PropertyViolated,
            Outcome::GuestCrashed(_) => Class::GuestCrashed,
            Outcome::GuestHung(_) => Class::GuestHung,
            Outcome::LivenessViolated(_) => Class::LivenessViolated,
            Outcome::DurabilityViolated(_) => Class::DurabilityViolated,
            Outcome::ProbeViolated(_) => Class::ProbeViolated,
            Outcome::UnsupportedSyscall(_) => Class::UnsupportedSyscall,
            Outcome::ImplementationsDisagreed(_) => Class::ImplementationsDisagreed,
            Outcome::InternalError(_) => Class::InternalError,
            Outcome::StoppedOnRequest(_) => Class::StoppedOnRequest,
        };
        let signature = match class {
            Class::PropertyViolated => Signature::Assertions(failed_assertions(events)),
            Class::GuestCrashed => Signature::Exits(bad_exits(events)),
            Class::GuestHung => Signature::NoProgress(stop_reason),
            Class::LivenessViolated => Signature::Silent(silent_nodes(events)),
            Class::DurabilityViolated => Signature::Lost(lost_writes(events)),
            Class::ProbeViolated => Signature::Probed(probed_codes(events)),
            // Read from the outcome, since no single event stream records a disagreement.
            Class::ImplementationsDisagreed => Signature::Disagreement(disagreed_labels(outcome)),
            // The unmodelled call, by name.
            Class::UnsupportedSyscall => Signature::Refused(refused_call(outcome)),
            _ => Signature::ClassAlone,
        };
        // An empty signature cannot tell findings apart, so it is refused like a clean baseline.
        if signature.is_empty() {
            return None;
        }
        Some(Self { class, signature })
    }

    /// The violation as one line: its class and what tells it from others of its class.
    ///
    /// `bohr findings` groups by this line.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{:?} {:?}", self.class, self.signature)
    }

    /// What kind of thing went wrong.
    #[must_use]
    pub const fn class(&self) -> Class {
        self.class
    }

    /// The promises that did not hold, where that is what went wrong.
    #[must_use]
    pub fn failed(&self) -> Vec<&FailedAssertion> {
        match &self.signature {
            Signature::Assertions(failed) => failed.iter().collect(),
            _ => Vec::new(),
        }
    }

    /// Whether `candidate` still reproduces this: the same class and the same signature.
    ///
    /// The signatures must be equal. A run that also trips a second assertion belongs to another
    /// group in `bohr findings`.
    #[must_use]
    pub fn reproduced_by(&self, candidate: &Self) -> bool {
        self.class == candidate.class && self.signature == candidate.signature
    }
}

/// Every promise the timeline says did not hold.
///
/// Read from the events, which every run has. The location comes from the diagnostic message.
fn failed_assertions(events: &[Event]) -> BTreeSet<FailedAssertion> {
    let located = located_by_name(events);
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Assertion { name, condition: false, .. } => Some(FailedAssertion {
                name: name.clone(),
                at: located.iter().find(|(who, _)| who == name).map(|(_, at)| at.clone()),
            }),
            _ => None,
        })
        .collect()
}

/// Source locations, by the promise they belong to, read out of the diagnostics.
///
/// Parsed from `always("name") failed at file:line`, so bundles without an SDK record work.
fn located_by_name(events: &[Event]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic { message, .. } => {
                let (promise, at) = message.rsplit_once(" failed at ")?;
                let name = promise.split_once('"')?.1.rsplit_once('"')?.0;
                Some((name.to_owned(), at.to_owned()))
            }
            _ => None,
        })
        .collect()
}

/// The nodes the world said committed nothing while a liveness window was open.
///
/// Read from the timeline, so guests without the SDK work.
fn silent_nodes(events: &[Event]) -> BTreeSet<NodeId> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic { code, .. } if code == LIVENESS_CODE => Some(event.node),
            _ => None,
        })
        .collect()
}

/// The properties a probe saw broken, by code and node.
fn probed_codes(events: &[Event]) -> BTreeSet<(String, NodeId)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic { code, .. } if PROBE_CODES.contains(&code.as_str()) => {
                Some((code.clone(), event.node))
            }
            _ => None,
        })
        .collect()
}

/// The projections a group oracle said the implementations differed on.
fn disagreed_labels(outcome: &Outcome) -> BTreeSet<String> {
    match outcome {
        Outcome::ImplementationsDisagreed(labels) => {
            labels.split(", ").filter(|l| !l.is_empty()).map(str::to_owned).collect()
        }
        _ => BTreeSet::new(),
    }
}

/// The nodes that lost an acknowledged write, and the file each lost it from.
///
/// The file name is parsed from the message, as assertion locations are.
fn lost_writes(events: &[Event]) -> BTreeSet<(NodeId, String)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic { code, message } if code == DURABILITY_CODE => {
                let (_, rest) = message.split_once(" bytes of ")?;
                let (file, _) = rest.split_once(" and came back")?;
                Some((event.node, file.to_owned()))
            }
            _ => None,
        })
        .collect()
}

/// The nodes whose tasks ended badly, and how each ended.
fn bad_exits(events: &[Event]) -> BTreeSet<(NodeId, String)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::TaskExited { status: ExitStatus::Signalled(signal) } => {
                Some((event.node, format!("signal {signal}")))
            }
            EventKind::TaskExited { status: ExitStatus::Exited(code) } if *code != 0 => {
                Some((event.node, format!("status {code}")))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use bohr_trace::AssertionKind;
    use bohr_types::{EventId, TaskId, Tick};

    use super::*;

    fn event(id: u64, kind: EventKind) -> Event {
        Event { id: EventId(id), tick: Tick(id), node: NodeId(0), task: TaskId(0), kind }
    }

    fn broke(name: &str, at: &str) -> Vec<Event> {
        vec![
            event(
                1,
                EventKind::Assertion {
                    name: name.to_owned(),
                    kind: AssertionKind::Always,
                    hit: true,
                    condition: false,
                },
            ),
            event(
                2,
                EventKind::Diagnostic {
                    code: "BOHR-2012".to_owned(),
                    message: format!("always(\"{name}\") failed at {at}"),
                },
            ),
        ]
    }

    /// A liveness finding naming the nodes the world reported silent.
    fn quiet(nodes: &[NodeId]) -> Violation {
        let events: Vec<Event> = nodes
            .iter()
            .enumerate()
            .map(|(i, node)| Event {
                id: EventId(i as u64 + 1),
                tick: Tick(1),
                node: *node,
                task: TaskId(0),
                kind: EventKind::Diagnostic {
                    code: LIVENESS_CODE.to_owned(),
                    message: "committed nothing after the heal".to_owned(),
                },
            })
            .collect();
        Violation::of(
            &Outcome::LivenessViolated("quiet".to_owned()),
            StopReason::HorizonReached,
            &events,
        )
        .expect("a finding")
    }

    fn violated(events: &[Event]) -> Violation {
        Violation::of(
            &Outcome::PropertyViolated("whatever".to_owned()),
            StopReason::GuestFinished,
            events,
        )
        .expect("a finding")
    }

    /// The identity is the promise and its location.
    #[test]
    fn the_same_promise_from_two_runs_is_one_violation() {
        let first = violated(&broke("log is monotonic", "src/log.rs:214"));
        let again = violated(&broke("log is monotonic", "src/log.rs:214"));
        assert!(first.reproduced_by(&again));
        assert_eq!(first.failed().len(), 1);
        assert_eq!(first.failed()[0].at.as_deref(), Some("src/log.rs:214"));
    }

    /// Two assertions with one message differ by line.
    #[test]
    fn one_promise_does_not_stand_in_for_another_written_elsewhere() {
        let here = violated(&broke("log is monotonic", "src/log.rs:214"));
        let there = violated(&broke("log is monotonic", "src/index.rs:9"));
        assert!(!here.reproduced_by(&there), "a promise from another file was accepted");
    }

    /// A candidate that also trips something else does not reproduce.
    #[test]
    fn a_candidate_that_breaks_more_is_a_different_finding() {
        let asked = violated(&broke("log is monotonic", "src/log.rs:214"));
        let mut more = broke("log is monotonic", "src/log.rs:214");
        more.extend(broke("index agrees with log", "src/index.rs:9"));
        assert!(!asked.reproduced_by(&violated(&more)));
    }

    /// A silence is identified by exactly who was silent, in both directions.
    #[test]
    fn a_silence_is_identified_by_who_was_silent() {
        let one = quiet(&[NodeId(1)]);
        let both = quiet(&[NodeId(1), NodeId(2)]);
        assert!(one.reproduced_by(&quiet(&[NodeId(1)])));
        assert!(!one.reproduced_by(&both), "silencing more is another finding");
        assert!(!both.reproduced_by(&one), "and silencing fewer is too");
    }

    /// A liveness finding is not a property violation.
    ///
    /// Filed as one, it would carry the empty assertion set and any violation would reproduce it.
    #[test]
    fn a_silence_is_not_reproduced_by_something_else_going_wrong() {
        let silence = quiet(&[NodeId(1)]);
        let promise = violated(&broke("log is monotonic", "src/log.rs:214"));
        assert!(!silence.reproduced_by(&promise));
        assert!(!promise.reproduced_by(&silence));
    }

    /// A disagreement is identified by exactly which promises were broken.
    #[test]
    fn a_disagreement_is_identified_by_what_was_disagreed_about() {
        let root = disagreed("state-root");
        let both = disagreed("state-root, receipts-root");

        assert!(root.reproduced_by(&disagreed("state-root")));
        assert!(!root.reproduced_by(&both), "differing about more is another finding");
        assert!(!both.reproduced_by(&root), "and differing about less is too");
    }

    /// Values differ between candidates and are ignored.
    #[test]
    fn a_disagreement_is_not_identified_by_what_they_came_to() {
        assert_eq!(disagreed("state-root"), disagreed("state-root"));
    }

    /// A disagreement about something else is a different bug.
    #[test]
    fn disagreeing_about_something_else_is_not_a_reproduction() {
        let root = disagreed("state-root");
        let height = disagreed("height");
        assert!(!root.reproduced_by(&height));
        assert!(!height.reproduced_by(&root));
    }

    /// A finding naming the projections the implementations differed on.
    fn disagreed(labels: &str) -> Violation {
        Violation::of(
            &Outcome::ImplementationsDisagreed(labels.to_owned()),
            StopReason::GuestFinished,
            &[],
        )
        .expect("a finding")
    }

    /// A run that goes wrong differently does not reproduce.
    #[test]
    fn a_different_kind_of_wrong_is_not_a_reproduction() {
        let promise = violated(&broke("log is monotonic", "src/log.rs:214"));
        let crashed = Violation::of(
            &Outcome::GuestCrashed("killed by signal 9".to_owned()),
            StopReason::GuestFinished,
            &[event(1, EventKind::TaskExited { status: ExitStatus::Signalled(9) })],
        )
        .expect("a finding");
        assert!(!promise.reproduced_by(&crashed));
        assert!(!crashed.reproduced_by(&promise));
    }

    /// A hang is identified by why the loop stopped, never by its tick.
    #[test]
    fn a_hang_is_compared_by_why_and_never_by_when() {
        let one = Violation::of(
            &Outcome::GuestHung("no progress by tick 900".to_owned()),
            StopReason::TickBudget,
            &[],
        )
        .expect("a finding");
        let other = Violation::of(
            &Outcome::GuestHung("no progress by tick 40000000".to_owned()),
            StopReason::TickBudget,
            &[],
        )
        .expect("a finding");
        assert!(one.reproduced_by(&other), "two hangs of one cause were told apart by their ticks");

        let deadlocked = Violation::of(
            &Outcome::GuestHung("every task is waiting".to_owned()),
            StopReason::Deadlock,
            &[],
        )
        .expect("a finding");
        assert!(!one.reproduced_by(&deadlocked), "a budget and a deadlock are not one hang");
    }

    #[test]
    fn a_clean_run_is_not_a_violation() {
        assert!(Violation::of(&Outcome::Clean, StopReason::GuestFinished, &[]).is_none());
    }
}
