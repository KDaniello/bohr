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
    /// Stable name, recorded in the bundle.
    fn identity(&self) -> &'static str;

    /// Chooses from a non-empty, deterministically ordered set of runnable tasks.
    fn pick(
        &mut self,
        runnable: &[TaskId],
        cursor: &mut TapeCursor<'_>,
    ) -> Result<TaskId, KernelError>;

    /// A task asked to give up the processor.
    ///
    /// Policies that run one task in long stretches switch here, since a spin shows no progress.
    fn yielded(&mut self, _task: TaskId) {}

    /// A task ended and will never be scheduled again.
    ///
    /// Ids are never reused, so the policy can forget it and stay bounded.
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
/// The highest-priority runnable task always runs, so a run is a few long stretches. At `d−1`
/// points drawn in advance the running task is demoted below all others.
///
/// For `n` tasks and `k` steps, a bug needing `d` orderings is found with probability at least
/// `1/(n·k^(d−1))` per run. Most real concurrency bugs have depth one to three.
///
/// `steps` is a window: change points are drawn again for each window, so the bound holds per
/// window in runs of any length.
///
/// A yield passes the task over once and leaves its priority alone. A demoted yielder would never
/// run again, and a real execution client then never finishes starting.
#[derive(Clone, Debug)]
pub struct PctPolicy {
    depth: u32,
    /// Steps one window of change points is spread over.
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
    /// One slot: the run loop runs one task between picks, so at most one can have yielded.
    stood_aside: Option<TaskId>,
    /// The priority a newly seen task is placed above, keeping fresh tasks in contention.
    next_priority: u64,
}

/// Floor of the tape-priced priorities. Demotions count down below it, so each demoted task sits
/// under every priced one and every earlier demotion, as PCT requires.
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
    /// Every window costs the same draws at the same place in its sequence.
    fn arm(&mut self, cursor: &mut TapeCursor<'_>) -> Result<(), KernelError> {
        let base = self.armed_through;
        for _ in 1..self.depth {
            let at =
                base.saturating_add(cursor.draw_range(self.steps, DrawSite::SchedSwitchPoint)?);
            self.change_points.push(at);
        }
        self.armed_through = base.saturating_add(self.steps);
        // Ascending; a point drawn twice demotes twice.
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
        // Passed over once; no priority changes and nothing is drawn.
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

        // A task gets its priority when first offered for scheduling.
        for task in runnable {
            if !self.priorities.contains_key(task) {
                let spread = cursor.draw_range(self.steps, DrawSite::SchedPriority)?;
                self.priorities.insert(*task, self.next_priority + spread);
                self.next_priority += 1;
            }
        }

        while self.change_points.last().is_some_and(|at| *at <= self.step) {
            self.change_points.pop();
            // Demoting the next task hands this step to the second.
            if let Some(current) = self.highest(runnable, |_| true) {
                self.priorities.insert(current, self.next_demotion);
                self.next_demotion = self.next_demotion.saturating_sub(1);
            }
        }

        // The yielder is skipped only if another task can run.
        let aside = self.stood_aside.take();
        self.highest(runnable, |task| Some(task) != aside)
            .or_else(|| self.highest(runnable, |_| true))
            .ok_or(KernelError::Invariant("scheduler was given no tasks"))
    }
}

impl PctPolicy {
    /// The runnable task with the highest priority, ties broken by identifier.
    ///
    /// Ties are broken without a draw, so mutations keep their meaning.
    fn highest(&self, runnable: &[TaskId], eligible: impl Fn(TaskId) -> bool) -> Option<TaskId> {
        runnable
            .iter()
            .copied()
            .filter(|task| eligible(*task))
            .max_by_key(|task| (self.priorities.get(task).copied().unwrap_or(0), task.get()))
    }
}

/// Uniform choice, with starvation bounded: with `n` tasks runnable, none is passed over more than
/// `bound + n - 1` picks in a row.
///
/// For liveness oracles, which would read PCT's long stretches as hangs. A task passed over `bound`
/// times runs next without a draw.
#[derive(Clone, Debug)]
pub struct FairPolicy {
    bound: u32,
    /// Picks each runnable task has been passed over for since it last ran.
    passed_over: BTreeMap<TaskId, u32>,
    /// A task that has just given up its turn, passed over once if anyone else can run.
    stood_aside: Option<TaskId>,
    /// The tasks a draw chooses among, kept across picks so a pick does not allocate.
    candidates: Vec<TaskId>,
}

impl FairPolicy {
    /// A policy whose tasks join the front of the line after `bound` picks passed over.
    #[must_use]
    pub fn new(bound: u32) -> Self {
        Self {
            bound: bound.max(1),
            passed_over: BTreeMap::new(),
            stood_aside: None,
            candidates: Vec::new(),
        }
    }
}

impl Default for FairPolicy {
    fn default() -> Self {
        Self::new(64)
    }
}

impl SchedulePolicy for FairPolicy {
    fn identity(&self) -> &'static str {
        "fair"
    }

    fn pick(
        &mut self,
        runnable: &[TaskId],
        cursor: &mut TapeCursor<'_>,
    ) -> Result<TaskId, KernelError> {
        // The most starved task at the bound runs; ties go to the lower id.
        let starved = runnable
            .iter()
            .copied()
            .filter(|task| self.passed_over.get(task).is_some_and(|n| *n >= self.bound))
            .max_by_key(|task| (self.passed_over[task], std::cmp::Reverse(task.get())));
        let aside = self.stood_aside.take();
        let chosen = match starved {
            Some(task) => task,
            None => {
                self.candidates.clear();
                self.candidates.extend(runnable.iter().copied().filter(|t| Some(*t) != aside));
                if self.candidates.is_empty() {
                    self.candidates.extend_from_slice(runnable);
                }
                let index = cursor.draw_index(self.candidates.len(), DrawSite::SchedPick)?;
                *self
                    .candidates
                    .get(index)
                    .ok_or(KernelError::Invariant("scheduler drew out of range"))?
            }
        };
        for task in runnable {
            if *task != chosen {
                *self.passed_over.entry(*task).or_default() += 1;
            }
        }
        self.passed_over.remove(&chosen);
        Ok(chosen)
    }

    fn yielded(&mut self, task: TaskId) {
        self.stood_aside = Some(task);
    }

    fn forget(&mut self, task: TaskId) {
        self.passed_over.remove(&task);
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

    /// PCT runs one task in long stretches.
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

    /// A change point falls anywhere in the window with roughly equal probability, checked against
    /// the paper's bound.
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

        // Depth 2 places one change point; the loose bound rules out always-early, always-late and
        // never.
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

    /// Each demotion lands below the previous one, so demotion order decides among demoted tasks.
    #[test]
    fn a_later_demotion_sits_below_an_earlier_one() {
        let tape = Tape::from_seed(Seed(2));
        let mut cursor = tape.cursor();
        // Depth 3 arms two change points in a short window.
        let mut policy = PctPolicy::new(3, 4);
        let runnable = [TaskId(0), TaskId(1), TaskId(2)];

        let mut demoted = Vec::new();
        for _ in 0..12 {
            let picked = policy.pick(&runnable, &mut cursor).expect("pick");
            // Below the priced band means demoted.
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
    fn fair_passes_no_task_over_more_than_its_bound() {
        let tasks: Vec<TaskId> = (0..6).map(TaskId).collect();
        let tape = Tape::from_seed(Seed(11));
        let mut cursor = tape.cursor();
        let mut policy = FairPolicy::new(4);
        let mut since = [0u32; 6];
        for _ in 0..2_000 {
            let picked = policy.pick(&tasks, &mut cursor).expect("pick");
            for (i, n) in since.iter_mut().enumerate() {
                *n = if i as u32 == picked.get() { 0 } else { *n + 1 };
                // bound + n - 1, with a bound of 4 and six tasks
                assert!(*n <= 9, "task {i} was passed over {n} times in a row");
            }
        }
    }

    #[test]
    fn fair_reproduces_and_costs_nothing_for_one_task() {
        let tasks: Vec<TaskId> = (0..3).map(TaskId).collect();
        let schedule = |seed: u64| {
            let tape = Tape::from_seed(Seed(seed));
            let mut cursor = tape.cursor();
            let mut policy = FairPolicy::default();
            (0..200).map(|_| policy.pick(&tasks, &mut cursor).unwrap()).collect::<Vec<_>>()
        };
        assert_eq!(schedule(5), schedule(5));
        assert!((0..16).any(|seed| schedule(5) != schedule(seed)), "the seed chooses the schedule");

        let tape = Tape::from_seed(Seed(1));
        let mut cursor = tape.cursor();
        assert_eq!(FairPolicy::default().pick(&[TaskId(9)], &mut cursor).unwrap(), TaskId(9));
        assert_eq!(cursor.position().get(), 0, "one option is not a decision");
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
