//! The run loop: one guest driven from its first instruction to the ending the run is judged on.
//! It owns task bookkeeping and the order of everything; what a stop means belongs to the world.

use std::collections::BTreeMap;

use bohr_backend::{DeterminismBackend, GuestExit, GuestSpec, Resume, Stop, StopKind};
use bohr_tape::{DrawSite, Journal, Tape, TapeCursor};
use bohr_trace::{EventKind, ExitStatus, Outcome, SwitchReason};
use bohr_types::{Digest, EventId, NodeId, TaskId, Tick};

use crate::{
    KernelError, StopReason,
    clock::{Clock, MAX_DRIFT_PPM, NodeClock},
    ports::{
        DueEffects, EventSink, NodeOp, Observer, Oracle, Progress, Recorded, RunOutcome, TaskState,
        Verdict, Wake, WorldCtx, WorldModel,
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
    /// One direction only: the offset is drawn non-negative ([`crate::NodeClock`]), because a
    /// clock reading before the run began would have to be clamped and would then stand still
    /// at the start instead of running.
    pub max_clock_skew: Tick,
    /// An event to stop the world before, where a caller asked for one.
    ///
    /// The run stops at the first scheduling point at or after this event, with every task
    /// stopped — never inside a stop the world is still half-answering, where a return value has
    /// been decided and not yet applied and the registers do not mean what they say. Overshooting
    /// is therefore possible and is reported rather than hidden. See `docs/API.md` §7.3.
    pub break_at: Option<EventId>,
    /// Virtual time at which a run that was never going to end on its own is complete.
    ///
    /// `None` for a guest expected to finish, which is every guest until a scenario says otherwise
    /// — and with `None` this changes nothing about how a run proceeds, which is what keeps every
    /// stored bundle replaying to the digest it recorded.
    ///
    /// Distinct from `max_ticks` because the two answer opposite questions. `max_ticks` is "this
    /// guest is stuck"; a horizon is "this guest is a server, and we have seen enough". A guest
    /// that stops making progress before the horizon is still a deadlock, and one that runs past
    /// its budget without a horizon is still hung.
    pub horizon: Option<Tick>,
    /// The node whose ending is the run's ending, where a scenario names one.
    ///
    /// A cluster of a service and the thing that drives it has no ending of its own: the service
    /// goes on serving, and a horizon has to be guessed at generously enough to cover whatever the
    /// driver does. This says it exactly — the run is over when that node is — and what it buys is
    /// every tick the run would otherwise have spent watching a server talk to nobody.
    ///
    /// `None` for every scenario that does not name one, which is what keeps stored bundles
    /// replaying to the digest they recorded.
    pub ends_with: Option<NodeId>,
    /// Virtual time one unit of the guest's own computation costs, where the backend counts it.
    ///
    /// `None` keeps time moving only on what the world answers. With a rate, a stop on an
    /// exhausted budget also advances the clock by the units the guest consumed
    /// (`docs/ARCHITECTURE.md` §4.2).
    pub ticks_per_progress_unit: Option<u64>,
}

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
        }
    }
}

/// Virtual hours a run may reach before it is called hung.
///
/// A day and an hour, because a guest that sleeps for a day is a test that is supposed to finish
/// and a budget below it would refuse the very thing virtual time exists for. It catches sleeping
/// without end; a guest spinning without syscalls advances no virtual time at all and is caught by
/// the event budget instead.
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
    /// A slice rather than a map: node identity is dense and ordered by construction, which is
    /// what keeps the clock draws at the same tape offsets however many nodes there are.
    pub specs: &'a [GuestSpec],
    /// Bounds on the run.
    pub config: RunConfig,
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
    /// The event a report points a reader at, and it is not the same as the last event of a run
    /// that ended badly: a guest goes on running after an assertion fails, so the last event is
    /// wherever it happened to stop rather than where the answer is.
    pub violation_event: Option<EventId>,
    /// Draws made, in order.
    pub journal: Journal,
    /// How many faults actually landed on the guest.
    ///
    /// Reported because a campaign that injected nothing is indistinguishable from one that
    /// injected plenty and found nothing, and only this number separates them.
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
/// The kernel's own mirror of the lifecycle: the world decides transitions and narrates them; this
/// is only what the run loop needs to act on them.
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
    /// Waiting on the world. Not a scheduling candidate; every task here with nothing armed is a
    /// deadlock rather than a finish.
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
    /// The signal it last stopped on. A task the world ends after a fault is killed by us, so the
    /// status the host reports names our signal rather than the one the guest died of.
    faulted_on: Option<u32>,
    /// Taken off the books when its node ended. The backend still reports its exit afterwards, and
    /// that exit is expected rather than refused.
    retired: bool,
}

/// The loop's books: one entry per task the run knows, and what it knows about each node.
#[derive(Clone, Default)]
struct Tasks {
    entries: BTreeMap<TaskId, TaskEntry>,
    /// Entries that name each node, so "does this node still have tasks" is a lookup.
    per_node: BTreeMap<NodeId, usize>,
    node_state: BTreeMap<NodeId, NodeLifecycle>,
    last_run: Option<TaskId>,
}

impl Tasks {
    /// Enters a task the run has just learned of.
    fn add(&mut self, task: TaskId, node: NodeId, slot: Slot) {
        *self.per_node.entry(node).or_default() += 1;
        self.entries
            .insert(task, TaskEntry { node: Some(node), slot, faulted_on: None, retired: false });
    }

    /// The node a task belongs to, where this run still knows.
    ///
    /// **An option rather than node nought**: a task whose node this run has forgotten is a task
    /// nothing should be filed under (§8 rule 2).
    fn node_for(&self, task: TaskId) -> Option<NodeId> {
        self.entries.get(&task).and_then(|e| e.node)
    }

    fn has_tasks(&self, node: NodeId) -> bool {
        self.per_node.get(&node).is_some_and(|n| *n > 0)
    }

    /// Hands each released task the value its blocking call returns.
    ///
    /// Only a task that is actually blocked is released. A wake naming a pending task would
    /// overwrite the answer the world already decided for its syscall, and one naming a task that
    /// has ended would put back a task the backend no longer has.
    fn release(&mut self, wakes: impl IntoIterator<Item = Wake>) {
        for released in wakes {
            if let Some(entry) = self.entries.get_mut(&released.task)
                && entry.slot == Slot::Blocked
            {
                entry.slot = Slot::Pending(Resume::Emulate { ret: released.ret });
            }
        }
    }

    fn set_slot(&mut self, task: TaskId, slot: Slot) {
        if let Some(entry) = self.entries.get_mut(&task) {
            entry.slot = slot;
        }
    }

    /// Takes the action a pending task resumes with, leaving it out of both queues.
    fn take_pending(&mut self, task: TaskId) -> Option<Resume> {
        let entry = self.entries.get_mut(&task)?;
        match std::mem::replace(&mut entry.slot, Slot::Out) {
            Slot::Pending(resume) => Some(resume),
            other => {
                entry.slot = other;
                None
            }
        }
    }

    fn unblock(&mut self, task: TaskId) {
        if let Some(entry) = self.entries.get_mut(&task)
            && entry.slot == Slot::Blocked
        {
            entry.slot = Slot::Out;
        }
    }

    fn take_faulted(&mut self, task: TaskId) -> Option<u32> {
        self.entries.get_mut(&task).and_then(|e| e.faulted_on.take())
    }

    fn is_retired(&self, task: TaskId) -> bool {
        self.entries.get(&task).is_some_and(|e| e.retired && e.node.is_none())
    }

    /// Whether anything is still owed a turn: a task ready, or one waiting on the world.
    fn anything_outstanding(&self) -> bool {
        self.entries.values().any(|e| matches!(e.slot, Slot::Pending(_) | Slot::Blocked))
    }

    /// Whether a task the host is executing still owes the loop its next stop.
    ///
    /// A retired task is not one: its node ended, and whatever the backend reports about it is
    /// dropped on arrival.
    fn in_flight(&self) -> bool {
        self.entries.values().any(|e| matches!(e.slot, Slot::Out) && !e.retired)
    }

    fn schedulable(&self, task: TaskId) -> bool {
        // A task whose node is gone is not a task to run, and the state of node nought is not an
        // answer about it.
        let Some(node) = self.node_for(task) else {
            return false;
        };
        matches!(self.node_state.get(&node), Some(NodeLifecycle::Running))
    }

    /// The pending tasks a scheduler may choose from, in identifier order.
    fn runnable_into(&self, out: &mut Vec<TaskId>) {
        out.clear();
        out.extend(
            self.entries
                .iter()
                .filter(|(_, e)| matches!(e.slot, Slot::Pending(_)))
                .map(|(t, _)| *t)
                .filter(|t| self.schedulable(*t)),
        );
    }

    /// The tasks of a node, in identifier order.
    fn of_node(&self, node: NodeId) -> impl Iterator<Item = (TaskId, &TaskEntry)> {
        self.entries.iter().filter(move |(_, e)| e.node == Some(node)).map(|(t, e)| (*t, e))
    }

    /// Takes a task off its node's books for good, as when its node ended around it.
    fn retire(&mut self, task: TaskId) {
        let Some(entry) = self.entries.get_mut(&task) else {
            return;
        };
        if let Some(node) = entry.node.take()
            && let Some(n) = self.per_node.get_mut(&node)
        {
            *n -= 1;
        }
        entry.slot = Slot::Out;
        entry.faulted_on = None;
        entry.retired = true;
    }

    /// Forgets a task that has ended, and marks its node dead once its last task is gone, so a
    /// later `Restart` composes with a clean exit as well as with a kill.
    fn note_task_gone(&mut self, task: TaskId) {
        let Some(entry) = self.entries.remove(&task) else {
            return;
        };
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
    let RunInputs { tape, backend, world, policy, oracle, sink, observer, specs, config } = inputs;
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
        cursor: tape.cursor(),
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
    /// Rebuilt from the books each iteration and kept across them: a fresh allocation per stop is
    /// a per-syscall cost.
    runnable: Vec<TaskId>,
    metrics: RunMetrics,
    /// The event count at the last progress report.
    reported_at: u64,
    /// The task whose stop is being handled, for saying where an invariant failed.
    handling: Option<TaskId>,
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
        // Refused here rather than never satisfied: a run told to end with node five in a cluster
        // of three would otherwise fall back to the horizon, and a scenario with a typo in it would
        // look like one whose node never finished. §8 rule 6: the boundary is where this is caught.
        if let Some(node) = self.config.ends_with
            && node.get() as usize >= self.specs.len()
        {
            return Err(KernelError::Invariant("ends_with names a node this run does not have"));
        }
        if self.specs.is_empty() {
            return Err(KernelError::Invariant("a run needs at least one node"));
        }
        // Every node gets a clock of its own before anything runs. Two machines never agree on the
        // time, and a run where they did would never reach the bugs that depend on them
        // disagreeing.
        for (index, spec) in self.specs.iter().enumerate() {
            let node = NodeId(index as u32);
            let node_clock = draw_node_clock(&mut self.cursor, self.config.max_clock_skew)?;
            self.clock.set_node_clock(node, node_clock);
            let first = self.backend.spawn(node, spec)?;
            self.tasks.add(first, node, Slot::Pending(Resume::Continue));
            self.tasks.node_state.insert(node, NodeLifecycle::Running);
        }
        Ok(())
    }

    /// The loop: fire what is due, check every ending, pick, run, and answer the stop.
    fn drive(&mut self) -> Result<StopReason, KernelError> {
        loop {
            // **A task the host is running is waited for before anything else is decided.** The
            // backend reports stops in the order the host delivers them, and a thread group that
            // ends reports each thread's death in an order no tape decides. Choosing another task,
            // moving time or ending the run while one is still in flight let that order through:
            // a process that failed from a thread other than its first ended the run before its own
            // exit arrived, and its status was lost on some runs and kept on others.
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
            // A backend answering `None` with work still outstanding turns a half-finished run
            // into a clean verdict, and the oracle then judges a prefix as the whole execution.
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

    /// Fires every deadline at or before now: what was armed for this tick happens before anything
    /// at this tick is scheduled, which is the only order a replay can reconstruct without
    /// guessing.
    fn fire_due(&mut self) -> Result<(), KernelError> {
        if !self.world.next_deadline().is_some_and(|t| t <= self.clock.now()) {
            return Ok(());
        }
        let due = {
            let mut ctx = WorldCtx::detached(
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
        // The contract is "fire everything due"; a due entry surviving the call would spin this
        // loop forever, so it is an invariant rather than a retry.
        if self.world.next_deadline().is_some_and(|t| t <= self.clock.now()) {
            return Err(KernelError::Invariant("a due deadline survived on_deadline"));
        }
        Ok(())
    }

    /// The ending the run has reached, checked in order: a named node, the horizon, the budgets,
    /// then a requested break.
    fn ending(&self) -> Option<StopReason> {
        // Before the horizon, so a scenario that named the ending it wanted is answered with that
        // ending. A node with no tasks left is done whether it ended or was ended.
        if self.config.ends_with.is_some_and(|node| {
            matches!(self.tasks.node_state.get(&node), Some(NodeLifecycle::Dead))
        }) {
            return Some(StopReason::NodeFinished);
        }
        // Before the budget, so a horizon can never be shadowed by a bound it sits under.
        if self.config.horizon.is_some_and(|at| self.clock.now() >= at) {
            return Some(StopReason::HorizonReached);
        }
        if self.clock.now() >= self.config.max_ticks {
            return Some(StopReason::TickBudget);
        }
        if self.next_event.get() >= self.config.max_events {
            return Some(StopReason::EventBudget);
        }
        // After the deadlines and before the scheduler picks: the one place in the loop where
        // every task is stopped and none is part-way through being answered.
        if self.config.break_at.is_some_and(|at| self.next_event >= at) {
            return Some(StopReason::BreakRequested);
        }
        None
    }

    /// Nothing is runnable: move time to the next deadline, or end the run.
    fn when_idle(&mut self) -> Idle {
        // An armed deadline outranks both endings: blocked and frozen tasks are waiting for it, and
        // even an empty cluster may owe a restart to a timer. A deadline at or past the tick budget
        // is one the run will never live to see, and chasing it would make every far-future timer
        // a hang.
        let ceiling =
            self.config.horizon.unwrap_or(self.config.max_ticks).min(self.config.max_ticks);
        match self.world.next_deadline() {
            Some(due) if due < ceiling => {
                self.clock.advance(Tick(due.get().saturating_sub(self.clock.now().get())));
                Idle::Advanced
            }
            // A cluster whose next timer is past the horizon has reached the horizon. Advancing to
            // it first makes the recorded tick count the span the scenario asked for, which is
            // what an oracle reasoning about a span of silence measures against.
            //
            // With **nothing** armed, no horizon saves it: nothing can ever happen again, and a
            // clean ending there would hide exactly the defect the deadlock check exists to find.
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
            // The first dispatch of a run moves execution from nowhere and is not a switch; every
            // later change of the running task is one, including the change after an exit.
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

        self.backend.resume(chosen, action)?;
        Ok(self.backend.next_stop()?)
    }

    /// Answers one stop: the kernel's own bookkeeping first, then the world's interpretation.
    fn on_stop(&mut self, stop: &Stop) -> Result<(), KernelError> {
        match &stop.kind {
            StopKind::TaskCreated { parent, .. } => return self.on_created(stop.task, *parent),
            StopKind::TaskExited { status } => return self.on_exited(stop.task, status),
            StopKind::Signal { signal } => {
                if let Some(entry) = self.tasks.entries.get_mut(&stop.task) {
                    entry.faulted_on = Some(*signal);
                }
            }
            _ => {}
        }

        let node = self
            .tasks
            .node_for(stop.task)
            .ok_or(KernelError::Invariant("a stop arrived for a task with no node"))?;
        let effects = {
            let mut ctx = WorldCtx::new(
                &mut self.clock,
                &mut self.cursor,
                &mut *self.backend,
                &mut *self.sink,
                &mut self.next_event,
                node,
                stop.task,
                &mut self.recorded,
            );
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
            // Ending a task is carried out here rather than left for the scheduler: the task is
            // gone once the backend has been told, and how it ended is written while still known.
            TaskState::Runnable(Resume::Terminate { signal }) => {
                self.backend.resume(stop.task, Resume::Terminate { signal })?;
                // A task that asked to die stops on no signal, so only the world can say what
                // took it.
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

    /// A task appeared: it belongs to the node that created it, and so does the event announcing
    /// it — reading the node from a task that does not exist yet would file every spawn under node
    /// zero.
    fn on_created(&mut self, task: TaskId, parent: TaskId) -> Result<(), KernelError> {
        let node = self
            .tasks
            .node_for(parent)
            .ok_or(KernelError::Invariant("a task was created by one this run has no node for"))?;
        self.tasks.add(task, node, Slot::Pending(Resume::Continue));
        self.world.task_created(task, parent);
        self.emit(node, task, EventKind::TaskSpawned { parent })?;
        self.tasks.set_slot(parent, Slot::Pending(Resume::Continue));
        Ok(())
    }

    /// A task's exit, as the backend reports it.
    fn on_exited(&mut self, task: TaskId, status: &GuestExit) -> Result<(), KernelError> {
        self.tasks.unblock(task);
        self.tasks.release(self.world.task_exited(task));
        // A task retired with its node was accounted for there, deliberately without an exit
        // event; its own exit still arrives from the backend, and filing it would report a task the
        // run has finished with twice over.
        if self.tasks.is_retired(task) {
            self.tasks.note_task_gone(task);
            self.policy.forget(task);
            return Ok(());
        }
        let node = self.tasks.node_for(task);
        let converted = match status {
            GuestExit::Exited(c) => {
                // A guest that failed is a finding whoever exits after it: a cluster's caller
                // routinely outlives the node it drove, and keeping the last status would let it
                // report its success as the run's.
                if *c != 0 || self.last_exit.is_none() {
                    self.last_exit = Some(*c);
                }
                ExitStatus::Exited(*c)
            }
            GuestExit::Signalled(s) => {
                // Never a task this loop killed: those leave the books when they are killed, and a
                // backend reports nothing more about them.
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
                // The host has already ended them; what is left is to stop expecting them. Nothing
                // is killed or narrated: the program said it was done.
                NodeOp::Ended { node, by } => {
                    let gone: Vec<TaskId> =
                        self.tasks.of_node(node).map(|(t, _)| t).filter(|t| *t != by).collect();
                    for task in gone {
                        // Off the node's books, so "does this node still have tasks" can answer
                        // no and the node reaches `Dead` once the spared task goes.
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
                        // Off the books now: a backend reports nothing more about a task it was
                        // told to kill, so a record kept for its exit would count against the node
                        // for ever and a restarted node could never end.
                        self.tasks.note_task_gone(task);
                        self.policy.forget(task);
                        // No switch event follows a kill: the world's NodeState narration already
                        // explains why execution moved, and the killed task is not a `from`.
                        if self.tasks.last_run == Some(task) {
                            self.tasks.last_run = None;
                        }
                    }
                    // Not recorded as the guest being killed: the signal came from the fault
                    // injector, and what the node was killed by is on the timeline as
                    // `NodeState::Dead`.
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
                    let spec = self.specs.get(node.get() as usize).ok_or(
                        KernelError::Invariant("restarting a node that was never started"),
                    )?;
                    let first = self.backend.spawn(node, spec)?;
                    self.tasks.add(first, node, Slot::Pending(Resume::Continue));
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
    /// A run cut short by a budget is a hang whatever the oracle thought: it only ever saw a
    /// prefix. A run that reached its horizon or its named node is judged in full.
    fn judge(&mut self, digest: Digest, stop_reason: StopReason) -> Outcome {
        let now = self.clock.now().get();
        let events = self.next_event.get();
        match stop_reason {
            StopReason::TickBudget => {
                return Outcome::GuestHung(format!("no progress by tick {now}"));
            }
            // The budget names the horizon when there was one: ten million events go by long
            // before twenty-five virtual hours, and the message should send a reader to the budget.
            StopReason::EventBudget => {
                return Outcome::GuestHung(match self.config.horizon {
                    Some(at) => format!(
                        "event budget reached at {events}, short of the horizon at tick {}",
                        at.get()
                    ),
                    None => format!("event budget reached at {events}"),
                });
            }
            // Every task waiting with nothing left to release any of them is the guest's own
            // defect, and one the oracle never gets to see.
            StopReason::Deadlock => {
                return Outcome::GuestHung(format!("every task is waiting at tick {now}"));
            }
            // Asked to stop is neither a hang nor a clean finish.
            StopReason::BreakRequested => {
                return Outcome::StoppedOnRequest(format!("stopped at event {events}"));
            }
            StopReason::GuestFinished | StopReason::HorizonReached | StopReason::NodeFinished => {}
        }
        let outcome = RunOutcome {
            digest,
            events,
            ticks: self.clock.now(),
            exit: self.last_exit,
            killed_by: self.killed_by,
            diagnostics: self.recorded.diagnostics().to_vec(),
            // Read after the run, by whoever holds the world; the kernel never sees one.
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
/// Both come from the tape, so a seed selects how far apart the machines in a run are, and a
/// mutation of those bytes moves them without disturbing any other decision.
fn draw_node_clock(cursor: &mut TapeCursor<'_>, max_skew: Tick) -> Result<NodeClock, KernelError> {
    let offset = cursor.draw_range(max_skew.get().saturating_add(1), DrawSite::ClockSkew)?;

    let drift_span = u64::from(MAX_DRIFT_PPM) * 2 + 1;
    let drift = cursor.draw_range(drift_span, DrawSite::ClockSkew)? as i32 - MAX_DRIFT_PPM as i32;

    Ok(NodeClock::new(offset, drift))
}
