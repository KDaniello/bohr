//! The run loop.

use std::collections::{BTreeMap, BTreeSet};

use bohr_backend::{DeterminismBackend, GuestExit, GuestSpec, Resume, StopKind};
use bohr_tape::{DrawSite, Journal, Tape};
use bohr_trace::{EventKind, ExitStatus, Outcome, SwitchReason};
use bohr_types::{Digest, EventId, NodeId, TaskId, Tick};

use crate::{
    clock::{Clock, NodeClock, MAX_DRIFT_PPM},
    ports::{
        DueEffects, EventSink, NodeOp, Oracle, Recorded, RunOutcome, TaskState, Verdict, WorldCtx,
        WorldModel,
    },
    schedule::SchedulePolicy,
    KernelError, StopReason,
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
    /// One direction, not "either way" as this used to say: the offset is drawn non-negative
    /// ([`crate::NodeClock`]), because a clock reading before the run began would have to be
    /// clamped and would then stand still at the start instead of running.
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

/// Everything the loop tracks about tasks and nodes, gathered so the node-op executor can borrow
/// it as one thing.
struct Tasks {
    /// A stopped task and the action it will resume with. The action is decided when the task
    /// stops, and applied when the scheduler next chooses it.
    pending: BTreeMap<TaskId, Resume>,
    /// Tasks waiting on the world. They are not scheduling candidates, and a run where every
    /// remaining task is here with nothing armed has deadlocked rather than finished.
    blocked: BTreeSet<TaskId>,
    node_of: BTreeMap<TaskId, NodeId>,
    node_state: BTreeMap<NodeId, NodeLifecycle>,
    /// The signal a task last stopped on. A task the world ends after a fault is killed by us, so
    /// the status the host reports names our signal rather than the one the guest died of.
    faulted_on: BTreeMap<TaskId, u32>,
    /// Tasks taken off the books when their node ended, whose own exits are still to come.
    ///
    /// The backend reports every thread's exit whatever the world decided about the node, and
    /// those reports arrive after the node is gone. Without this they are indistinguishable from
    /// an exit for a task nobody ever created, which is a defect in the tool and is refused as
    /// one -- so an ordinary threaded guest exiting ended the run with an internal error.
    retired: BTreeSet<TaskId>,
    /// Tasks this loop killed to carry out a node fault.
    ///
    /// Their exit arrives as an ordinary signalled exit, and without knowing better the run would
    /// be judged a crash — so a campaign injecting faults would report every one of its own faults
    /// back to itself as a finding. The timeline still records the signal, because it happened;
    /// what it does not do is decide the verdict.
    killed_by_us: BTreeSet<TaskId>,
    last_run: Option<TaskId>,
}

impl Tasks {
    /// The node a task belongs to, where this run still knows.
    ///
    /// **An option rather than node nought.** The fallback put the exits of fault-killed tasks and
    /// the spawns of their children on node nought's timeline: deterministic, so no digest gate
    /// ever saw it, and wrong in the way §8 rule 2 calls worse than a crash. A task whose node
    /// this run has forgotten is a task nothing should be filed under.
    fn node_for(&self, task: TaskId) -> Option<NodeId> {
        self.node_of.get(&task).copied()
    }

    fn schedulable(&self, task: TaskId) -> bool {
        let Some(node) = self.node_for(task) else {
            // A task whose node is gone is not a task to run. It cannot be scheduled onto a node
            // that no longer exists, and the state of node nought is not an answer about it.
            return false;
        };
        matches!(self.node_state.get(&node), Some(NodeLifecycle::Running) | None)
    }

    /// Marks a node dead once its last task is gone, so a later `Restart` composes with a clean
    /// exit as well as with a kill.
    fn note_task_gone(&mut self, task: TaskId) {
        if let Some(node) = self.node_of.remove(&task) {
            if !self.node_of.values().any(|n| *n == node) {
                self.node_state.insert(node, NodeLifecycle::Dead);
            }
        }
        // Both queues, because a task can be in either when it ends. One released by the world and
        // ending before the scheduler ever picked it sits in `pending` holding a resume nobody can
        // carry out: the backend has already reaped it, so the pick that follows names a task that
        // no longer exists. A process whose threads exit together is where this happens, because a
        // delivery can land on one of them in the same breath as the exit that takes it.
        self.pending.remove(&task);
        self.blocked.remove(&task);
        self.faulted_on.remove(&task);
        self.killed_by_us.remove(&task);
    }
}

/// Drives one guest to completion.
#[cfg_attr(feature = "chaos-delivery-tie", allow(unused_variables))]
pub fn run(inputs: RunInputs<'_, '_>) -> Result<RunReport, KernelError> {
    let RunInputs { tape, backend, world, policy, oracle, sink, specs, config } = inputs;
    // Refused here rather than never satisfied. A run told to end with node five in a cluster of
    // three used to fall back silently to the horizon, so a scenario with a typo in it looked like
    // a scenario whose node never finished. §8 rule 6: the boundary is where this is caught.
    if let Some(node) = config.ends_with {
        if node.get() as usize >= specs.len() {
            return Err(KernelError::Invariant("ends_with names a node this run does not have"));
        }
    }
    if specs.is_empty() {
        return Err(KernelError::Invariant("a run needs at least one node"));
    }

    let mut clock = Clock::new();
    let mut cursor = tape.cursor();
    let mut next_event = EventId::ZERO;
    let mut recorded = Recorded::new();

    let mut tasks = Tasks {
        pending: BTreeMap::new(),
        blocked: BTreeSet::new(),
        node_of: BTreeMap::new(),
        node_state: BTreeMap::new(),
        faulted_on: BTreeMap::new(),
        retired: BTreeSet::new(),
        killed_by_us: BTreeSet::new(),
        last_run: None,
    };
    let mut last_exit: Option<i32> = None;
    let mut killed_by: Option<u32> = None;
    // Why the previous running task's turn ended; stamped on the next switch event.
    let mut last_reason = SwitchReason::SyscallBoundary;
    // Rebuilt from `pending` each iteration and kept across them: the set changes by one or two
    // entries per stop, and a fresh allocation per stop is a per-syscall cost.
    let mut runnable: Vec<TaskId> = Vec::new();

    // Every node gets a clock of its own before anything runs. Two machines never agree on the
    // time, and a run where they did would never reach the bugs that depend on them disagreeing.
    for (index, spec) in specs.iter().enumerate() {
        let node = NodeId(index as u32);
        clock.set_node_clock(node, draw_node_clock(&mut cursor, config.max_clock_skew)?);
        let first = backend.spawn(node, spec)?;
        tasks.pending.insert(first, Resume::Continue);
        tasks.node_of.insert(first, node);
        tasks.node_state.insert(node, NodeLifecycle::Running);
    }

    let stop_reason = loop {
        // Deadlines first: what was armed for this tick happens before anything at this tick is
        // scheduled, which is the only order a replay can reconstruct without guessing.
        if world.next_deadline().is_some_and(|t| t <= clock.now()) {
            let due = {
                let mut ctx = WorldCtx::detached(
                    &mut clock,
                    &mut cursor,
                    backend,
                    sink,
                    &mut next_event,
                    &mut recorded,
                );
                world.on_deadline(&mut ctx)?
            };
            apply_due(due, &mut tasks, backend, policy, specs)?;
            // The contract is "fire everything due"; a due entry surviving the call would spin
            // this loop forever, so it is an invariant rather than a retry.
            if world.next_deadline().is_some_and(|t| t <= clock.now()) {
                return Err(KernelError::Invariant("a due deadline survived on_deadline"));
            }
        }

        // Before the horizon for the reason the horizon is before the budget: a scenario that named
        // the ending it wanted should be answered with that ending rather than with whichever
        // ceiling happened to arrive. A node with no tasks left is done whether it ended or was
        // ended — there is nothing there to drive anything, and waiting out the horizon around it
        // would measure a server talking to nobody.
        if config
            .ends_with
            .is_some_and(|node| matches!(tasks.node_state.get(&node), Some(NodeLifecycle::Dead)))
        {
            break StopReason::NodeFinished;
        }
        // Before the budget, so a horizon can never be shadowed by a bound it sits under. A run
        // asked to reach a moment and stopped one tick short of it by an unrelated ceiling would
        // be judged on an execution nobody asked for.
        if config.horizon.is_some_and(|at| clock.now() >= at) {
            break StopReason::HorizonReached;
        }
        if clock.now() >= config.max_ticks {
            break StopReason::TickBudget;
        }
        if next_event.get() >= config.max_events {
            break StopReason::EventBudget;
        }
        // After the deadlines and before the scheduler picks, which is the one place in the loop
        // where every task is stopped and none is part-way through being answered.
        if config.break_at.is_some_and(|at| next_event >= at) {
            break StopReason::BreakRequested;
        }

        runnable.clear();
        runnable.extend(tasks.pending.keys().copied().filter(|t| tasks.schedulable(*t)));
        #[cfg(feature = "chaos-hash-iteration")]
        let runnable = crate::chaos::in_hash_order(&runnable);

        if runnable.is_empty() {
            // An armed deadline outranks both endings: blocked and frozen tasks are waiting for
            // it, and even an empty cluster may owe a restart to a timer. Time jumps there and
            // the loop top fires it. A deadline at or past the tick budget is a deadline the run
            // will never live to see, and chasing it would turn every far-future scenario timer
            // into a hang.
            let ceiling = config.horizon.unwrap_or(config.max_ticks).min(config.max_ticks);
            match world.next_deadline() {
                Some(due) if due < ceiling => {
                    clock.advance(Tick(due.get().saturating_sub(clock.now().get())));
                    continue;
                }
                // A cluster whose next timer is past the horizon has reached the horizon: time is
                // the thing it was waiting for, and it would have gone on waiting. Advancing to
                // the horizon before ending is what makes the recorded tick count the span the
                // scenario asked for rather than the moment everything went quiet — which is what
                // an oracle reasoning about a span of silence has to measure against.
                //
                // With **nothing** armed, no horizon saves it: nothing can ever happen again, and
                // a horizon that turned that into a clean ending would hide exactly the defect
                // the deadlock check exists to find.
                Some(_) if config.horizon.is_some_and(|at| at <= config.max_ticks) => {
                    let at = config.horizon.unwrap_or(clock.now());
                    clock.advance(Tick(at.get().saturating_sub(clock.now().get())));
                    break StopReason::HorizonReached;
                }
                _ => {
                    break if tasks.pending.is_empty() && tasks.blocked.is_empty() {
                        StopReason::GuestFinished
                    } else {
                        StopReason::Deadlock
                    };
                }
            }
        }

        // Draws made while choosing belong to the event that switch produces.
        cursor.set_event(next_event);

        #[cfg(feature = "chaos-delivery-tie")]
        let chosen = crate::chaos::first_to_arrive(&runnable);
        #[cfg(not(feature = "chaos-delivery-tie"))]
        let chosen = policy.pick(&runnable, &mut cursor)?;
        let action = tasks
            .pending
            .remove(&chosen)
            .ok_or(KernelError::Invariant("scheduler chose a task that is not stopped"))?;

        if tasks.last_run != Some(chosen) {
            // The first dispatch of a run moves execution from nowhere and is not a switch; every
            // later change of the running task is one, including the change after an exit.
            if let Some(from) = tasks.last_run {
                let node = tasks
                    .node_for(chosen)
                    .ok_or(KernelError::Invariant("scheduled a task with no node"))?;
                emit(
                    sink,
                    &mut next_event,
                    &clock,
                    node,
                    chosen,
                    EventKind::Schedule { from, to: chosen, reason: last_reason },
                )?;
            }
            tasks.last_run = Some(chosen);
        }
        last_reason = SwitchReason::SyscallBoundary;

        backend.resume(chosen, action)?;
        let Some(stop) = backend.next_stop()? else {
            // Checked here as at the other place a run can end this way. A backend answering
            // `None` with work still outstanding turns a half-finished run into a clean verdict,
            // and the oracle then judges a prefix as though it were the whole execution.
            if !tasks.pending.is_empty() || !tasks.blocked.is_empty() {
                return Err(KernelError::Invariant(
                    "the backend reported every task finished while tasks were still to run",
                ));
            }
            break StopReason::GuestFinished;
        };

        // Structural bookkeeping belongs to the kernel; interpretation belongs to the world.
        match &stop.kind {
            StopKind::TaskCreated { parent, .. } => {
                // A new task belongs to the node that created it, and so does the event announcing
                // it: reading the node from the task that does not exist yet would file every
                // spawn under node zero.
                let node = tasks.node_for(*parent).ok_or(KernelError::Invariant(
                    "a task was created by one this run has no node for",
                ))?;
                tasks.node_of.insert(stop.task, node);
                world.task_created(stop.task, *parent);
                tasks.pending.insert(stop.task, Resume::Continue);
                emit(
                    sink,
                    &mut next_event,
                    &clock,
                    node,
                    stop.task,
                    EventKind::TaskSpawned { parent: *parent },
                )?;
                tasks.pending.insert(*parent, Resume::Continue);
                continue;
            }
            StopKind::TaskExited { status } => {
                tasks.blocked.remove(&stop.task);
                for released in world.task_exited(stop.task) {
                    // Guarded, as at the three other sites that apply a wake. Without it a wake
                    // naming a task that was *pending* rather than blocked overwrote the answer
                    // the world had already decided for its syscall, and one naming a task
                    // already reaped inserted a ghost the scheduler would go on to pick.
                    if tasks.blocked.remove(&released.task) {
                        tasks.pending.insert(released.task, Resume::Emulate { ret: released.ret });
                    }
                }
                let node = tasks.node_for(stop.task);
                // A task retired with its node has been accounted for already, and deliberately
                // without an exit event: the node was declared gone by the task that ended it and
                // the rest were taken off the books there. Its own exit still arrives from the
                // backend afterwards, and filing it would report a task the run has finished with
                // twice over.
                if node.is_none() && tasks.retired.remove(&stop.task) {
                    tasks.note_task_gone(stop.task);
                    policy.forget(stop.task);
                    continue;
                }
                let converted = match status {
                    GuestExit::Exited(c) => {
                        // A guest that failed is a finding whoever exits after it. A cluster's
                        // caller routinely outlives the node it was driving, so keeping the last
                        // status would let a node that died report the caller's success as the
                        // run's — and a campaign would count its worst outcome as its best.
                        if *c != 0 || last_exit.is_none() {
                            last_exit = Some(*c);
                        }
                        ExitStatus::Exited(*c)
                    }
                    GuestExit::Signalled(s) => {
                        let signal = tasks.faulted_on.remove(&stop.task).unwrap_or(*s);
                        if !tasks.killed_by_us.contains(&stop.task) {
                            killed_by = Some(signal);
                        }
                        ExitStatus::Signalled(signal)
                    }
                };
                let node =
                    node.ok_or(KernelError::Invariant("a task exited with no node to file it on"))?;
                emit(
                    sink,
                    &mut next_event,
                    &clock,
                    node,
                    stop.task,
                    EventKind::TaskExited { status: converted },
                )?;
                if tasks.last_run == Some(stop.task) {
                    last_reason = SwitchReason::TaskExit;
                }
                tasks.note_task_gone(stop.task);
                policy.forget(stop.task);
                continue;
            }
            _ => {}
        }

        if let StopKind::Signal { signal } = stop.kind {
            tasks.faulted_on.insert(stop.task, signal);
        }

        let node = tasks
            .node_for(stop.task)
            .ok_or(KernelError::Invariant("a stop arrived for a task with no node"))?;
        let effects = {
            let mut ctx = WorldCtx::new(
                &mut clock,
                &mut cursor,
                backend,
                sink,
                &mut next_event,
                node,
                stop.task,
                &mut recorded,
            );
            world.handle(&stop, &mut ctx)?
        };

        clock.advance(effects.advance);
        if effects.yielded {
            policy.yielded(stop.task);
            if tasks.last_run == Some(stop.task) {
                last_reason = SwitchReason::Yield;
            }
        }
        for released in effects.wake {
            // Only a task that is actually waiting can be released. A model that named one which
            // has since ended would hand the backend a task it no longer has.
            if tasks.blocked.remove(&released.task) {
                tasks.pending.insert(released.task, Resume::Emulate { ret: released.ret });
            }
        }
        match effects.state {
            // Ending a task is carried out here rather than left for the scheduler to pick up: the
            // task is gone once the backend has been told, and the record of how it ended has to be
            // written while what ended it is still known.
            TaskState::Runnable(Resume::Terminate { signal }) => {
                backend.resume(stop.task, Resume::Terminate { signal })?;
                // What the world named, and what the kernel saw where it named nothing. A task
                // that asked to die stops on no signal, so `faulted_on` is empty for it and only
                // the world can say what took it.
                let died_of = signal.or_else(|| tasks.faulted_on.remove(&stop.task));
                let status = match died_of {
                    Some(signal) => {
                        killed_by = Some(signal);
                        ExitStatus::Signalled(signal)
                    }
                    None => ExitStatus::Exited(0),
                };
                emit(
                    sink,
                    &mut next_event,
                    &clock,
                    node,
                    stop.task,
                    EventKind::TaskExited { status },
                )?;
                if tasks.last_run == Some(stop.task) {
                    last_reason = SwitchReason::TaskExit;
                }
                tasks.note_task_gone(stop.task);
                policy.forget(stop.task);
                for released in world.task_exited(stop.task) {
                    if tasks.blocked.remove(&released.task) {
                        tasks.pending.insert(released.task, Resume::Emulate { ret: released.ret });
                    }
                }
            }
            TaskState::Runnable(resume) => {
                if matches!(stop.kind, StopKind::BudgetExhausted { .. })
                    && tasks.last_run == Some(stop.task)
                {
                    last_reason = SwitchReason::BudgetExhausted;
                }
                tasks.pending.insert(stop.task, resume);
            }
            TaskState::Blocked => {
                if tasks.last_run == Some(stop.task) {
                    last_reason = SwitchReason::Blocked;
                }
                tasks.blocked.insert(stop.task);
            }
        }
        apply_node_ops(&effects.node_ops, &mut tasks, backend, policy, specs)?;
    };

    let digest = sink.digest();
    let outcome = judge(
        oracle,
        &clock,
        next_event,
        digest,
        last_exit,
        killed_by,
        &recorded,
        stop_reason,
        config.horizon,
    );

    Ok(RunReport {
        digest,
        events: next_event.get(),
        ticks: clock.now(),
        stop_reason,
        outcome,
        violation_event: recorded.violation_event(),
        adversity: recorded.adversity(),
        journal: cursor.into_journal(),
    })
}

/// Applies what firing deadlines produced.
fn apply_due(
    due: DueEffects,
    tasks: &mut Tasks,
    backend: &mut dyn DeterminismBackend,
    policy: &mut dyn SchedulePolicy,
    specs: &[GuestSpec],
) -> Result<(), KernelError> {
    for released in due.wake {
        if tasks.blocked.remove(&released.task) {
            tasks.pending.insert(released.task, Resume::Emulate { ret: released.ret });
        }
    }
    apply_node_ops(&due.node_ops, tasks, backend, policy, specs)
}

/// Executes the world's node decisions: the task bookkeeping and backend calls only the kernel
/// can make. The narrating events were already emitted by the world.
fn apply_node_ops(
    ops: &[NodeOp],
    tasks: &mut Tasks,
    backend: &mut dyn DeterminismBackend,
    policy: &mut dyn SchedulePolicy,
    specs: &[GuestSpec],
) -> Result<(), KernelError> {
    for op in ops {
        match *op {
            // The host has already ended them; what is left is to stop expecting them. Nothing is
            // killed and nothing is narrated: the program said it was done, and a task still
            // parked in a call it will never return from is not a task that is waiting.
            NodeOp::Ended { node, by } => {
                let gone: Vec<TaskId> = tasks
                    .node_of
                    .iter()
                    .filter(|(task, n)| **n == node && **task != by)
                    .map(|(t, _)| *t)
                    .collect();
                for task in gone {
                    tasks.pending.remove(&task);
                    tasks.blocked.remove(&task);
                    tasks.faulted_on.remove(&task);
                    // Also the node it belonged to. Left behind, these entries answered "does this
                    // node still have tasks" with yes for ever: the node never reached `Dead`, so
                    // `ends_with` never fired for a program that exited with more than one thread
                    // -- which is the Go case this variant exists for -- and a later `Restart`
                    // failed the invariant that a node being restarted is dead.
                    tasks.node_of.remove(&task);
                    tasks.retired.insert(task);
                    policy.forget(task);
                }
                // The spared task carries on and will be noticed when it goes, so the node is
                // marked from here only if nothing of it is left already.
                if !tasks.node_of.values().any(|n| *n == node) {
                    tasks.node_state.insert(node, NodeLifecycle::Dead);
                }
            }
            NodeOp::Kill(node) => {
                if tasks.node_state.get(&node) != Some(&NodeLifecycle::Running)
                    && tasks.node_state.get(&node) != Some(&NodeLifecycle::Frozen)
                {
                    return Err(KernelError::Invariant("killing a node that is not alive"));
                }
                // Tasks already killed are left alone: their entries are kept so their exits can
                // still be filed on the node they belonged to, and asking the backend to kill one
                // twice names a task it has already let go.
                let doomed: Vec<TaskId> = tasks
                    .node_of
                    .iter()
                    .filter(|(task, n)| **n == node && !tasks.killed_by_us.contains(task))
                    .map(|(t, _)| *t)
                    .collect();
                for task in doomed {
                    backend.kill(task)?;
                    tasks.killed_by_us.insert(task);
                    tasks.pending.remove(&task);
                    tasks.blocked.remove(&task);
                    // `node_of` is deliberately kept. A backend that ends a task by signalling it
                    // still reports the exit afterwards, and a task whose node had already been
                    // forgotten had that exit filed under the first node of the cluster. The
                    // restart path inserts its new tasks before any stale exit could re-mark the
                    // node, so holding the entry until `note_task_gone` is safe.
                    tasks.faulted_on.remove(&task);
                    policy.forget(task);
                    // No switch event follows a kill: the world's NodeState narration already
                    // explains why execution moved, and the killed task is not a `from`.
                    if tasks.last_run == Some(task) {
                        tasks.last_run = None;
                    }
                }
                // Deliberately not recorded as the guest being killed. The signal came from the
                // fault injector, and a run reported as a crash because we pulled the plug would
                // make every fault-injected run a finding — which is a search that reports its own
                // faults back to itself. What the node was killed *by* is on the timeline as
                // `NodeState::Dead`, and what it does about it is the verdict.
                tasks.node_state.insert(node, NodeLifecycle::Dead);
            }
            NodeOp::Freeze(node) => {
                if tasks.node_state.get(&node) != Some(&NodeLifecycle::Running) {
                    return Err(KernelError::Invariant("freezing a node that is not running"));
                }
                tasks.node_state.insert(node, NodeLifecycle::Frozen);
            }
            NodeOp::Thaw(node) => {
                if tasks.node_state.get(&node) != Some(&NodeLifecycle::Frozen) {
                    return Err(KernelError::Invariant("thawing a node that is not frozen"));
                }
                tasks.node_state.insert(node, NodeLifecycle::Running);
            }
            NodeOp::Restart(node) => {
                if tasks.node_state.get(&node) != Some(&NodeLifecycle::Dead) {
                    return Err(KernelError::Invariant("restarting a node that is not dead"));
                }
                let spec = specs
                    .get(node.get() as usize)
                    .ok_or(KernelError::Invariant("restarting a node that was never started"))?;
                let first = backend.spawn(node, spec)?;
                tasks.pending.insert(first, Resume::Continue);
                tasks.node_of.insert(first, node);
                tasks.node_state.insert(node, NodeLifecycle::Running);
            }
        }
    }
    Ok(())
}

/// Turns the oracle's verdict into the outcome recorded in a bundle.
///
/// A run cut short by a budget is a hang whatever the oracle thought, because the oracle only ever
/// saw a prefix of the execution. A run that reached its horizon is the opposite case and the
/// reason the horizon exists: the execution the scenario asked for happened in full, and refusing
/// to judge it would mean a guest that never exits can never be judged at all — which is every
/// guest this engine now exists to be pointed at.
#[allow(clippy::too_many_arguments)]
fn judge(
    oracle: &mut dyn Oracle,
    clock: &Clock,
    next_event: EventId,
    digest: Digest,
    exit: Option<i32>,
    killed_by: Option<u32>,
    recorded: &Recorded,
    stop_reason: StopReason,
    horizon: Option<Tick>,
) -> Outcome {
    let outcome = RunOutcome {
        digest,
        events: next_event.get(),
        ticks: clock.now(),
        exit,
        killed_by,
        diagnostics: recorded.diagnostics().to_vec(),
        // Read after the run, by whoever holds the world; the kernel never sees one.
        projections: Vec::new(),
    };
    match stop_reason {
        StopReason::TickBudget => {
            return Outcome::GuestHung(format!("no progress by tick {}", clock.now().get()))
        }
        // The budget names the horizon when there was one, because that is the first wall a
        // long-running guest hits: ten million events go by long before twenty-five virtual
        // hours, and without this the message sends a reader to doubt the oracle instead of
        // raising the budget.
        StopReason::EventBudget => {
            return Outcome::GuestHung(match horizon {
                Some(at) => format!(
                    "event budget reached at {}, short of the horizon at tick {}",
                    next_event.get(),
                    at.get()
                ),
                None => format!("event budget reached at {}", next_event.get()),
            })
        }
        // Every task waiting with nothing left to release any of them is the guest's own defect,
        // and one the oracle never gets to see: there is no further execution to judge.
        StopReason::Deadlock => {
            return Outcome::GuestHung(format!(
                "every task is waiting at tick {}",
                clock.now().get()
            ))
        }
        // Asked to stop is neither a hang nor a clean finish. The oracle saw a prefix, exactly as
        // with a budget — but nothing about the guest went wrong, and reporting it as a hang would
        // put a finding in a log for a command somebody ran to look at something.
        StopReason::BreakRequested => {
            return Outcome::StoppedOnRequest(format!("stopped at event {}", next_event.get()))
        }
        // A node the scenario named as the run's ending has ended, which is the ending it asked
        // for. Judged like a horizon and for the same reason: the execution that was wanted is the
        // one that happened, so the oracle is asked about it rather than told it was abandoned.
        StopReason::GuestFinished | StopReason::HorizonReached | StopReason::NodeFinished => {}
    }
    match oracle.judge(&outcome) {
        Verdict::Finding(o) => o,
        Verdict::Clean | Verdict::Agree | Verdict::Skipped => Outcome::Clean,
    }
}

/// Draws the offset and drift one node's clock runs at.
///
/// Both come from the tape, so a seed selects how far apart the machines in a run are, and a
/// mutation of those bytes moves them without disturbing any other decision.
fn draw_node_clock(
    cursor: &mut bohr_tape::TapeCursor<'_>,
    max_skew: Tick,
) -> Result<NodeClock, KernelError> {
    let offset = cursor.draw_range(max_skew.get().saturating_add(1), DrawSite::ClockSkew)?;

    let drift_span = u64::from(MAX_DRIFT_PPM) * 2 + 1;
    let drift = cursor.draw_range(drift_span, DrawSite::ClockSkew)? as i32 - MAX_DRIFT_PPM as i32;

    Ok(NodeClock::new(offset, drift))
}

fn emit(
    sink: &mut dyn EventSink,
    next_event: &mut EventId,
    clock: &Clock,
    node: NodeId,
    task: TaskId,
    kind: EventKind,
) -> Result<(), KernelError> {
    let event = bohr_trace::Event { id: *next_event, tick: clock.now(), node, task, kind };
    sink.emit(&event)?;
    *next_event = EventId(next_event.get() + 1);
    Ok(())
}
