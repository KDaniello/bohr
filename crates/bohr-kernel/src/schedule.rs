//! Choosing which runnable task executes next.

use std::collections::BTreeMap;

use bohr_tape::{DrawSite, TapeCursor};
use bohr_types::TaskId;

use crate::KernelError;

/// Picks the next task to run.
///
/// Every choice draws from the tape, so a policy is reproducible from the seed and a mutated tape
/// explores a different interleaving.
pub trait SchedulePolicy {
    /// Stable name, recorded in the bundle so that runs made under different policies are not
    /// compared as if they were alike.
    fn identity(&self) -> &'static str;

    /// Chooses from a non-empty, deterministically ordered set of runnable tasks.
    fn pick(
        &mut self,
        runnable: &[TaskId],
        cursor: &mut TapeCursor<'_>,
    ) -> Result<TaskId, KernelError>;

    /// A task asked to give up the processor.
    ///
    /// Policies that keep one task running until something moves it need this, since a spin on a
    /// flag makes no other progress the policy could notice.
    fn yielded(&mut self, _task: TaskId) {}

    /// A task ended and will never be scheduled again.
    ///
    /// Identifiers are never reused, so anything a policy remembers about one is dead weight from
    /// here on: a guest that creates threads in a loop would otherwise grow the policy without end.
    fn forget(&mut self, _task: TaskId) {}
}

/// Uniform choice among runnable tasks.
#[derive(Clone, Copy, Debug, Default)]
pub struct RandomPolicy;

impl SchedulePolicy for RandomPolicy {
    fn identity(&self) -> &'static str {
        "random"
    }

    fn pick(
        &mut self,
        runnable: &[TaskId],
        cursor: &mut TapeCursor<'_>,
    ) -> Result<TaskId, KernelError> {
        let index = cursor.draw_index(runnable.len(), DrawSite::SchedPick)?;
        runnable.get(index).copied().ok_or(KernelError::Invariant("scheduler drew out of range"))
    }
}

/// Probabilistic Concurrency Testing (Burckhardt et al., ASPLOS 2010).
///
/// Each task holds a priority and the highest runnable one always runs, so a run is a small number
/// of long uninterrupted stretches rather than a fine shuffle. At `d−1` points chosen in advance
/// the running task is demoted below every other, which forces one specific reordering.
///
/// The guarantee is what makes it the default: for `n` tasks and `k` steps, a bug that needs `d`
/// particular orderings is found with probability at least `1/(n·k^(d−1))` per run. Uniform choice
/// offers no such bound, and most real concurrency bugs have a depth of one to three.
///
/// **`steps` is a window, not the run.** A run of millions of steps is longer than any estimate
/// worth writing down, and change points drawn once over the first thousand leave the rest of it
/// with a priority order nothing can disturb — which is not exploration, it is a fixed schedule
/// with a shuffled beginning. So the points are drawn again for each window, and the guarantee is
/// the same statement made once per window rather than once per run.
///
/// **A yield is not a demotion.** It says "let somebody else go now", and that is all it is taken
/// to mean: the task is passed over once and is back in contention at the next decision. Demoting
/// it instead put it below the priced band for good, and a task there is runnable at every decision
/// and picked at none — measured against a real execution client that never got past its startup
/// under this policy while coming up in full under uniform choice.
#[derive(Clone, Debug)]
pub struct PctPolicy {
    depth: u32,
    /// How many steps one window of change points is spread over, and so how often the priority
    /// order is disturbed. Not an estimate of the run: a run is as long as the guest makes it.
    steps: u64,
    priorities: BTreeMap<TaskId, u64>,
    /// Steps at which the running task is demoted, in the order they will be used.
    change_points: Vec<u64>,
    /// Priorities handed out by demotion, counting down so each demoted task sits below the last.
    next_demotion: u64,
    step: u64,
    /// The step the drawn change points reach, and where the next window is drawn from.
    armed_through: u64,
    /// A task that has just given up its turn, passed over once and then forgotten.
    ///
    /// One slot is enough, and that is a fact about the caller rather than a simplification: the
    /// run loop picks one task, runs it, and only then asks again, so at most one task can have
    /// yielded between two picks.
    stood_aside: Option<TaskId>,
    /// The priority a newly seen task is placed above, keeping fresh tasks in contention.
    next_priority: u64,
}

/// Floor of the band where tape-priced priorities live. Demotions count down from just below it,
/// so a demoted task sits under every priced one and under every task demoted before it — the
/// ordering the PCT guarantee is stated over.
const PRICED_FLOOR: u64 = 1 << 63;

impl PctPolicy {
    /// A policy of the given depth, spreading its change points over `steps`.
    #[must_use]
    pub fn new(depth: u32, steps: u64) -> Self {
        Self {
            depth: depth.max(1),
            steps: steps.max(1),
            priorities: BTreeMap::new(),
            change_points: Vec::new(),
            next_demotion: PRICED_FLOOR - 1,
            step: 0,
            armed_through: 0,
            stood_aside: None,
            next_priority: PRICED_FLOOR,
        }
    }

    /// Draws the change points for the window that begins where the last one ended.
    ///
    /// Once per window rather than once per run, because a run is as long as the guest makes it and
    /// a window is a number written down here. Every window costs the same draws in the same place,
    /// so the tape stays a function of the run's shape rather than of anything the guest computed.
    fn arm(&mut self, cursor: &mut TapeCursor<'_>) -> Result<(), KernelError> {
        let base = self.armed_through;
        for _ in 1..self.depth {
            let at =
                base.saturating_add(cursor.draw_range(self.steps, DrawSite::SchedSwitchPoint)?);
            self.change_points.push(at);
        }
        self.armed_through = base.saturating_add(self.steps);
        // Used in ascending order so the demotions happen in the order the run reaches them, and a
        // point drawn twice simply demotes twice at that step.
        self.change_points.sort_unstable();
        self.change_points.reverse();
        Ok(())
    }
}

impl Default for PctPolicy {
    fn default() -> Self {
        Self::new(3, 1_000)
    }
}

impl SchedulePolicy for PctPolicy {
    fn identity(&self) -> &'static str {
        "pct"
    }

    fn yielded(&mut self, task: TaskId) {
        // Passed over once, and no priority is touched: the call says "let somebody else go now",
        // and a task that meant to stop being scheduled would have blocked instead. It costs no
        // entropy either way — the guest asked, rather than the tape.
        self.stood_aside = Some(task);
    }

    fn forget(&mut self, task: TaskId) {
        self.priorities.remove(&task);
    }

    fn pick(
        &mut self,
        runnable: &[TaskId],
        cursor: &mut TapeCursor<'_>,
    ) -> Result<TaskId, KernelError> {
        if self.step >= self.armed_through {
            self.arm(cursor)?;
        }
        self.step += 1;

        // A task is given its priority when it is first seen rather than when it is created: the
        // policy is told about tasks by being asked to schedule them.
        for task in runnable {
            if !self.priorities.contains_key(task) {
                let spread = cursor.draw_range(self.steps, DrawSite::SchedPriority)?;
                self.priorities.insert(*task, self.next_priority + spread);
                self.next_priority += 1;
            }
        }

        while self.change_points.last().is_some_and(|at| *at <= self.step) {
            self.change_points.pop();
            // Demoting the task that would run next is what makes the change a reordering: it hands
            // execution to whoever was second, at exactly this step.
            if let Some(current) = self.highest(runnable) {
                self.priorities.insert(current, self.next_demotion);
                self.next_demotion = self.next_demotion.saturating_sub(1);
            }
        }

        // Whoever gave up its turn is skipped, but only while somebody else can take it: a task
        // standing aside for an empty room would be standing aside for ever.
        let aside = self.stood_aside.take();
        let waiting: Vec<TaskId> =
            runnable.iter().copied().filter(|task| Some(*task) != aside).collect();
        let among = if waiting.is_empty() { runnable } else { &waiting };

        self.highest(among).ok_or(KernelError::Invariant("scheduler was given no tasks"))
    }
}

impl PctPolicy {
    /// The runnable task with the highest priority, ties broken by identifier.
    ///
    /// A tie is broken without a draw on purpose: two tasks share a priority only when the tape
    /// gave them one, and spending entropy to separate them again would change what a mutation
    /// means.
    fn highest(&self, runnable: &[TaskId]) -> Option<TaskId> {
        runnable
            .iter()
            .copied()
            .max_by_key(|task| (self.priorities.get(task).copied().unwrap_or(0), task.get()))
    }
}

#[cfg(test)]
mod tests {
    use bohr_tape::Tape;
    use bohr_types::Seed;

    use super::*;

    #[test]
    fn a_single_runnable_task_costs_no_entropy() {
        let tape = Tape::from_seed(Seed(1));
        let mut cursor = tape.cursor();
        let mut policy = RandomPolicy;
        let picked = policy.pick(&[TaskId(7)], &mut cursor).expect("pick");
        assert_eq!(picked, TaskId(7));
        assert_eq!(cursor.position().get(), 0, "one option is not a decision");
    }

    #[test]
    fn the_same_tape_yields_the_same_schedule() {
        let tasks: Vec<TaskId> = (0..5).map(TaskId).collect();
        let schedule = || {
            let tape = Tape::from_seed(Seed(99));
            let mut cursor = tape.cursor();
            let mut policy = RandomPolicy;
            (0..64).map(|_| policy.pick(&tasks, &mut cursor).expect("pick")).collect::<Vec<_>>()
        };
        assert_eq!(schedule(), schedule());
    }

    #[test]
    fn every_runnable_task_is_reachable() {
        let tasks: Vec<TaskId> = (0..4).map(TaskId).collect();
        let tape = Tape::from_seed(Seed(3));
        let mut cursor = tape.cursor();
        let mut policy = RandomPolicy;
        let mut seen = [false; 4];
        for _ in 0..256 {
            let t = policy.pick(&tasks, &mut cursor).expect("pick");
            seen[t.get() as usize] = true;
        }
        assert!(seen.iter().all(|s| *s), "a task the policy never picks is a task never tested");
    }

    /// PCT runs one task at a time in long stretches, which is what its guarantee is built on: a
    /// policy that shuffled every step would explore orderings uniformly and prove nothing.
    #[test]
    fn pct_runs_one_task_until_it_demotes_it() {
        let tasks: Vec<TaskId> = (0..4).map(TaskId).collect();
        let tape = Tape::from_seed(Seed(5));
        let mut cursor = tape.cursor();
        let mut policy = PctPolicy::new(3, 200);

        let picks: Vec<TaskId> =
            (0..200).map(|_| policy.pick(&tasks, &mut cursor).unwrap()).collect();
        let switches = picks.windows(2).filter(|w| w[0] != w[1]).count();
        assert!(
            switches <= 2,
            "a depth-3 policy changes the running task at most twice, saw {switches}"
        );
    }

    /// The guarantee PCT is chosen for: a change point falls anywhere in the run with roughly equal
    /// probability, so a bug that needs one particular ordering is reached at a rate that can be
    /// stated in advance rather than hoped for.
    ///
    /// Checked against the bound from the paper rather than against another implementation of it: a
    /// second implementation would have to be trusted, and the bound does not.
    #[test]
    fn pct_spreads_its_change_points_across_the_run() {
        const STEPS: u64 = 20;
        const SEEDS: u64 = 2_000;

        let tasks: Vec<TaskId> = (0..2).map(TaskId).collect();
        let mut switched_at = vec![0u32; STEPS as usize + 1];

        for seed in 0..SEEDS {
            let tape = Tape::from_seed(Seed(seed));
            let mut cursor = tape.cursor();
            let mut policy = PctPolicy::new(2, STEPS);

            let mut previous = None;
            for step in 1..=STEPS {
                let picked = policy.pick(&tasks, &mut cursor).unwrap();
                if previous.is_some_and(|p| p != picked) {
                    switched_at[step as usize] += 1;
                    break;
                }
                previous = Some(picked);
            }
        }

        // A depth-2 policy places one change point, so each step should take about its share of the
        // runs. The bound is loose because the sample is finite; what it rules out is a policy that
        // switches always at the start, always at the end, or never.
        let switched: u32 = switched_at.iter().sum();
        assert!(
            switched > SEEDS as u32 / 2,
            "a change point should fall inside the run most of the time, saw {switched}/{SEEDS}"
        );

        let reached = switched_at.iter().skip(1).filter(|count| **count > 0).count();
        assert!(
            reached >= STEPS as usize / 2,
            "the change point only ever landed on {reached} of {STEPS} steps"
        );
    }

    /// Demotions used to count down from zero and saturate there, so every demoted task shared
    /// one priority and the identifier tie-break, not demotion order, decided who ran.
    ///
    /// Driven through the change points, which is the only thing that demotes. The test this
    /// replaces armed a depth of one -- no change points at all -- and called `yielded` twice,
    /// so it exercised standing aside and never reached the countdown it is named for.
    #[test]
    fn a_later_demotion_sits_below_an_earlier_one() {
        let tape = Tape::from_seed(Seed(2));
        let mut cursor = tape.cursor();
        // Depth three arms two change points, and a short window puts them within reach.
        let mut policy = PctPolicy::new(3, 4);
        let runnable = [TaskId(0), TaskId(1), TaskId(2)];

        let mut demoted = Vec::new();
        for _ in 0..12 {
            let picked = policy.pick(&runnable, &mut cursor).expect("pick");
            // A task whose priority has fallen below the priced band is one that was demoted.
            for task in runnable {
                let below = policy.priorities.get(&task).is_some_and(|p| *p < PRICED_FLOOR);
                if below && !demoted.contains(&task) {
                    demoted.push(task);
                }
            }
            let _ = picked;
        }

        assert!(demoted.len() >= 2, "the change points demoted {} tasks", demoted.len());
        let first = policy.priorities[&demoted[0]];
        let second = policy.priorities[&demoted[1]];
        assert!(
            second < first,
            "the task demoted second sits at {second}, not below the first at {first}"
        );
    }

    #[test]
    fn pct_reproduces_and_responds_to_the_seed() {
        let tasks: Vec<TaskId> = (0..3).map(TaskId).collect();
        let schedule = |seed: u64| {
            let tape = Tape::from_seed(Seed(seed));
            let mut cursor = tape.cursor();
            let mut policy = PctPolicy::new(3, 100);
            (0..100).map(|_| policy.pick(&tasks, &mut cursor).unwrap()).collect::<Vec<_>>()
        };
        assert_eq!(schedule(7), schedule(7));

        let differs = (0..32).any(|seed| schedule(7) != schedule(seed));
        assert!(differs, "no seed produced a different schedule");
    }
}
