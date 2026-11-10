//! Logical time: the world clock the run is ordered by, and each node's two readings of it.
//! No host clock is read here; time moves only when the run loop advances it.

use std::collections::BTreeMap;

use bohr_types::{NodeId, Tick};

/// Parts per million a node's clock may run fast or slow by.
///
/// Real machines' clocks disagree; the bound keeps a run's skew realistic.
pub const MAX_DRIFT_PPM: u32 = 200;

/// Where every wall clock starts: 2025-01-01 00:00:00 UTC, in ticks since 1970.
///
/// A plausible date, so certificate and genesis checks pass and an early step back has room.
/// Fixed, so every host reads the same dates.
pub const WALL_EPOCH: Tick = Tick(1_735_689_600 * 1_000_000_000);

/// How one node sees the world clock, as two readings: monotonic, and wall.
///
/// The monotonic reading is the offset plus drift and never goes back. The wall reading is the same
/// on top of [`WALL_EPOCH`], plus a signed step, so a clock set back moves wall time back while
/// monotonic time keeps rising, as `settimeofday` does on Linux.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NodeClock {
    /// Signed, so a change of rate leaves the reading where it was, faster or slower.
    offset: i64,
    /// Rate difference in parts per million, positive for a clock that runs fast.
    drift_ppm: i32,
    /// How far the wall reading has been stepped from the monotonic one.
    wall_step: i64,
}

impl NodeClock {
    /// A clock reading `offset` ticks ahead and running `drift_ppm` parts per million fast.
    ///
    /// # Panics
    ///
    /// Panics on a rate outside ±[`MAX_DRIFT_PPM`].
    #[must_use]
    pub const fn new(offset: u64, drift_ppm: i32) -> Self {
        assert_drift_in_bounds(drift_ppm);
        let offset = if offset > i64::MAX as u64 { i64::MAX } else { offset as i64 };
        Self { offset, drift_ppm, wall_step: 0 }
    }

    /// The same reading at `at`, drifting at `drift_ppm` from there on.
    ///
    /// The accumulated drift folds into the offset, so readings before `at` are unchanged. The
    /// clock never reads backwards.
    ///
    /// # Panics
    ///
    /// Panics on a rate outside ±[`MAX_DRIFT_PPM`].
    #[must_use]
    pub fn rebased(self, at: Tick, drift_ppm: i32) -> Self {
        assert_drift_in_bounds(drift_ppm);
        let reading = i128::from(self.read(at).get());
        let elapsed = i128::from(at.get());
        let drifted = elapsed + (elapsed * i128::from(drift_ppm)) / 1_000_000;
        let offset = (reading - drifted).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        Self { offset, drift_ppm, wall_step: self.wall_step }
    }

    /// The earliest world time, not before `from`, at which this clock reads at least `reading`.
    ///
    /// How a node's own duration becomes a deadline on the world clock: a slow clock needs more
    /// world time to show the same span, so a timer armed this way never fires early by the
    /// node's own reading.
    #[must_use]
    pub const fn world_at(self, reading: Tick, from: Tick) -> Tick {
        if self.read(from).get() >= reading.get() {
            return from;
        }
        // The inverse of `read`, then a step either way past its rounding.
        let rate = 1_000_000 + self.drift_ppm as i128;
        let wanted = reading.get() as i128 - self.offset as i128;
        let mut at = (wanted * 1_000_000 + rate - 1) / rate;
        if at < from.get() as i128 {
            at = from.get() as i128;
        }
        if at > u64::MAX as i128 {
            return Tick(u64::MAX);
        }
        let mut at = at as u64;
        while self.read(Tick(at)).get() < reading.get() && at < u64::MAX {
            at += 1;
        }
        while at > from.get() && self.read(Tick(at - 1)).get() >= reading.get() {
            at -= 1;
        }
        Tick(at)
    }

    /// The same clock with its wall reading stepped by `by` ticks, back where `by` is negative.
    ///
    /// The monotonic reading is untouched; the wall reading never goes below 1970.
    #[must_use]
    pub const fn stepped(self, by: i64) -> Self {
        Self { wall_step: self.wall_step.saturating_add(by), ..self }
    }

    /// What this node's wall clock (`CLOCK_REALTIME`) reads when the world clock says `now`.
    #[must_use]
    pub const fn read_wall(self, now: Tick) -> Tick {
        let stepped =
            WALL_EPOCH.get() as i128 + self.read(now).get() as i128 + self.wall_step as i128;
        if stepped <= 0 {
            Tick::ZERO
        } else if stepped > u64::MAX as i128 {
            Tick(u64::MAX)
        } else {
            Tick(stepped as u64)
        }
    }

    /// What this node's monotonic clock reads when the world clock says `now`.
    ///
    /// Drift accumulates with elapsed time, so nodes diverge as the run goes on.
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
/// Advances only when the run says so. A guest sleeping an hour wakes at once, and load on the
/// host never changes virtual time.
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

    /// The world time at which `node`'s clock will have moved `ahead` past what it reads now.
    ///
    /// A node with no clock of its own reads the world clock, and its deadline is plain.
    #[must_use]
    pub fn deadline_on(&self, node: NodeId, ahead: u64) -> Tick {
        match self.nodes.get(&node) {
            Some(clock) => {
                let target = Tick(clock.read(self.now).get().saturating_add(ahead));
                clock.world_at(target, self.now)
            }
            None => Tick(self.now.get().saturating_add(ahead)),
        }
    }

    /// What `node` reads.
    ///
    /// Only what the guest is told; the run is ordered by the world clock.
    #[must_use]
    pub fn now_on(&self, node: NodeId) -> Tick {
        self.nodes.get(&node).map_or(self.now, |clock| clock.read(self.now))
    }

    /// What `node`'s wall clock reads: [`Self::now_on`] plus any step it has been given.
    #[must_use]
    pub fn wall_on(&self, node: NodeId) -> Tick {
        self.nodes
            .get(&node)
            .map_or(Tick(WALL_EPOCH.get().saturating_add(self.now.get())), |clock| {
                clock.read_wall(self.now)
            })
    }

    /// The clock `node` reads by, where it has one of its own.
    #[must_use]
    pub fn node_clock(&self, node: NodeId) -> Option<NodeClock> {
        self.nodes.get(&node).copied()
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

    /// A clock that starts running fast keeps its reading at the change, as NTP slews one.
    #[test]
    fn a_change_of_rate_leaves_the_reading_where_it_was() {
        let at = Tick(1_000 * Tick::SECOND.get());
        for (from, to) in [(-200, 200), (200, -200), (0, 200), (-200, 0)] {
            let before = NodeClock::new(0, from);
            let after = before.rebased(at, to);
            assert_eq!(after.read(at), before.read(at), "{from} to {to} ppm stepped the clock");
        }
    }

    /// A node's span becomes a world deadline the node's clock has fully passed, and no later.
    #[test]
    fn a_deadline_is_never_early_by_the_nodes_own_clock() {
        let span = 10 * Tick::SECOND.get();
        for drift in [-200, -1, 0, 1, 200] {
            let mut clock = Clock::new();
            clock.advance(Tick(3 * Tick::SECOND.get()));
            clock.set_node_clock(NodeId(0), NodeClock::new(17, drift));
            let start = clock.now_on(NodeId(0));
            let at = clock.deadline_on(NodeId(0), span);
            let node = NodeClock::new(17, drift);
            assert!(node.read(at).get() >= start.get() + span, "{drift} ppm woke early");
            assert!(node.read(Tick(at.get() - 1)).get() < start.get() + span, "{drift} ppm late");
        }
    }

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
    fn a_step_back_moves_wall_time_and_never_monotonic_time() {
        let at = Tick(10 * Tick::SECOND.get());
        let clock = NodeClock::new(1_000, 0).stepped(-(5 * Tick::SECOND.get() as i64));
        assert_eq!(clock.read(at), Tick(at.get() + 1_000), "monotonic time is untouched");
        assert_eq!(
            clock.read_wall(at),
            Tick(WALL_EPOCH.get() + at.get() + 1_000 - 5 * Tick::SECOND.get())
        );
        let later = Tick(at.get() + Tick::MILLISECOND.get());
        assert!(clock.read(later) > clock.read(at), "and keeps rising");
    }

    #[test]
    fn a_wall_clock_set_before_1970_reads_1970() {
        let clock = NodeClock::new(0, 0).stepped(i64::MIN);
        assert_eq!(clock.read_wall(Tick::SECOND), Tick::ZERO);
    }

    /// A step back in a run's first second moves the clock back; from zero it could not.
    #[test]
    fn a_step_back_at_the_start_still_moves_the_clock() {
        let clock = NodeClock::new(0, 0).stepped(-(60 * Tick::SECOND.get() as i64));
        assert_eq!(clock.read_wall(Tick::SECOND), Tick(WALL_EPOCH.get() - 59 * Tick::SECOND.get()));
    }

    #[test]
    fn a_rebase_keeps_the_step() {
        let clock = NodeClock::new(0, 0).stepped(7);
        let rebased = clock.rebased(Tick::SECOND, 100);
        let later = Tick(2 * Tick::SECOND.get());
        assert_eq!(
            rebased.read_wall(later).get() - rebased.read(later).get(),
            WALL_EPOCH.get() + 7
        );
    }

    #[test]
    fn an_unstepped_clock_reads_the_same_on_both_from_the_epoch() {
        let mut c = Clock::new();
        c.set_node_clock(NodeId(1), NodeClock::new(1_234, -50));
        c.advance(Tick(42 * Tick::SECOND.get()));
        assert_eq!(c.wall_on(NodeId(1)), Tick(WALL_EPOCH.get() + c.now_on(NodeId(1)).get()));
        assert_eq!(
            c.wall_on(NodeId(9)),
            Tick(WALL_EPOCH.get() + c.now().get()),
            "a node without a clock reads the world, from the epoch"
        );
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
