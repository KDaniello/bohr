//! Logical time.

use std::collections::BTreeMap;

use bohr_types::{NodeId, Tick};

/// Parts per million a node's clock may run fast or slow by.
///
/// Wall clocks on separate machines never agree, and software that assumes they do fails in ways
/// that only appear in production. The rate is bounded so a run stays comprehensible: a node an
/// hour ahead is a different test from a node a microsecond ahead, and both are reachable.
pub const MAX_DRIFT_PPM: u32 = 200;

/// How one node sees the world clock.
///
/// The offset is how far ahead of the world clock this node reads, and it is never negative: what
/// matters is that two nodes disagree, and a clock that could read before the run began would have
/// to be clamped, which would make it stand still at the start instead of running.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NodeClock {
    offset: u64,
    /// Rate difference in parts per million, positive for a clock that runs fast.
    drift_ppm: i32,
}

impl NodeClock {
    /// A clock reading `offset` ticks ahead and running `drift_ppm` parts per million fast.
    ///
    /// # Panics
    ///
    /// Panics on a rate outside ±[`MAX_DRIFT_PPM`]. Below -1 000 000 a node's clock would read
    /// earlier as the world moves on, so the bound is checked here rather than trusted to callers.
    #[must_use]
    pub const fn new(offset: u64, drift_ppm: i32) -> Self {
        assert_drift_in_bounds(drift_ppm);
        Self { offset, drift_ppm }
    }

    /// The same reading at `at`, drifting at `drift_ppm` from there on.
    ///
    /// Folding the accumulated part into the offset is what keeps a rate change from rewriting the
    /// past: readings before `at` stay exactly what they were, which a replayed trace depends on.
    /// Where the fold would need a negative offset the clock reads the drifted value instead —
    /// forward, never back.
    ///
    /// # Panics
    ///
    /// Panics on a rate outside ±[`MAX_DRIFT_PPM`], for the reason [`Self::new`] does.
    #[must_use]
    pub fn rebased(self, at: Tick, drift_ppm: i32) -> Self {
        assert_drift_in_bounds(drift_ppm);
        let reading = i128::from(self.read(at).get());
        let elapsed = i128::from(at.get());
        let drifted = elapsed + (elapsed * i128::from(drift_ppm)) / 1_000_000;
        let offset = u64::try_from((reading - drifted).max(0)).unwrap_or(u64::MAX);
        Self { offset, drift_ppm }
    }

    /// What this node reads when the world clock says `now`.
    ///
    /// Drift accumulates with elapsed time, which is what makes two nodes diverge the longer a run
    /// goes on rather than sitting at a fixed distance apart.
    #[must_use]
    pub const fn read(self, now: Tick) -> Tick {
        let elapsed = now.get() as i128;
        let drifted = elapsed + (elapsed * self.drift_ppm as i128) / 1_000_000;
        let shifted = drifted + self.offset as i128;
        if shifted <= 0 {
            Tick::ZERO
        } else if shifted > u64::MAX as i128 {
            Tick(u64::MAX)
        } else {
            Tick(shifted as u64)
        }
    }
}

/// The one check every way of making a [`NodeClock`] goes through.
const fn assert_drift_in_bounds(drift_ppm: i32) {
    assert!(
        drift_ppm >= -(MAX_DRIFT_PPM as i32) && drift_ppm <= MAX_DRIFT_PPM as i32,
        "a node's clock may not drift further than MAX_DRIFT_PPM"
    );
}

/// Virtual time for a run.
///
/// Advances only when something in the run says it should. No host clock is ever consulted, so a
/// guest that sleeps for an hour wakes instantly and a run takes the same virtual time on a busy
/// machine as on an idle one.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Clock {
    now: Tick,
    /// How each node sees the world clock. A node with no entry reads the world clock itself.
    nodes: BTreeMap<NodeId, NodeClock>,
}

impl Clock {
    /// A clock at zero, with every node reading it directly.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The world time, which is what the run is ordered by.
    #[inline]
    #[must_use]
    pub const fn now(&self) -> Tick {
        self.now
    }

    /// What `node` reads.
    ///
    /// Ordering inside the run always uses the world clock; this is only what a guest is told, so
    /// two nodes disagreeing about the time cannot make the run itself ambiguous.
    #[must_use]
    pub fn now_on(&self, node: NodeId) -> Tick {
        self.nodes.get(&node).map_or(self.now, |clock| clock.read(self.now))
    }

    /// Gives a node a clock of its own.
    pub fn set_node_clock(&mut self, node: NodeId, clock: NodeClock) {
        self.nodes.insert(node, clock);
    }

    /// Moves time forward.
    pub fn advance(&mut self, delta: Tick) {
        self.now = self.now.saturating_add(delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_only_moves_forward() {
        let mut c = Clock::new();
        assert_eq!(c.now(), Tick::ZERO);
        c.advance(Tick::MILLISECOND);
        c.advance(Tick::MILLISECOND);
        assert_eq!(c.now(), Tick(2_000_000));
        c.advance(Tick(u64::MAX));
        assert_eq!(c.now(), Tick(u64::MAX), "saturates instead of wrapping to the past");
    }

    #[test]
    fn a_node_without_a_clock_of_its_own_reads_the_world() {
        let mut c = Clock::new();
        c.advance(Tick::SECOND);
        assert_eq!(c.now_on(NodeId(3)), Tick::SECOND);
    }

    #[test]
    fn an_offset_shifts_every_reading_by_the_same_amount() {
        let mut c = Clock::new();
        c.set_node_clock(NodeId(1), NodeClock::new(Tick::MILLISECOND.get(), 0));
        c.advance(Tick::SECOND);
        assert_eq!(c.now_on(NodeId(1)), Tick(Tick::SECOND.get() + Tick::MILLISECOND.get()));
        c.advance(Tick::SECOND);
        assert_eq!(c.now_on(NodeId(1)), Tick(2 * Tick::SECOND.get() + Tick::MILLISECOND.get()));
    }

    #[test]
    fn drift_pulls_two_nodes_further_apart_as_the_run_goes_on() {
        let mut c = Clock::new();
        c.set_node_clock(NodeId(1), NodeClock::new(0, 100));
        c.set_node_clock(NodeId(2), NodeClock::new(0, -100));

        c.advance(Tick::SECOND);
        let after_one = c.now_on(NodeId(1)).get() - c.now_on(NodeId(2)).get();
        c.advance(Tick(9 * Tick::SECOND.get()));
        let after_ten = c.now_on(NodeId(1)).get() - c.now_on(NodeId(2)).get();

        // 100 parts per million either way is a fifth of a millisecond apart after a second.
        assert_eq!(after_one, 200_000);
        assert_eq!(after_ten, 2_000_000, "and ten times that after ten seconds");
    }

    #[test]
    fn a_rebased_clock_keeps_its_reading_and_changes_only_its_rate() {
        let clock = NodeClock::new(1_000, 150);
        let at = Tick(10 * Tick::SECOND.get());
        let rebased = clock.rebased(at, -150);

        assert_eq!(rebased.read(at), clock.read(at), "the rebase moment reads identically");

        let later = Tick(20 * Tick::SECOND.get());
        assert!(rebased.read(later) < clock.read(later), "the new rate applies after the rebase");
        assert!(rebased.read(later) > rebased.read(at), "and time still moves forward");
    }

    #[test]
    #[should_panic(expected = "MAX_DRIFT_PPM")]
    fn a_rebase_cannot_set_a_rate_past_the_bound() {
        let _ = NodeClock::new(0, 0).rebased(Tick::SECOND, -2_000_000);
    }

    #[test]
    fn a_clock_a_node_reads_never_goes_backwards() {
        let mut c = Clock::new();
        c.set_node_clock(NodeId(1), NodeClock::new(1_000, -200));
        let mut last = Tick::ZERO;
        for _ in 0..1_000 {
            c.advance(Tick::MILLISECOND);
            let now = c.now_on(NodeId(1));
            assert!(now >= last, "a node read {now:?} after {last:?}");
            last = now;
        }
    }
}
