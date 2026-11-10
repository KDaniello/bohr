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
    offset: u64,
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
        let offset = u64::try_from((reading - drifted).max(0)).unwrap_or(u64::MAX);
        Self { offset, drift_ppm, wall_step: self.wall_step }
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
