//! The run loop: one guest driven from its first instruction to the ending the run is judged on.
//! It owns task bookkeeping and the order of everything; what a stop means belongs to the world.

use std::collections::{BTreeMap, BTreeSet};

use bohr_backend::{DeterminismBackend, GuestExit, GuestSpec, Resume, Stop, StopKind};
use bohr_tape::{DrawSite, Journal, Tape, TapeCursor};
use bohr_trace::{EventKind, ExitStatus, Outcome, SwitchReason};
use bohr_types::{Digest, EventId, NodeId, TaskId, Tick};

use crate::{
    KernelError, StopReason,
    clock::{Clock, MAX_DRIFT_PPM, NodeClock},
    ports::{
        DeadlineCtx, DueEffects, EventSink, NodeOp, Observer, Oracle, Progress, Recorded,
        RunOutcome, StopCtx, TaskState, Verdict, Wake, WorldModel,
    },
    schedule::SchedulePolicy,
};

/// Bounds that stop a run that is not going to end on its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RunConfig {
    /// Virtual time after which the guest is considered hung.
    pub max_ticks: Tick,
    /// Event count after which the run is abandoned.
    pub max_events: u64,
    /// How far ahead of the world clock a node's clock may be drawn, in ticks.
    ///
    /// Ahead only: the offset is non-negative ([`crate::NodeClock`]).
    pub max_clock_skew: Tick,
    /// An event to stop the world before, where a caller asked for one.
    ///
    /// The run stops at the first scheduling point at or after this event, with every task
    /// stopped and no stop half-answered. It can overshoot, and says so. See `docs/API.md` §7.3.
    pub break_at: Option<EventId>,
    /// Virtual time at which a run that was never going to end on its own is complete.
    ///
    /// `None` for a guest expected to finish; `None` leaves the run unchanged.
    ///
    /// Reaching `max_ticks` means the guest hung; reaching the horizon means a server ran long
    /// enough. A deadlock before the horizon is still a deadlock.
    pub horizon: Option<Tick>,
    /// The node whose ending is the run's ending, where a scenario names one.
    ///
    /// A service never ends, so the run ends with the node that drives it, without guessing a
    /// horizon. `None` leaves the run unchanged.
    pub ends_with: Option<NodeId>,
    /// Virtual time one unit of the guest's own computation costs, where the backend counts it.
    ///
    /// `None` keeps time moving only on what the world answers. With a rate, a stop on an
    /// exhausted budget also advances the clock by the units the guest consumed
    /// (`docs/ARCHITECTURE.md` §4.2).
    pub ticks_per_progress_unit: Option<u64>,
    /// Which of its draws the run's journal keeps; every one is counted.
    ///
    /// All by default, as a bundle records them. A run that keeps no log may keep only what its
    /// caller reads back.
    pub draws: bohr_tape::Keep,
    /// Whether a guest reads its clock without a stop, where the backend can give it a clock page.
    ///
    /// On by default. A stored run is run again as it was recorded: one from before the page, or
    /// recorded without it, runs without it.
    pub clock_page: bool,
}

/// The SDK fault points a run may arm, by name, and the odds each is armed on a node.
///
/// Empty arms nothing; the draw that decides is made either way (see [`ARMING`]).
#[derive(Clone, Copy, Debug)]
pub struct Arming<'a> {
    /// The names, as the guest gives them to `fault_point`.
    pub names: &'a [String],
    /// The odds each is armed on a node.
    pub odds: (u32, u32),
    /// A node never armed, whatever is listed: the probe's, which is not the cluster's.
    pub except: Option<NodeId>,
}

impl Arming<'_> {
    /// Nothing armed.
    pub const NONE: Arming<'static> = Arming { names: &[], odds: (0, 1), except: None };
}

/// The variable the SDKs read the armed fault points from: names, comma-separated.
pub const ARMED_VARIABLE: &str = "BOHR_ARMED_FAULT_POINTS";

/// The diagnostic that says which fault points a node was started with armed.
pub const ARMING: &str = "BOHR-2008";

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            max_ticks: Tick(DEFAULT_MAX_HOURS * 3600 * Tick::SECOND.get()),
            max_events: 10_000_000,
            max_clock_skew: Tick(50 * Tick::MILLISECOND.get()),
            break_at: None,
            horizon: None,
            ends_with: None,
            ticks_per_progress_unit: None,
            draws: bohr_tape::Keep::All,
            clock_page: true,
        }
    }
}

/// Whether `name` is armed on `node`, from the run's arming draw.
///
/// A hash of the draw, node and name, so the tape spends eight bytes whatever the scenario lists.
///
/// All-ones arms nothing, since the reducer writes all-ones to quiet a decision.
fn armed(seed: u64, node: u32, name: &str, (num, den): (u32, u32)) -> bool {
    if num == 0 || den == 0 || seed == u64::MAX {
        return false;
    }
    let mut bytes = seed.to_le_bytes().to_vec();
    bytes.extend_from_slice(&node.to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    let digest = Digest::of(&bytes);
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(first) % u64::from(den) < u64::from(num)
}

/// Virtual hours a run may reach before it is called hung.
///
/// A day and an hour, so a guest sleeping a day finishes. A guest spinning without syscalls
/// advances no time and is caught by the event budget.
const DEFAULT_MAX_HOURS: u64 = 25;

/// Events between two progress reports to an [`Observer`].
pub const PROGRESS_EVERY: u64 = 4096;

/// Everything one run needs.
#[allow(missing_debug_implementations)] // trait objects, none of which are Debug
pub struct RunInputs<'a, 't> {
    /// Where every nondeterministic decision comes from.
    pub tape: &'t Tape,
    /// How the guest is executed.
    pub backend: &'a mut dyn DeterminismBackend,
    /// What the guest thinks it is talking to.
    pub world: &'a mut dyn WorldModel,
    /// How execution is interleaved.
    pub policy: &'a mut dyn SchedulePolicy,
    /// How the run is judged.
    pub oracle: &'a mut dyn Oracle,
    /// Where events are recorded.
    pub sink: &'a mut dyn EventSink,
    /// Who is shown the run's progress as it goes, if anyone. Never part of what the run is.
    pub observer: Option<&'a mut dyn Observer>,
    /// What to run: one spec per node, `NodeId(i)` running `specs[i]`.
    ///
    /// Node ids are dense and ordered, so clock draws keep their tape offsets.
    pub specs: &'a [GuestSpec],
    /// Bounds on the run.
    pub config: RunConfig,
    /// Which SDK fault points the run may arm.
    pub arming: Arming<'a>,
}

/// What a run produced.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RunReport {
    /// Digest of the canonical event stream: the definition of which run this was.
    pub digest: Digest,
    /// Events recorded.
    pub events: u64,
    /// Virtual time reached.
    pub ticks: Tick,
    /// Why the loop ended.
    pub stop_reason: StopReason,
    /// What the oracle made of it.
    pub outcome: Outcome,
    /// Where a promise the guest made about itself first did not hold.
    ///
    /// A report points here, since the guest keeps running after a failed assertion.
    pub violation_event: Option<EventId>,
    /// Draws made, in order.
    pub journal: Journal,
    /// How many faults actually landed on the guest.
    ///
    /// Shows whether a clean campaign injected anything.
    pub adversity: u32,
    /// What the loop did, counted. Read for progress and comparison, never for a verdict.
    pub metrics: RunMetrics,
}

/// What the loop did, counted as it went.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RunMetrics {
    /// Scheduling decisions made.
    pub decisions: u64,
    /// Times the running task changed.
    pub switches: u64,
    /// Stops the world answered.
    pub stops: u64,
    /// Virtual time each task's stops moved the world clock by.
    pub ticks_by_task: BTreeMap<TaskId, Tick>,
}

/// Whether a node's tasks may be offered to the scheduler.
///
/// The kernel's mirror of the lifecycle the world decides and narrates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NodeLifecycle {
    Running,
    Frozen,
    Dead,
}

/// Where a task stands between two picks. One slot per task, so it cannot be waiting and ready at
/// once.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Slot {
    /// Stopped, holding the action it resumes with when the scheduler next picks it.
    Pending(Resume),
    /// Waiting on the world. All tasks here with nothing armed is a deadlock.
    Blocked,
    /// Neither: executing, or killed and awaiting the exit the backend still reports.
    Out,
}

/// Everything the loop knows about one task.
#[derive(Clone, PartialEq, Eq, Debug)]
struct TaskEntry {
    /// The node it belongs to; `None` once it was retired with its node.
    node: Option<NodeId>,
    slot: Slot,
    /// The signal it last stopped on, since the host reports our kill signal instead.
    faulted_on: Option<u32>,
    /// Taken off the books when its node ended; the later backend exit is expected.
    retired: bool,
    /// The first task of its process: what `exit_group` ends is the tasks sharing this.
    group: TaskId,
}

/// What a task counts as in the loop's indices, read from its slot and whether it was retired.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Standing {
    /// Ready for the scheduler, where its node runs.
    Pending,
    /// Waiting on the world.
    Blocked,
    /// Executing on the host, which still owes the loop its next stop.
    Running,
    /// Out of both queues for good; its reports are dropped.
    Retired,
}

impl TaskEntry {
    const fn standing(&self) -> Standing {
        match (&self.slot, self.retired) {
            (Slot::Pending(_), _) => Standing::Pending,
            (Slot::Blocked, _) => Standing::Blocked,
            (Slot::Out, false) => Standing::Running,
            (Slot::Out, true) => Standing::Retired,
        }
    }
}

/// The loop's books: one entry per task the run knows, and what it knows about each node.
///
/// Every stop asks what is ready, waiting and running; the answers are kept as the entries change,
/// so a stop costs the same with ten tasks as with ten thousand. An entry changes only through
/// [`Tasks::change`], [`Tasks::add`] and [`Tasks::note_task_gone`], which keep them in step.
#[derive(Clone, Default)]
struct Tasks {
    entries: BTreeMap<TaskId, TaskEntry>,
    /// Entries that name each node, so "does this node still have tasks" is a lookup.
    per_node: BTreeMap<NodeId, usize>,
    node_state: BTreeMap<NodeId, NodeLifecycle>,
    last_run: Option<TaskId>,
    /// The tasks standing `Pending`, in identifier order.
    pending: BTreeSet<TaskId>,
    /// How many stand `Blocked`.
    blocked: usize,
    /// How many stand `Running`.
    running: usize,
}

impl Tasks {
    /// Enters a task the run has just learned of.
    /// Adds a task, of the process `group` names: itself for a process's first task.
    fn add(&mut self, task: TaskId, node: NodeId, slot: Slot, group: TaskId) {
        *self.per_node.entry(node).or_default() += 1;
        let entry = TaskEntry { node: Some(node), slot, faulted_on: None, retired: false, group };
        let standing = entry.standing();
        if let Some(old) = self.entries.insert(task, entry) {
            self.leave(task, old.standing());
        }
        self.enter(task, standing);
    }

    /// Changes one task's entry, keeping the indices in step with it.
    fn change<R>(&mut self, task: TaskId, f: impl FnOnce(&mut TaskEntry) -> R) -> Option<R> {
        let entry = self.entries.get_mut(&task)?;
        let before = entry.standing();
        let out = f(entry);
        let after = entry.standing();
        if before != after {
            self.leave(task, before);
            self.enter(task, after);
        }
        Some(out)
    }

    fn enter(&mut self, task: TaskId, standing: Standing) {
        match standing {
            Standing::Pending => {
                self.pending.insert(task);
            }
            Standing::Blocked => self.blocked += 1,
            Standing::Running => self.running += 1,
            Standing::Retired => {}
        }
    }

    fn leave(&mut self, task: TaskId, standing: Standing) {
        match standing {
            Standing::Pending => {
                self.pending.remove(&task);
            }
            Standing::Blocked => self.blocked -= 1,
            Standing::Running => self.running -= 1,
            Standing::Retired => {}
        }
    }

    /// Records the signal a task stopped on.
    fn note_fault(&mut self, task: TaskId, signal: u32) {
        self.change(task, |entry| entry.faulted_on = Some(signal));
    }

    /// The node a task belongs to, where this run still knows.
    ///
    /// `None` for a forgotten node, never node 0 (§8 rule 2).
    fn node_for(&self, task: TaskId) -> Option<NodeId> {
        self.entries.get(&task).and_then(|e| e.node)
    }

    /// The process a task belongs to, named by its first task.
    fn group_of(&self, task: TaskId) -> Option<TaskId> {
        self.entries.get(&task).map(|e| e.group)
    }

    fn has_tasks(&self, node: NodeId) -> bool {
        self.per_node.get(&node).is_some_and(|n| *n > 0)
    }

    /// Hands each released task the value its blocking call returns.
    ///
    /// Only blocked tasks are released, so a pending task keeps its answer and an ended one stays
    /// gone.
    fn release(&mut self, wakes: impl IntoIterator<Item = Wake>) {
        for released in wakes {
            self.change(released.task, |entry| {
                if entry.slot == Slot::Blocked {
                    entry.slot = Slot::Pending(Resume::Emulate { ret: released.ret });
                }
            });
        }
    }

    fn set_slot(&mut self, task: TaskId, slot: Slot) {
        self.change(task, |entry| entry.slot = slot);
    }

    /// Takes the action a pending task resumes with, leaving it out of both queues.
    fn take_pending(&mut self, task: TaskId) -> Option<Resume> {
        self.change(task, |entry| match std::mem::replace(&mut entry.slot, Slot::Out) {
            Slot::Pending(resume) => Some(resume),
            other => {
                entry.slot = other;
                None
            }
        })
        .flatten()
    }

    fn unblock(&mut self, task: TaskId) {
        self.change(task, |entry| {
            if entry.slot == Slot::Blocked {
                entry.slot = Slot::Out;
            }
        });
    }

    fn take_faulted(&mut self, task: TaskId) -> Option<u32> {
        self.change(task, |e| e.faulted_on.take()).flatten()
    }

    fn is_retired(&self, task: TaskId) -> bool {
        self.entries.get(&task).is_some_and(|e| e.retired && e.node.is_none())
    }

    /// Whether anything is still owed a turn: a task ready, or one waiting on the world.
    fn anything_outstanding(&self) -> bool {
        debug_assert!(self.in_step(), "the task indices drifted from the entries");
        !self.pending.is_empty() || self.blocked > 0
    }

    /// Whether a task the host is executing still owes the loop its next stop.
    ///
    /// A retired task does not; its reports are dropped.
    fn in_flight(&self) -> bool {
        debug_assert!(self.in_step(), "the task indices drifted from the entries");
        self.running > 0
    }

    /// Whether the indices say what a scan of the entries says; checked in debug builds, where
    /// every test run is the proof.
    fn in_step(&self) -> bool {
        let standing = |s| self.entries.values().filter(|e| e.standing() == s).count();
        let pending: BTreeSet<TaskId> = self
            .entries
            .iter()
            .filter(|(_, e)| e.standing() == Standing::Pending)
            .map(|(t, _)| *t)
            .collect();
        pending == self.pending
            && standing(Standing::Blocked) == self.blocked
            && standing(Standing::Running) == self.running
    }

    fn schedulable(&self, task: TaskId) -> bool {
        // A task whose node is gone is not runnable.
        let Some(node) = self.node_for(task) else {
            return false;
        };
        matches!(self.node_state.get(&node), Some(NodeLifecycle::Running))
    }

    /// The pending tasks a scheduler may choose from, in identifier order.
    fn runnable_into(&self, out: &mut Vec<TaskId>) {
        out.clear();
        out.extend(self.pending.iter().copied().filter(|t| self.schedulable(*t)));
    }

    /// The tasks of a node, in identifier order.
    fn of_node(&self, node: NodeId) -> impl Iterator<Item = (TaskId, &TaskEntry)> {
        self.entries.iter().filter(move |(_, e)| e.node == Some(node)).map(|(t, e)| (*t, e))
    }

    /// Takes a task off its node's books for good, as when its node ended around it.
    fn retire(&mut self, task: TaskId) {
        let node = self.change(task, |entry| {
            entry.slot = Slot::Out;
            entry.faulted_on = None;
            entry.retired = true;
            entry.node.take()
        });
        if let Some(node) = node.flatten()
            && let Some(n) = self.per_node.get_mut(&node)
        {
            *n -= 1;
        }
    }

    /// Forgets a task that has ended, and marks its node dead once its last task is gone, so a
    /// later `Restart` composes with a clean exit as well as with a kill.
    fn note_task_gone(&mut self, task: TaskId) {
        let Some(entry) = self.entries.remove(&task) else {
            return;
        };
        self.leave(task, entry.standing());
        if let Some(node) = entry.node
            && let Some(n) = self.per_node.get_mut(&node)
        {
            *n -= 1;
            if *n == 0 {
                self.node_state.insert(node, NodeLifecycle::Dead);
            }
        }
    }
}

/// Drives one guest to completion.
pub fn run(inputs: RunInputs<'_, '_>) -> Result<RunReport, KernelError> {
    let RunInputs { tape, backend, world, policy, oracle, sink, observer, specs, config, arming } =
        inputs;
    let mut run = Run {
        backend,
        world,
        policy,
        oracle,
        sink,
        observer,
        specs,
        config,
        clock: Clock::new(),
        cursor: tape.cursor().keeping(config.draws),
        next_event: EventId::ZERO,
        recorded: Recorded::new(),
        tasks: Tasks::default(),
        last_exit: None,
        killed_by: None,
        last_reason: SwitchReason::SyscallBoundary,
        runnable: Vec::new(),
        metrics: RunMetrics::default(),
        reported_at: 0,
        handling: None,
        arming,
        armed: Vec::new(),
    };
    let ended = run.start().and_then(|()| run.drive());
    match ended {
        Ok(stop_reason) => Ok(run.finish(stop_reason)),
        Err(KernelError::Invariant(what)) => Err(run.locate(what)),
        Err(other) => Err(other),
    }
}

/// One run in progress: the ports it drives and the state the loop keeps between stops.
struct Run<'a, 't> {
    backend: &'a mut dyn DeterminismBackend,
    world: &'a mut dyn WorldModel,
    policy: &'a mut dyn SchedulePolicy,
    oracle: &'a mut dyn Oracle,
    sink: &'a mut dyn EventSink,
    observer: Option<&'a mut dyn Observer>,
    specs: &'a [GuestSpec],
    config: RunConfig,
    clock: Clock,
    cursor: TapeCursor<'t>,
    next_event: EventId,
    recorded: Recorded,
    tasks: Tasks,
    /// The exit status the run is judged on: the first failure, else the first exit.
    last_exit: Option<i32>,
    killed_by: Option<u32>,
    /// Why the previous running task's turn ended; stamped on the next switch event.
    last_reason: SwitchReason,
    /// Rebuilt each iteration in a kept buffer, avoiding an allocation per syscall.
    runnable: Vec<TaskId>,
    metrics: RunMetrics,
    /// The event count at the last progress report.
    reported_at: u64,
    /// The task whose stop is being handled, for saying where an invariant failed.
    handling: Option<TaskId>,
    /// What the run may arm.
    arming: Arming<'a>,
    /// The fault points armed on each node, by index, drawn at the start.
    armed: Vec<Vec<String>>,
}

/// What the loop does when nothing is runnable.
enum Idle {
    /// Time moved to the next deadline; go round again.
    Advanced,
    /// The run is over.
    Ended(StopReason),
}

impl Run<'_, '_> {
    /// Refuses a run that cannot be carried out, then gives every node its clock and first task.
    fn start(&mut self) -> Result<(), KernelError> {
        // An ending node outside the cluster is refused here (§8 rule 6).
        if let Some(node) = self.config.ends_with
            && node.get() as usize >= self.specs.len()
        {
            return Err(KernelError::Invariant("ends_with names a node this run does not have"));
        }
        if self.specs.is_empty() {
            return Err(KernelError::Invariant("a run needs at least one node"));
        }
        // The arming draw comes first, always eight bytes, so the tape layout is the same for every
        // scenario. See `docs/API.md` §5.4.
        let mut drawn = [0u8; 8];
        self.cursor.draw_into(&mut drawn, DrawSite::SdkFaultPoint);
        let seed = u64::from_le_bytes(drawn);
        self.armed = (0..self.specs.len() as u32)
            .map(|node| {
                if self.arming.except == Some(NodeId(node)) {
                    return Vec::new();
                }
                self.arming
                    .names
                    .iter()
                    .filter(|name| armed(seed, node, name, self.arming.odds))
                    .cloned()
                    .collect()
            })
            .collect();
        // Every node gets its own skewed clock before anything runs.
        for index in 0..self.specs.len() {
            let node = NodeId(index as u32);
            let node_clock = draw_node_clock(&mut self.cursor, self.config.max_clock_skew)?;
            self.clock.set_node_clock(node, node_clock);
            let spec = self.spec_for(node)?;
            self.say_armed(node)?;
            let first = self.backend.spawn(node, &spec)?;
            self.tasks.add(first, node, Slot::Pending(Resume::Continue), first);
            self.tasks.node_state.insert(node, NodeLifecycle::Running);
        }
        Ok(())
    }

    /// A node's program as it is started: its spec, with the fault points armed on it.
    ///
    /// The same at every start, restarts included; arming is drawn once per run.
    fn spec_for(&self, node: NodeId) -> Result<GuestSpec, KernelError> {
        let mut spec = self
            .specs
            .get(node.get() as usize)
            .ok_or(KernelError::Invariant("starting a node that the run does not have"))?
            .clone();
        if let Some(names) = self.armed.get(node.get() as usize).filter(|n| !n.is_empty()) {
            spec.env.push((ARMED_VARIABLE.to_owned(), names.join(",")));
        }
        Ok(spec)
    }

    /// Says which fault points a node starts with armed, at every start of it.
    fn say_armed(&mut self, node: NodeId) -> Result<(), KernelError> {
        let Some(names) = self.armed.get(node.get() as usize).filter(|n| !n.is_empty()) else {
            return Ok(());
        };
        let message =
            format!("node {} starts with fault points armed: {}", node.get(), names.join(", "));
        self.emit(node, TaskId::ZERO, EventKind::Diagnostic { code: ARMING.to_owned(), message })
    }

    /// The loop: fire what is due, check every ending, pick, run, and answer the stop.
    fn drive(&mut self) -> Result<StopReason, KernelError> {
        loop {
            // A task in flight is waited for before anything else is decided, so host delivery
            // order (such as a thread group's exits) never reaches the run.
            if self.tasks.in_flight() {
                let stop = self.backend.next_stop()?;
                if !self.take(stop)? {
                    return Ok(StopReason::GuestFinished);
                }
                continue;
            }
            self.fire_due()?;
            if let Some(ending) = self.ending() {
                return Ok(ending);
            }
            self.tasks.runnable_into(&mut self.runnable);
            if self.runnable.is_empty() {
                match self.when_idle() {
                    Idle::Advanced => continue,
                    Idle::Ended(reason) => return Ok(reason),
                }
            }
            let stop = self.dispatch()?;
            if !self.take(stop)? {
                return Ok(StopReason::GuestFinished);
            }
        }
    }

    /// Answers a stop the backend reported; `false` where it reported that nothing is left.
    fn take(&mut self, stop: Option<Stop>) -> Result<bool, KernelError> {
        let Some(stop) = stop else {
            // `None` with work outstanding is an invariant failure.
            if self.tasks.anything_outstanding() || self.tasks.in_flight() {
                return Err(KernelError::Invariant(
                    "the backend reported every task finished while tasks were still to run",
                ));
            }
            return Ok(false);
        };
        self.handling = Some(stop.task);
        self.on_stop(&stop)?;
        self.handling = None;
        self.report_progress(false);
        Ok(true)
    }

    /// Fires every deadline at or before now, before anything at this tick is scheduled.
    fn fire_due(&mut self) -> Result<(), KernelError> {
        if !self.world.next_deadline().is_some_and(|t| t <= self.clock.now()) {
            return Ok(());
        }
        let due = {
            let mut ctx = DeadlineCtx::new(
                &mut self.clock,
                &mut self.cursor,
                &mut *self.backend,
                &mut *self.sink,
                &mut self.next_event,
                &mut self.recorded,
            );
            self.world.on_deadline(&mut ctx)?
        };
        self.apply_due(due)?;
        // A due entry surviving the call would spin forever, so it is an invariant failure.
        if self.world.next_deadline().is_some_and(|t| t <= self.clock.now()) {
            return Err(KernelError::Invariant("a due deadline survived on_deadline"));
        }
        Ok(())
    }

    /// The ending the run has reached, checked in order: a named node, the horizon, the budgets,
    /// then a requested break.
    fn ending(&self) -> Option<StopReason> {
        // Before the horizon. A node with no tasks is done however it ended.
        if self.config.ends_with.is_some_and(|node| {
            matches!(self.tasks.node_state.get(&node), Some(NodeLifecycle::Dead))
        }) {
            return Some(StopReason::NodeFinished);
        }
        // Before the budget, so the budget never shadows the horizon.
        if self.config.horizon.is_some_and(|at| self.clock.now() >= at) {
            return Some(StopReason::HorizonReached);
        }
        if self.clock.now() >= self.config.max_ticks {
            return Some(StopReason::TickBudget);
        }
        if self.next_event.get() >= self.config.max_events {
            return Some(StopReason::EventBudget);
        }
        // Here every task is stopped and none is half-answered.
        if self.config.break_at.is_some_and(|at| self.next_event >= at) {
            return Some(StopReason::BreakRequested);
        }
        None
    }

    /// Nothing is runnable: move time to the next deadline, or end the run.
    fn when_idle(&mut self) -> Idle {
        // An armed deadline outranks both endings, unless it lies at or past the tick budget.
        let ceiling =
            self.config.horizon.unwrap_or(self.config.max_ticks).min(self.config.max_ticks);
        match self.world.next_deadline() {
            Some(due) if due < ceiling => {
                self.clock.advance(Tick(due.get().saturating_sub(self.clock.now().get())));
                Idle::Advanced
            }
            // A next timer past the horizon ends the run at the horizon, with the clock advanced
            // to it. With nothing armed it is a deadlock, horizon or not.
            Some(_) if self.config.horizon.is_some_and(|at| at <= self.config.max_ticks) => {
                let at = self.config.horizon.unwrap_or(self.clock.now());
                self.clock.advance(Tick(at.get().saturating_sub(self.clock.now().get())));
                Idle::Ended(StopReason::HorizonReached)
            }
            _ => Idle::Ended(if self.tasks.anything_outstanding() {
                StopReason::Deadlock
            } else {
                StopReason::GuestFinished
            }),
        }
    }

    /// Picks a runnable task, narrates the switch if there is one, and runs it to its next stop.
    fn dispatch(&mut self) -> Result<Option<Stop>, KernelError> {
        #[cfg(feature = "chaos-hash-iteration")]
        {
            self.runnable = crate::chaos::in_hash_order(&self.runnable);
        }
        // Draws made while choosing belong to the event that switch produces.
        self.cursor.set_event(self.next_event);
        #[cfg(feature = "chaos-delivery-tie")]
        let chosen = crate::chaos::first_to_arrive(&self.runnable);
        #[cfg(not(feature = "chaos-delivery-tie"))]
        let chosen = self.policy.pick(&self.runnable, &mut self.cursor)?;
        self.metrics.decisions += 1;
        let action = self
            .tasks
            .take_pending(chosen)
            .ok_or(KernelError::Invariant("scheduler chose a task that is not stopped"))?;

        if self.tasks.last_run != Some(chosen) {
            // The first dispatch is not a switch; every later change is, including after an exit.
            if let Some(from) = self.tasks.last_run {
                let node = self
                    .tasks
                    .node_for(chosen)
                    .ok_or(KernelError::Invariant("scheduled a task with no node"))?;
                let reason = self.last_reason;
                self.emit(node, chosen, EventKind::Schedule { from, to: chosen, reason })?;
                self.metrics.switches += 1;
            }
            self.tasks.last_run = Some(chosen);
        }
        self.last_reason = SwitchReason::SyscallBoundary;

        // What a read of the clock without a stop returns until the next stop: the node's clock
        // now, as a `clock_gettime` answered here would have been.
        let node = self
            .tasks
            .node_for(chosen)
            .ok_or(KernelError::Invariant("resumed a task with no node"))?;
        self.backend.present_clock(
            chosen,
            self.clock.now_on(node).get(),
            self.clock.wall_on(node).get(),
            CLOCK_READ_COST.get(),
        )?;
        self.backend.resume(chosen, action)?;
        Ok(self.backend.next_stop()?)
    }

    /// Answers one stop: the kernel's own bookkeeping first, then the world's interpretation.
    fn on_stop(&mut self, stop: &Stop) -> Result<(), KernelError> {
        match &stop.kind {
            StopKind::TaskCreated { parent, thread, .. } => {
                return self.on_created(stop.task, *parent, *thread);
            }
            StopKind::TaskExited { status } => return self.on_exited(stop.task, status),
            StopKind::Signal { signal } => self.tasks.note_fault(stop.task, *signal),
            _ => {}
        }

        let node = self
            .tasks
            .node_for(stop.task)
            .ok_or(KernelError::Invariant("a stop arrived for a task with no node"))?;
        // The clock reads made without a stop since the resume are charged before the stop is
        // answered, as each would have been had it stopped: the world moves until the node's own
        // clock reads what the last of them returned and one cost more, so a drifting clock never
        // reads less after the stop than it did before it.
        let reads = self.backend.clock_reads();
        if reads > 0 {
            let at = self.clock.deadline_on(node, reads.saturating_mul(CLOCK_READ_COST.get()));
            self.clock.advance(Tick(at.get().saturating_sub(self.clock.now().get())));
        }
        let effects = {
            let at = DeadlineCtx::new(
                &mut self.clock,
                &mut self.cursor,
                &mut *self.backend,
                &mut *self.sink,
                &mut self.next_event,
                &mut self.recorded,
            );
            let mut ctx = StopCtx::new(at, node, stop.task);
            self.world.handle(stop, &mut ctx)?
        };
        self.metrics.stops += 1;

        let mut advance = effects.advance;
        if let (StopKind::BudgetExhausted { consumed }, Some(rate)) =
            (&stop.kind, self.config.ticks_per_progress_unit)
        {
            advance = advance.saturating_add(Tick(consumed.saturating_mul(rate)));
        }
        self.clock.advance(advance);
        let spent = self.metrics.ticks_by_task.entry(stop.task).or_insert(Tick::ZERO);
        *spent = spent.saturating_add(advance);

        if effects.yielded {
            self.policy.yielded(stop.task);
            if self.tasks.last_run == Some(stop.task) {
                self.last_reason = SwitchReason::Yield;
            }
        }
        self.tasks.release(effects.wake);
        match effects.state {
            // Ended here, while how it ended is still known.
            TaskState::Runnable(Resume::Terminate { signal }) => {
                self.backend.resume(stop.task, Resume::Terminate { signal })?;
                // No signal stopped it, so the world names the cause.
                let died_of = signal.or_else(|| self.tasks.take_faulted(stop.task));
                let status = match died_of {
                    Some(signal) => {
                        self.killed_by = Some(signal);
                        ExitStatus::Signalled(signal)
                    }
                    None => ExitStatus::Exited(0),
                };
                self.emit(node, stop.task, EventKind::TaskExited { status })?;
                if self.tasks.last_run == Some(stop.task) {
                    self.last_reason = SwitchReason::TaskExit;
                }
                self.tasks.note_task_gone(stop.task);
                self.policy.forget(stop.task);
                self.tasks.release(self.world.task_exited(stop.task));
            }
            TaskState::Runnable(resume) => {
                if matches!(stop.kind, StopKind::BudgetExhausted { .. })
                    && self.tasks.last_run == Some(stop.task)
                {
                    self.last_reason = SwitchReason::BudgetExhausted;
                }
                self.tasks.set_slot(stop.task, Slot::Pending(resume));
            }
            TaskState::Blocked => {
                if self.tasks.last_run == Some(stop.task) {
                    self.last_reason = SwitchReason::Blocked;
                }
                self.tasks.set_slot(stop.task, Slot::Blocked);
            }
        }
        self.apply_node_ops(&effects.node_ops)
    }

    /// A task appeared; it and its event belong to the creator's node, and to its creator's process
    /// where it is a thread of it, or to a process of its own.
    fn on_created(
        &mut self,
        task: TaskId,
        parent: TaskId,
        thread: bool,
    ) -> Result<(), KernelError> {
        let node = self
            .tasks
            .node_for(parent)
            .ok_or(KernelError::Invariant("a task was created by one this run has no node for"))?;
        let group = if thread { self.tasks.group_of(parent).unwrap_or(parent) } else { task };
        self.tasks.add(task, node, Slot::Pending(Resume::Continue), group);
        self.world.task_created(task, parent);
        self.emit(node, task, EventKind::TaskSpawned { parent })?;
        self.tasks.set_slot(parent, Slot::Pending(Resume::Continue));
        Ok(())
    }

    /// A task's exit, as the backend reports it.
    fn on_exited(&mut self, task: TaskId, status: &GuestExit) -> Result<(), KernelError> {
        self.tasks.unblock(task);
        self.tasks.release(self.world.task_exited(task));
        // A task retired with its node was accounted for then; its backend exit is dropped.
        if self.tasks.is_retired(task) {
            self.tasks.note_task_gone(task);
            self.policy.forget(task);
            return Ok(());
        }
        let node = self.tasks.node_for(task);
        let converted = match status {
            GuestExit::Exited(c) => {
                // The first failure is kept, since a caller often outlives the node it drove.
                if *c != 0 || self.last_exit.is_none() {
                    self.last_exit = Some(*c);
                }
                ExitStatus::Exited(*c)
            }
            GuestExit::Signalled(s) => {
                // Killed tasks left the books already.
                let signal = self.tasks.take_faulted(task).unwrap_or(*s);
                self.killed_by = Some(signal);
                ExitStatus::Signalled(signal)
            }
        };
        let node =
            node.ok_or(KernelError::Invariant("a task exited with no node to file it on"))?;
        self.emit(node, task, EventKind::TaskExited { status: converted })?;
        if self.tasks.last_run == Some(task) {
            self.last_reason = SwitchReason::TaskExit;
        }
        self.tasks.note_task_gone(task);
        self.policy.forget(task);
        Ok(())
    }

    /// Applies what firing deadlines produced.
    fn apply_due(&mut self, due: DueEffects) -> Result<(), KernelError> {
        self.tasks.release(due.wake);
        self.apply_node_ops(&due.node_ops)
    }

    /// Carries out the world's node decisions: the task bookkeeping and backend calls only the
    /// kernel can make. The narrating events were already emitted by the world.
    fn apply_node_ops(&mut self, ops: &[NodeOp]) -> Result<(), KernelError> {
        for op in ops {
            match *op {
                // The host ended the caller's process; nothing is killed or narrated. Another
                // process of the node, a child it forked, lives on as on Linux.
                NodeOp::Ended { node, by } => {
                    let group = self.tasks.group_of(by);
                    let gone: Vec<TaskId> = self
                        .tasks
                        .of_node(node)
                        .filter(|(t, e)| *t != by && Some(e.group) == group)
                        .map(|(t, _)| t)
                        .collect();
                    for task in gone {
                        // Off the books, so the node is `Dead` once the spared task goes.
                        self.tasks.retire(task);
                        self.policy.forget(task);
                    }
                    if !self.tasks.has_tasks(node) {
                        self.tasks.node_state.insert(node, NodeLifecycle::Dead);
                    }
                }
                NodeOp::Kill(node) => {
                    if !matches!(
                        self.tasks.node_state.get(&node),
                        Some(NodeLifecycle::Running | NodeLifecycle::Frozen)
                    ) {
                        return Err(KernelError::Invariant("killing a node that is not alive"));
                    }
                    let doomed: Vec<TaskId> = self.tasks.of_node(node).map(|(t, _)| t).collect();
                    for task in doomed {
                        self.backend.kill(task)?;
                        // Off the books now, since no exit will be reported for it.
                        self.tasks.note_task_gone(task);
                        self.policy.forget(task);
                        // No switch event: the world's NodeState already explains it.
                        if self.tasks.last_run == Some(task) {
                            self.tasks.last_run = None;
                        }
                    }
                    // Recorded as `NodeState::Dead`.
                    self.tasks.node_state.insert(node, NodeLifecycle::Dead);
                }
                NodeOp::Freeze(node) => {
                    if self.tasks.node_state.get(&node) != Some(&NodeLifecycle::Running) {
                        return Err(KernelError::Invariant("freezing a node that is not running"));
                    }
                    self.tasks.node_state.insert(node, NodeLifecycle::Frozen);
                }
                NodeOp::Thaw(node) => {
                    if self.tasks.node_state.get(&node) != Some(&NodeLifecycle::Frozen) {
                        return Err(KernelError::Invariant("thawing a node that is not frozen"));
                    }
                    self.tasks.node_state.insert(node, NodeLifecycle::Running);
                }
                NodeOp::Restart(node) => {
                    if self.tasks.node_state.get(&node) != Some(&NodeLifecycle::Dead) {
                        return Err(KernelError::Invariant("restarting a node that is not dead"));
                    }
                    let spec = self.spec_for(node)?;
                    self.say_armed(node)?;
                    let first = self.backend.spawn(node, &spec)?;
                    self.tasks.add(first, node, Slot::Pending(Resume::Continue), first);
                    self.tasks.node_state.insert(node, NodeLifecycle::Running);
                }
            }
        }
        Ok(())
    }

    fn emit(&mut self, node: NodeId, task: TaskId, kind: EventKind) -> Result<(), KernelError> {
        let event =
            bohr_trace::Event { id: self.next_event, tick: self.clock.now(), node, task, kind };
        self.sink.emit(&event)?;
        self.next_event = EventId(self.next_event.get() + 1);
        Ok(())
    }

    /// Offers the observer the run so far, every [`PROGRESS_EVERY`] events and once at the end.
    fn report_progress(&mut self, last: bool) {
        let events = self.next_event.get();
        if !last && events < self.reported_at.saturating_add(PROGRESS_EVERY) {
            return;
        }
        self.reported_at = events;
        if let Some(observer) = self.observer.as_deref_mut() {
            observer.progress(&Progress {
                events,
                ticks: self.clock.now(),
                decisions: self.metrics.decisions,
                finished: last,
            });
        }
    }

    /// Judges the run and hands back what it produced.
    fn finish(mut self, stop_reason: StopReason) -> RunReport {
        self.report_progress(true);
        let digest = self.sink.digest();
        let outcome = self.judge(digest, stop_reason);
        RunReport {
            digest,
            events: self.next_event.get(),
            ticks: self.clock.now(),
            stop_reason,
            outcome,
            violation_event: self.recorded.violation_event(),
            adversity: self.recorded.adversity(),
            journal: self.cursor.into_journal(),
            metrics: self.metrics,
        }
    }

    /// Turns the oracle's verdict into the outcome recorded in a bundle.
    ///
    /// A horizon or ending node is judged in full. A budget stop is a hang, unless a law, a
    /// property or an assertion was already broken: what the run raised before it stopped holds
    /// whether or not it went on. It is judged with no exit and no signal, so a node's earlier
    /// crash, which the panel tells first, does not hide what was broken after it.
    fn judge(&mut self, digest: Digest, stop_reason: StopReason) -> Outcome {
        let now = self.clock.now().get();
        let events = self.next_event.get();
        let hung = match stop_reason {
            StopReason::TickBudget => Some(format!("no progress by tick {now}")),
            // Names the horizon, if any, alongside the budget that was reached.
            StopReason::EventBudget => Some(match self.config.horizon {
                Some(at) => format!(
                    "event budget reached at {events}, short of the horizon at tick {}",
                    at.get()
                ),
                None => format!("event budget reached at {events}"),
            }),
            _ => None,
        };
        if let Some(hung) = hung {
            let (exit, killed_by) = (self.last_exit.take(), self.killed_by.take());
            let judged = self.judged(digest, events);
            (self.last_exit, self.killed_by) = (exit, killed_by);
            return match judged {
                Outcome::Clean => Outcome::GuestHung(hung),
                found => found,
            };
        }
        match stop_reason {
            // A deadlock is the guest's defect; the oracle never sees it.
            StopReason::Deadlock => {
                return Outcome::GuestHung(format!("every task is waiting at tick {now}"));
            }
            // A requested stop is neither a hang nor a clean finish.
            StopReason::BreakRequested => {
                return Outcome::StoppedOnRequest(format!("stopped at event {events}"));
            }
            StopReason::TickBudget
            | StopReason::EventBudget
            | StopReason::GuestFinished
            | StopReason::HorizonReached
            | StopReason::NodeFinished => {}
        }
        self.judged(digest, events)
    }

    /// The oracle's verdict on the run as it stands.
    fn judged(&mut self, digest: Digest, events: u64) -> Outcome {
        let outcome = RunOutcome {
            digest,
            events,
            ticks: self.clock.now(),
            exit: self.last_exit,
            killed_by: self.killed_by,
            diagnostics: self.recorded.diagnostics().to_vec(),
            // Filled after the run by whoever holds the world.
            projections: Vec::new(),
        };
        match self.oracle.judge(&outcome) {
            Verdict::Finding(o) => o,
            Verdict::Clean | Verdict::Agree | Verdict::Skipped => Outcome::Clean,
        }
    }

    /// An invariant failure, with where in the run it happened.
    fn locate(&self, what: &'static str) -> KernelError {
        let task = match self.handling.or(self.tasks.last_run) {
            Some(task) => format!("task {}", task.get()),
            None => "no task running".to_owned(),
        };
        KernelError::InvariantAt {
            what,
            event: self.next_event.get(),
            tick: self.clock.now().get(),
            task,
        }
    }
}

/// Draws the offset and drift one node's clock runs at.
///
/// Both come from the tape, so mutating them moves no other decision.
/// What a clock read without a stop costs the node's time: what the world charges a
/// `clock_gettime` it answers, so a read costs the same either way.
const CLOCK_READ_COST: Tick = Tick(1_000);

fn draw_node_clock(cursor: &mut TapeCursor<'_>, max_skew: Tick) -> Result<NodeClock, KernelError> {
    let offset = cursor.draw_range(max_skew.get().saturating_add(1), DrawSite::ClockSkew)?;

    let drift_span = u64::from(MAX_DRIFT_PPM) * 2 + 1;
    let drift = cursor.draw_range(drift_span, DrawSite::ClockSkew)? as i32 - MAX_DRIFT_PPM as i32;

    Ok(NodeClock::new(offset, drift))
}

#[cfg(test)]
mod tests {
    use bohr_backend::Resume;
    use bohr_types::{NodeId, TaskId};

    use super::{NodeLifecycle, Slot, Tasks};
    use crate::ports::Wake;

    /// The ready set, the waiting count and the running count follow every way a task moves:
    /// picked, blocked, woken, retired with its node, and gone.
    #[test]
    fn the_books_say_what_a_scan_would() {
        let mut tasks = Tasks::default();
        tasks.node_state.insert(NodeId(0), NodeLifecycle::Running);
        for t in 1..=3 {
            tasks.add(TaskId(t), NodeId(0), Slot::Pending(Resume::Continue), TaskId(t));
        }
        let mut ready = Vec::new();
        tasks.runnable_into(&mut ready);
        assert_eq!(ready, [TaskId(1), TaskId(2), TaskId(3)]);

        assert!(tasks.take_pending(TaskId(2)).is_some());
        assert!(tasks.in_flight());
        tasks.set_slot(TaskId(2), Slot::Blocked);
        assert!(!tasks.in_flight());
        tasks.runnable_into(&mut ready);
        assert_eq!(ready, [TaskId(1), TaskId(3)]);

        tasks.release([Wake { task: TaskId(2), ret: 0 }]);
        tasks.runnable_into(&mut ready);
        assert_eq!(ready, [TaskId(1), TaskId(2), TaskId(3)]);

        tasks.retire(TaskId(1));
        tasks.note_task_gone(TaskId(3));
        tasks.runnable_into(&mut ready);
        assert_eq!(ready, [TaskId(2)]);
        assert!(tasks.anything_outstanding());
        assert!(!tasks.in_flight(), "a retired task owes no stop");
        assert!(tasks.in_step());

        assert!(tasks.take_pending(TaskId(2)).is_some());
        tasks.note_task_gone(TaskId(2));
        assert!(!tasks.anything_outstanding());
        assert!(tasks.in_step());
    }
}
