//! What made a run a finding, in a form two runs can be compared by.
//!
//! Reduction is a search for the smallest scenario that still goes wrong *in the same way*, so
//! everything turns on what "the same way" means. The obvious answer — compare the outcome — does
//! not work here, and the reasons are worth stating because they are not obvious either:
//!
//! - a violated property carries the words the world model wrote, which name the promise and the
//!   line it is written on, and that is exactly the right identity;
//! - a hang carries the tick and the event count the run gave up at, which no two candidates reach,
//!   so comparing the message would reject every reduction that worked;
//! - a crash carries a signal or an exit status, which is stable and worth comparing.
//!
//! So the identity is the outcome's *shape* plus a signature drawn from what the run recorded,
//! and each shape has its own notion of what recording it means.

use std::collections::BTreeSet;

use bohr_kernel::StopReason;
use bohr_trace::{DURABILITY_CODE, Event, EventKind, ExitStatus, LIVENESS_CODE, Outcome};
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
/// Located as well as named, because two assertions can carry the same message and differ only in
/// where they are written — and reducing one into the other is a false positive nobody can see.
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
    /// By node and status, never by task: a reduced run spawns fewer tasks and numbers them
    /// differently, so a task identifier is a fact about the run rather than about the defect.
    Exits(BTreeSet<(NodeId, String)>),
    /// Why the loop gave up, and never the tick it gave up at.
    NoProgress(StopReason),
    /// The values the implementations did not agree on.
    ///
    /// The labels and never the digests: two candidates reach different values by construction —
    /// that is what makes them different runs — so a signature carrying what they came to would
    /// reject every reduction that worked. What may not change is *which promise* was broken.
    Disagreement(BTreeSet<String>),
    /// The nodes that lost an acknowledged write, and the file each lost it from.
    ///
    /// The file by name and never how many bytes went missing: a smaller scenario loses a
    /// different amount from the same file, and a signature carrying the count would match
    /// nothing after the first move that worked.
    Lost(BTreeSet<(NodeId, String)>),
    /// The nodes that committed nothing while the window was open.
    ///
    /// Which nodes, never how long the window was or when it opened: a reduction brings moments
    /// forward, so every candidate's window opens somewhere else, and a signature carrying the
    /// tick would match nothing after the first successful move.
    Silent(BTreeSet<NodeId>),
    /// The call this build does not model, by name.
    ///
    /// Named rather than left as the class alone: two runs that stop at two different unmodelled
    /// calls are two findings, and a reduction told they were one walked away from the call it
    /// had been given towards whichever the smaller scenario happened to reach.
    Refused(String),
    /// A shape with nothing finer to say about itself.
    ClassAlone,
}

impl Signature {
    /// Whether this says nothing that would tell one finding from another of its class.
    ///
    /// `ClassAlone` is not empty: it is a shape that has nothing finer to say, and every candidate
    /// of that class genuinely is the same finding. An empty *set* is different -- it means the
    /// run went wrong and left nothing behind that says how.
    fn is_empty(&self) -> bool {
        match self {
            Self::Assertions(held) => held.is_empty(),
            Self::Exits(held) | Self::Lost(held) => held.is_empty(),
            Self::Disagreement(held) => held.is_empty(),
            Self::Silent(held) => held.is_empty(),
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
            // Read from the outcome's own words rather than from the timeline, because a
            // disagreement is not something that happened at an event: it is what several event
            // streams came to when they were compared, and no one of them carries it.
            Class::ImplementationsDisagreed => Signature::Disagreement(disagreed_labels(outcome)),
            // The call that could not be modelled, by name. `ClassAlone` made every unmodelled
            // call the same finding, so a candidate that ran into a *different* one counted as
            // having reproduced this one and the reduction walked away from what it was shrinking.
            Class::UnsupportedSyscall => Signature::Refused(refused_call(outcome)),
            _ => Signature::ClassAlone,
        };
        // **An empty signature is matched by everything of its class.** `is_subset` on an empty
        // set is true of any candidate, so a crash whose events carried no bad exit -- a truncated
        // log -- reduced towards any crash at all, and the scenario that came out reproduced
        // something else. Refused here, the way a clean baseline is refused one layer up: a
        // finding this build cannot tell apart from its neighbours is not one it can shrink.
        if signature.is_empty() {
            return None;
        }
        Some(Self { class, signature })
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

    /// Whether `candidate` still reproduces this.
    ///
    /// Containment rather than equality: a smaller scenario that also trips a second assertion is
    /// still a reproducer for the first, and demanding equality would throw away reductions that
    /// worked. What may not change is the shape and the promise named.
    #[must_use]
    pub fn reproduced_by(&self, candidate: &Self) -> bool {
        if self.class != candidate.class {
            return false;
        }
        match (&self.signature, &candidate.signature) {
            (Signature::Assertions(mine), Signature::Assertions(theirs)) => mine.is_subset(theirs),
            (Signature::Exits(mine), Signature::Exits(theirs)) => mine.is_subset(theirs),
            (Signature::NoProgress(mine), Signature::NoProgress(theirs)) => mine == theirs,
            (Signature::Silent(mine), Signature::Silent(theirs)) => mine.is_subset(theirs),
            (Signature::Lost(mine), Signature::Lost(theirs)) => mine.is_subset(theirs),
            (Signature::Disagreement(mine), Signature::Disagreement(theirs)) => {
                mine.is_subset(theirs)
            }
            (Signature::Refused(mine), Signature::Refused(theirs)) => mine == theirs,
            (Signature::ClassAlone, Signature::ClassAlone) => true,
            _ => false,
        }
    }
}

/// Every promise the timeline says did not hold.
///
/// Read from the events rather than from the stored records: the events are what a run always has,
/// and the model already put the assertion's own words there. The location comes from the message
/// the diagnostic carries, which is the only place a source line survives into the timeline.
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
/// The model writes `always("name") failed at file:line`, which is the only form the location
/// reaches the timeline in. Parsed rather than re-derived because the alternative is reading the
/// stored record section, and a reduction has to work on a bundle whose guest had no SDK at all.
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
/// Read from the timeline, like everything else here, and never from the stored records: a
/// reduction has to work on a bundle whose guest had no SDK at all, which is the entire point of
/// these oracles.
fn silent_nodes(events: &[Event]) -> BTreeSet<NodeId> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic { code, .. } if code == LIVENESS_CODE => Some(event.node),
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
/// The name is parsed out of the message, which is the only place it reaches the timeline — the
/// same technique the assertion locations use, and for the same reason: a reduction works on a
/// bundle whose guest had no SDK, so the stored record section is not available to it.
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

    /// The identity is the promise and where it is written, never the outcome's message: that
    /// message is one constant for every property in existence.
    #[test]
    fn the_same_promise_from_two_runs_is_one_violation() {
        let first = violated(&broke("log is monotonic", "src/log.rs:214"));
        let again = violated(&broke("log is monotonic", "src/log.rs:214"));
        assert!(first.reproduced_by(&again));
        assert_eq!(first.failed().len(), 1);
        assert_eq!(first.failed()[0].at.as_deref(), Some("src/log.rs:214"));
    }

    /// Two assertions can share a message and differ only in the line. Reducing one into the other
    /// is a false positive nobody can see by reading the result.
    #[test]
    fn one_promise_does_not_stand_in_for_another_written_elsewhere() {
        let here = violated(&broke("log is monotonic", "src/log.rs:214"));
        let there = violated(&broke("log is monotonic", "src/index.rs:9"));
        assert!(!here.reproduced_by(&there), "a promise from another file was accepted");
    }

    /// A smaller scenario that also trips something else still reproduces what was asked for.
    #[test]
    fn a_candidate_that_breaks_more_still_reproduces_what_was_asked_for() {
        let asked = violated(&broke("log is monotonic", "src/log.rs:214"));
        let mut more = broke("log is monotonic", "src/log.rs:214");
        more.extend(broke("index agrees with log", "src/index.rs:9"));
        assert!(asked.reproduced_by(&violated(&more)));
    }

    /// A silence names the nodes that were silent, and a smaller scenario that silences more of
    /// them still reproduces it — the same containment an exit set has, and for the same reason.
    #[test]
    fn a_silence_is_identified_by_who_was_silent() {
        let one = quiet(&[NodeId(1)]);
        let both = quiet(&[NodeId(1), NodeId(2)]);
        assert!(one.reproduced_by(&both), "silencing more still silences node 1");
        assert!(!both.reproduced_by(&one), "and silencing fewer does not");
    }

    /// The reason this outcome exists rather than reusing `PropertyViolated`.
    ///
    /// A guest with no SDK fails no assertions, so a liveness finding filed under that class would
    /// carry the empty set — and the empty set is a subset of every other, so any property
    /// violation whatsoever would count as reproducing it. A reduction would accept the first
    /// candidate that broke anything at all and report it as the same bug.
    #[test]
    fn a_silence_is_not_reproduced_by_something_else_going_wrong() {
        let silence = quiet(&[NodeId(1)]);
        let promise = violated(&broke("log is monotonic", "src/log.rs:214"));
        assert!(!silence.reproduced_by(&promise));
        assert!(!promise.reproduced_by(&silence));
    }

    /// A disagreement is identified by which promises were broken, and a candidate that breaks
    /// more still reproduces the one that was asked for.
    #[test]
    fn a_disagreement_is_identified_by_what_was_disagreed_about() {
        let root = disagreed("state-root");
        let both = disagreed("state-root, receipts-root");

        assert!(root.reproduced_by(&both), "differing about more still differs about the root");
        assert!(!both.reproduced_by(&root), "and differing about less does not");
    }

    /// Not by what they came to. Two candidates reach different values by construction — that is
    /// what makes them different runs — so a signature carrying the values would reject every
    /// reduction that worked.
    #[test]
    fn a_disagreement_is_not_identified_by_what_they_came_to() {
        assert_eq!(disagreed("state-root"), disagreed("state-root"));
    }

    /// A disagreement about something else is a different bug, and reduction must not accept one
    /// for the other.
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

    /// A run that goes wrong differently is not a reproduction, however small it is.
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

    /// A hang's own message names the tick it gave up at, which no two candidates reach. The
    /// identity is why the loop stopped and nothing else.
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
