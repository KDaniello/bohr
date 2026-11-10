//! Which events a page shows when a run recorded more than a page can hold.
//!
//! A dense range around the point of interest, merged with a keep-set of events worth showing
//! wherever they fall, such as faults far from the end. What is left out is counted.

use bohr_trace::{Event, EventKind};

/// Events a page draws before it starts leaving some out.
///
/// Past a couple of thousand chips the timeline stops being readable.
pub(crate) const MOST_SHOWN: usize = 2000;

/// Events kept before the point of interest, and after it.
///
/// Weighted backwards, toward what led to the failure.
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
        // The keep-set is bounded too, so an assertion in a loop cannot push the page past
        // `MOST_BYTES`. What does not fit is counted as elided.
        let mut kept: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(at, event)| !(from..to).contains(at) && always_worth_showing(&event.kind))
            .map(|(at, _)| at)
            .collect();
        if kept.len() > MOST_KEPT {
            // The earliest, where the run first went wrong.
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
    /// One entry longer than the shown list, so gaps at both ends are counted.
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
/// Every kept slot is drawn on every lane. No ordinary run reaches the bound.
const MOST_KEPT: usize = 2_000;

/// Whether an event is shown wherever it falls.
///
/// Everything the world did to the run and everything the guest said about itself.
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

    /// A fault at the start of a long run is shown with the failure at its end.
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

    /// What is left out is counted and located.
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

    /// With nothing to centre on, the window shows the end of the run.
    #[test]
    fn a_run_with_no_point_of_interest_shows_its_end() {
        let events: Vec<Event> = (0..10_000).map(syscall).collect();
        let window = Window::of(&events, None);
        assert!(window.shown().contains(&9_999), "the end of the run was left out");
    }
}
