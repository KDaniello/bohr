//! Which events a page shows when a run recorded more than a page can hold.
//!
//! A single slice around the violation would be the obvious answer and the wrong one: what a
//! reader needs is the neighbourhood of the failure *and* everything that was done to the run,
//! and the faults are usually nowhere near the end. So the window is a dense range around the
//! point of interest merged with a keep-set of the events that are worth showing wherever they
//! fall — and what is left out is counted and named rather than quietly dropped.

use bohr_trace::{Event, EventKind};

/// Events a page draws before it starts leaving some out.
///
/// A bound on the page rather than on the run: past a couple of thousand chips the timeline stops
/// being something a person reads and becomes something they scroll past, and the file stops being
/// something anybody attaches to an issue.
pub(crate) const MOST_SHOWN: usize = 2000;

/// Events kept before the point of interest, and after it.
///
/// Weighted backwards on purpose: what led to a failure is the question, and what the guest did
/// afterwards is rarely part of the answer.
const BEFORE: usize = 1200;
const AFTER: usize = 300;

/// What a page shows of a run, and what it left out.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Window {
    /// Positions into the run's events, in order.
    shown: Vec<usize>,
    /// How many were left out.
    elided: usize,
    /// How many the run recorded altogether.
    total: usize,
}

impl Window {
    /// The window for a run, centred on an event where there is one.
    pub(crate) fn of(events: &[Event], centre: Option<usize>) -> Self {
        if events.len() <= MOST_SHOWN {
            return Self { shown: (0..events.len()).collect(), elided: 0, total: events.len() };
        }

        let centre = centre.unwrap_or(events.len().saturating_sub(1));
        let from = centre.saturating_sub(BEFORE);
        let to = (centre + AFTER).min(events.len());

        let mut shown: Vec<usize> = (from..to).collect();
        // **The keep-set is bounded too.** It used to take everything worth showing wherever it
        // fell, which is right until a guest asserts in a loop: a hundred thousand assertions are
        // a hundred thousand kept slots, each costing a cell on every node's lane, and the page
        // went far past the size `MOST_BYTES` documents as the largest this crate will produce.
        // What does not fit is counted as elided like anything else, so the page still says so.
        let mut kept: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(at, event)| !(from..to).contains(at) && always_worth_showing(&event.kind))
            .map(|(at, _)| at)
            .collect();
        if kept.len() > MOST_KEPT {
            // The earliest of them, because what a reader is looking for is where a run first
            // went wrong rather than the last of a repeating tail.
            kept.truncate(MOST_KEPT);
        }
        shown.extend(kept);
        shown.sort_unstable();
        shown.dedup();
        Self { elided: events.len() - shown.len(), shown, total: events.len() }
    }

    /// The positions shown, in order.
    pub(crate) fn shown(&self) -> &[usize] {
        &self.shown
    }

    /// How many events the page does not show.
    pub(crate) const fn elided(&self) -> usize {
        self.elided
    }

    /// Whether anything was left out.
    pub(crate) const fn is_partial(&self) -> bool {
        self.elided > 0
    }

    /// How many events are missing immediately before each shown one, and after the last.
    ///
    /// One entry longer than the shown list, so the run's beginning and end are counted as well as
    /// its middle: a timeline that started at the eight-thousandth event with nothing said about
    /// the ones before it would read as a run that began there.
    pub(crate) fn gaps(&self) -> Vec<usize> {
        let mut missing = Vec::with_capacity(self.shown.len() + 1);
        let mut previous: Option<usize> = None;
        for at in &self.shown {
            missing.push(previous.map_or(*at, |before| at - before - 1));
            previous = Some(*at);
        }
        missing.push(previous.map_or(self.total, |last| self.total - last - 1));
        missing
    }
}

/// The most events the keep-set may add on top of the slice around the failure.
///
/// A bound rather than a preference: every kept slot is drawn on every node's lane, so the page
/// grows with the product of the two, and a guest that asserts inside a loop is not unusual. Wide
/// enough that no ordinary run reaches it, and the page says how many it left out either way.
const MOST_KEPT: usize = 2_000;

/// Whether an event is shown wherever it falls.
///
/// Everything the world did to the run, and everything the guest said about itself. These are the
/// events a reader is looking for, and they are the ones a slice around a failure misses: a
/// partition raised at the start of a long run is nowhere near the end of it.
const fn always_worth_showing(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::FaultFired { .. }
            | EventKind::NodeState { .. }
            | EventKind::Assertion { .. }
            | EventKind::Diagnostic { .. }
            | EventKind::TaskSpawned { .. }
            | EventKind::TaskExited { .. }
    )
}

#[cfg(test)]
mod tests {
    use bohr_trace::{AssertionKind, FaultKind, FaultTarget};
    use bohr_types::{EventId, NodeId, TaskId, Tick};

    use super::*;

    fn event(id: u64, kind: EventKind) -> Event {
        Event { id: EventId(id), tick: Tick(id), node: NodeId(0), task: TaskId(0), kind }
    }

    fn syscall(id: u64) -> Event {
        event(
            id,
            EventKind::Syscall { nr: 1, args: [0; 6], ret: 0, emulated: true, data_digest: None },
        )
    }

    #[test]
    fn a_short_run_is_shown_whole() {
        let events: Vec<Event> = (0..50).map(syscall).collect();
        let window = Window::of(&events, None);
        assert_eq!(window.shown().len(), 50);
        assert!(!window.is_partial());
        assert!(window.gaps().iter().all(|missing| *missing == 0), "a whole run has no holes");
    }

    /// The reason the keep-set exists: a fault at the start of a long run is nowhere near the
    /// failure at the end, and a slice would show a reader neither the cause nor that there was
    /// one.
    #[test]
    fn what_the_world_did_is_shown_wherever_it_happened() {
        let mut events: Vec<Event> = (0..10_000).map(syscall).collect();
        events[7] = event(
            7,
            EventKind::FaultFired {
                fault: FaultKind::NetPartition,
                target: FaultTarget::Node(NodeId(1)),
            },
        );
        events[9_998] = event(
            9_998,
            EventKind::Assertion {
                name: "log is monotonic".to_owned(),
                kind: AssertionKind::Always,
                hit: true,
                condition: false,
            },
        );

        let window = Window::of(&events, Some(9_998));
        assert!(window.is_partial());
        assert!(window.shown().contains(&7), "the fault that caused it was left out");
        assert!(window.shown().contains(&9_998), "the failure itself was left out");
        assert!(window.shown().len() <= MOST_SHOWN + 64, "shown {}", window.shown().len());
    }

    /// What is left out is counted and locatable, because a hole drawn as nothing reads as a run
    /// in which nothing happened.
    #[test]
    fn what_is_left_out_is_counted_and_placed() {
        let events: Vec<Event> = (0..10_000).map(syscall).collect();
        let window = Window::of(&events, Some(9_000));
        assert_eq!(window.elided(), 10_000 - window.shown().len());

        let gaps = window.gaps();
        assert_eq!(gaps.len(), window.shown().len() + 1, "a gap is owed on each side as well");
        let counted: usize = gaps.iter().sum();
        assert_eq!(counted, window.elided(), "the gaps and the count disagree");
        assert!(gaps[0] > 0, "the events before the first one shown were not counted");
    }

    /// With nothing to centre on, the end of the run is what a reader wants: that is where a
    /// crash or a hang left off.
    #[test]
    fn a_run_with_no_point_of_interest_shows_its_end() {
        let events: Vec<Event> = (0..10_000).map(syscall).collect();
        let window = Window::of(&events, None);
        assert!(window.shown().contains(&9_999), "the end of the run was left out");
    }
}
