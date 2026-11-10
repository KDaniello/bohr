//! The ports the run loop drives: the world, the oracle and the event sink.

use bohr_backend::{DeterminismBackend, Regs, Resume, Stop};
use bohr_tape::{DrawSite, TapeCursor};
use bohr_trace::{Event, EventKind, FaultKind, Outcome, PacketFate};
use bohr_types::{Digest, EventId, GuestAddr, NodeId, OracleId, TaskId, Tick, WorldModelId};

use crate::{
    KernelError,
    clock::{Clock, NodeClock},
};

/// The environment a guest believes it is running in.
///
/// The kernel supplies stops, the clock and the tape; the model decides what the world does about
/// them. Implementations are domain-specific by design.
pub trait WorldModel {
    /// Stable identity, recorded in the bundle and checked before a replay.
    fn identity(&self) -> WorldModelId;

    /// The only place a domain interprets guest behaviour.
    fn handle(&mut self, stop: &Stop, ctx: &mut WorldCtx<'_, '_>) -> Result<Effects, KernelError>;

    /// A task was created, and by which task.
    ///
    /// Ties the creating call, already seen, to the task it produced.
    fn task_created(&mut self, _task: TaskId, _parent: TaskId) {}

    /// A task ended, and what its ending releases.
    ///
    /// A thread ending wakes its joiner, who waits on a word the kernel clears. The task is gone,
    /// so the model answers from what it recorded at creation.
    fn task_exited(&mut self, _task: TaskId) -> Vec<Wake> {
        Vec::new()
    }

    /// Faults this model can inject.
    fn fault_catalog(&self) -> &FaultCatalog;

    /// An independent copy in the same state, for a run that branches from a checkpoint.
    ///
    /// `None` where the model cannot be copied; a run then cannot branch while it is in use.
    fn fork(&self) -> Option<Box<dyn WorldModel>> {
        None
    }

    /// Earliest armed timer, if any.
    fn next_deadline(&self) -> Option<Tick> {
        None
    }

    /// Fires everything armed at or before the world clock's current reading.
    ///
    /// Called while [`Self::next_deadline`] is due, and after the clock jumps to it. The context
    /// is detached, so events go through [`WorldCtx::emit_for`] and memory through `_for`
    /// accessors.
    fn on_deadline(&mut self, _ctx: &mut WorldCtx<'_, '_>) -> Result<DueEffects, KernelError> {
        Ok(DueEffects::default())
    }
}

/// What the world decided about a stop.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Effects {
    /// What the task that stopped may do next.
    pub state: TaskState,
    /// How much virtual time this stop consumed.
    pub advance: Tick,
    /// Tasks the world released, each with what its blocking call returns.
    ///
    /// A wake is an effect of this stop; the woken task runs when the scheduler picks it.
    pub wake: Vec<Wake>,
    /// Whether the task asked to give up the processor.
    ///
    /// A policy that runs long stretches switches here, so a spin on another task's flag ends.
    pub yielded: bool,
    /// Operations on whole nodes the world decided on, carried out by the kernel.
    pub node_ops: Vec<NodeOp>,
}

impl Effects {
    /// The task carries on.
    #[must_use]
    pub const fn go(resume: Resume, advance: Tick) -> Self {
        Self {
            state: TaskState::Runnable(resume),
            advance,
            wake: Vec::new(),
            yielded: false,
            node_ops: Vec::new(),
        }
    }

    /// The task waits until something releases it.
    #[must_use]
    pub const fn wait(advance: Tick) -> Self {
        Self {
            state: TaskState::Blocked,
            advance,
            wake: Vec::new(),
            yielded: false,
            node_ops: Vec::new(),
        }
    }

    /// The same effects, with the task having asked to give up the processor.
    #[must_use]
    pub const fn yielding(mut self) -> Self {
        self.yielded = true;
        self
    }

    /// The same effects, releasing these tasks as well.
    #[must_use]
    pub fn releasing(mut self, wake: Vec<Wake>) -> Self {
        self.wake = wake;
        self
    }

    /// The same effects, with these node operations to carry out as well.
    #[must_use]
    pub fn commanding(mut self, node_ops: Vec<NodeOp>) -> Self {
        self.node_ops = node_ops;
        self
    }
}

/// What a stop leaves the task able to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TaskState {
    /// Ready to run again whenever the scheduler picks it.
    Runnable(Resume),
    /// Waiting for the world to release it. All tasks blocked with nothing to release them is a
    /// deadlock, which is a finding.
    Blocked,
}

/// A task the world released, and what its blocking call returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wake {
    /// The task that was waiting.
    pub task: TaskId,
    /// What the call it blocked in returns.
    pub ret: i64,
}

/// An operation on a whole node, decided by the world and carried out by the kernel.
///
/// The world has already applied its consequences and emitted its events; the kernel does the
/// task bookkeeping and backend calls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeOp {
    /// End every task of the node, now.
    Kill(NodeId),
    /// Keep the node's tasks out of scheduling until thawed. Wakes still land, so a freeze is a
    /// pause.
    Freeze(NodeId),
    /// Let a frozen node run again.
    Thaw(NodeId),
    /// Re-spawn a dead node's initial task from the spec it was started with. The node keeps the
    /// clock it drew at spawn, so a restart cannot shift the tape position of later draws.
    Restart(NodeId),
    /// The node's process called `exit_group`, ending every task.
    ///
    /// A normal exit. Workers parked on a futex, as in the Go runtime, end with it.
    Ended {
        /// The node whose process is ending.
        node: NodeId,
        /// The calling task, spared so it can deliver the process's exit status.
        by: TaskId,
    },
}

/// What firing due deadlines produced.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DueEffects {
    /// Tasks released by expired timers, each with what its blocking call returns.
    pub wake: Vec<Wake>,
    /// Node operations to carry out.
    pub node_ops: Vec<NodeOp>,
}

/// Faults a world model is able to inject.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct FaultCatalog {
    kinds: Vec<FaultKind>,
}

impl FaultCatalog {
    /// A catalogue offering the given faults.
    #[must_use]
    pub fn new(kinds: Vec<FaultKind>) -> Self {
        Self { kinds }
    }

    /// The faults on offer.
    #[must_use]
    pub fn kinds(&self) -> &[FaultKind] {
        &self.kinds
    }
}

/// Decides whether a run, or a set of runs, counts as a finding.
pub trait Oracle {
    /// Stable identity, recorded in the bundle and checked before a replay.
    fn identity(&self) -> OracleId;

    /// Judges one run.
    fn judge(&mut self, run: &RunOutcome) -> Verdict;

    /// Judges a set of runs against each other.
    ///
    /// Implementations of one specification must agree; a disagreement is a bug in one of them.
    fn judge_group(&mut self, _runs: &[RunOutcome]) -> Verdict {
        Verdict::Skipped
    }
}

/// Something a world model noticed and wrote down, as an oracle reads it.
///
/// The message says which promise broke; the code alone names only the class.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    /// The code that names the class, stable forever (`docs/API.md` §8).
    pub code: String,
    /// What was noticed, in the words the model used.
    pub message: String,
}

/// What a run accumulated for whoever judges it.
///
/// One value, so new facts do not add arguments.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Recorded {
    diagnostics: Vec<Diagnostic>,
    violation_event: Option<EventId>,
    adversity: u32,
}

impl Recorded {
    /// Nothing noticed yet.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// What was noticed, in the order it was.
    pub(crate) fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// How much adversity actually landed on the guest.
    ///
    /// Faults are drawn, and a run can end before they arm, so only this shows whether any
    /// landed.
    pub(crate) const fn adversity(&self) -> u32 {
        self.adversity
    }

    /// The event where a promise the guest made about itself first did not hold.
    ///
    /// The first violation, since what follows it is the guest's reaction.
    pub(crate) const fn violation_event(&self) -> Option<EventId> {
        self.violation_event
    }
}

/// What a run produced, as an oracle sees it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RunOutcome {
    /// Digest of the canonical event stream.
    pub digest: Digest,
    /// How many events were recorded.
    pub events: u64,
    /// Virtual time the run reached.
    pub ticks: Tick,
    /// Exit status of the last task to end, where the run got that far.
    pub exit: Option<i32>,
    /// The signal that ended a task, where one did.
    ///
    /// A task killed by a fault has no exit status.
    pub killed_by: Option<u32>,
    /// What the run noticed and wrote down, in the order it did.
    pub diagnostics: Vec<Diagnostic>,
    /// What this run is compared by, where anything asked it to be.
    ///
    /// Empty for a single implementation. Filled after the run by the crate that executes it,
    /// since a projection is read from the finished world.
    pub projections: Vec<(String, Digest)>,
}

/// An oracle's answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Nothing to report.
    Clean,
    /// Something worth keeping.
    Finding(Outcome),
    /// The runs in a group matched.
    Agree,
    /// This oracle has no opinion on the question asked.
    Skipped,
}

/// Watches a run as it goes, for whoever shows progress. Never part of what the run is.
///
/// Called between stops with a copy of the run's position; nothing it does is read back.
pub trait Observer {
    /// The run so far: every [`crate::PROGRESS_EVERY`] events, and once more when it ends.
    fn progress(&mut self, progress: &Progress);
}

/// Where a run stands, as an [`Observer`] is shown it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Progress {
    /// Events recorded so far.
    pub events: u64,
    /// The world clock.
    pub ticks: Tick,
    /// Scheduling decisions made so far.
    pub decisions: u64,
    /// Whether this is the report the run ends with.
    pub finished: bool,
}

/// Where recorded events go.
pub trait EventSink {
    /// Appends one event.
    fn emit(&mut self, event: &Event) -> Result<(), KernelError>;

    /// Digest of everything emitted so far.
    fn digest(&self) -> Digest;
}

impl<W: std::io::Write> EventSink for bohr_trace::TraceWriter<W> {
    fn emit(&mut self, event: &Event) -> Result<(), KernelError> {
        self.record(event)?;
        Ok(())
    }

    fn digest(&self) -> Digest {
        bohr_trace::TraceWriter::digest(self)
    }
}

/// What a world model is given while handling a stop or firing deadlines.
///
/// Methods expose the clock, tape and memory, so every draw is journalled.
///
/// While handling a stop, the short accessors ([`Self::task`], [`Self::now`], [`Self::emit`], plain
/// memory) refer to the stopped task. In [`WorldModel::on_deadline`] the context is detached: those
/// panic, and the model uses the `_for` methods.
pub struct WorldCtx<'a, 't> {
    clock: &'a mut Clock,
    cursor: &'a mut TapeCursor<'t>,
    backend: &'a mut dyn DeterminismBackend,
    sink: &'a mut dyn EventSink,
    next_event: &'a mut EventId,
    origin: Option<(NodeId, TaskId)>,
    recorded: &'a mut Recorded,
}

impl<'a, 't> WorldCtx<'a, 't> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        clock: &'a mut Clock,
        cursor: &'a mut TapeCursor<'t>,
        backend: &'a mut dyn DeterminismBackend,
        sink: &'a mut dyn EventSink,
        next_event: &'a mut EventId,
        node: NodeId,
        task: TaskId,
        recorded: &'a mut Recorded,
    ) -> Self {
        Self { clock, cursor, backend, sink, next_event, origin: Some((node, task)), recorded }
    }

    /// A context with no current task, for firing deadlines.
    pub(crate) fn detached(
        clock: &'a mut Clock,
        cursor: &'a mut TapeCursor<'t>,
        backend: &'a mut dyn DeterminismBackend,
        sink: &'a mut dyn EventSink,
        next_event: &'a mut EventId,
        recorded: &'a mut Recorded,
    ) -> Self {
        Self { clock, cursor, backend, sink, next_event, origin: None, recorded }
    }

    /// The stop's origin, which a detached context does not have.
    ///
    /// Panics, since asking while firing deadlines is a bug in the model.
    #[track_caller]
    fn origin(&self) -> (NodeId, TaskId) {
        self.origin.expect("no current task while firing deadlines; attribute explicitly")
    }

    /// What the node this stop happened on reads for the time.
    ///
    /// Includes the node's own offset and drift.
    ///
    /// # Panics
    /// On a detached context; use [`Self::now_on`] there.
    #[must_use]
    pub fn now(&self) -> Tick {
        self.clock.now_on(self.origin().0)
    }

    /// What a named node reads for the time.
    #[must_use]
    pub fn now_on(&self, node: NodeId) -> Tick {
        self.clock.now_on(node)
    }

    /// What the node this stop happened on reads for wall time (`CLOCK_REALTIME`).
    ///
    /// [`Self::now`] on the wall epoch plus any step; a step back makes it read earlier.
    ///
    /// # Panics
    /// On a detached context; use [`Self::wall_on`] there.
    #[must_use]
    pub fn wall_now(&self) -> Tick {
        self.clock.wall_on(self.origin().0)
    }

    /// What a named node reads for wall time.
    #[must_use]
    pub fn wall_on(&self, node: NodeId) -> Tick {
        self.clock.wall_on(node)
    }

    /// The clock a named node reads by, so a fault can be built from it.
    #[must_use]
    pub fn node_clock(&self, node: NodeId) -> Option<NodeClock> {
        self.clock.node_clock(node)
    }

    /// The world clock, which the run is ordered by.
    #[must_use]
    pub fn world_now(&self) -> Tick {
        self.clock.now()
    }

    /// The node the stop happened on.
    ///
    /// # Panics
    /// On a detached context.
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.origin().0
    }

    /// The task the stop happened on.
    ///
    /// # Panics
    /// On a detached context.
    #[must_use]
    pub fn task(&self) -> TaskId {
        self.origin().1
    }

    /// Draws a value in `0..n`.
    pub fn draw_range(&mut self, n: u64, site: DrawSite) -> Result<u64, KernelError> {
        Ok(self.cursor.draw_range(n, site)?)
    }

    /// Draws a position in a slice of `len` elements.
    pub fn draw_index(&mut self, len: usize, site: DrawSite) -> Result<usize, KernelError> {
        Ok(self.cursor.draw_index(len, site)?)
    }

    /// Draws a boolean with probability `num/den`.
    pub fn draw_bool(&mut self, num: u32, den: u32, site: DrawSite) -> Result<bool, KernelError> {
        Ok(self.cursor.draw_bool(num, den, site)?)
    }

    /// Draws a boolean with probability `num/den` at a fixed two bytes; see
    /// [`bohr_tape::TapeCursor::draw_odds`].
    pub fn draw_odds(&mut self, num: u32, den: u32, site: DrawSite) -> bool {
        self.cursor.draw_odds(num, den, site)
    }

    /// Draws raw bytes.
    pub fn draw_into(&mut self, out: &mut [u8], site: DrawSite) {
        self.cursor.draw_into(out, site);
    }

    /// Draws an exponentially distributed value with the given mean.
    pub fn draw_exponential(&mut self, mean: u64, site: DrawSite) -> Result<u64, KernelError> {
        Ok(self.cursor.draw_exponential(mean, site)?)
    }

    /// Writes into guest memory.
    ///
    /// # Panics
    /// On a detached context; use [`Self::write_mem_for`] there.
    pub fn write_mem(&mut self, addr: GuestAddr, data: &[u8]) -> Result<(), KernelError> {
        let (_, task) = self.origin();
        self.backend.write_mem(task, addr, data)?;
        Ok(())
    }

    /// Reads guest memory.
    ///
    /// # Panics
    /// On a detached context; use [`Self::read_mem_for`] there.
    pub fn read_mem(&mut self, addr: GuestAddr, out: &mut [u8]) -> Result<(), KernelError> {
        let (_, task) = self.origin();
        self.backend.read_mem(task, addr, out)?;
        Ok(())
    }

    /// Writes into a named task's memory, such as a receiver's buffer on delivery.
    pub fn write_mem_for(
        &mut self,
        task: TaskId,
        addr: GuestAddr,
        data: &[u8],
    ) -> Result<(), KernelError> {
        self.backend.write_mem(task, addr, data)?;
        Ok(())
    }

    /// Reads the memory of a named task.
    pub fn read_mem_for(
        &mut self,
        task: TaskId,
        addr: GuestAddr,
        out: &mut [u8],
    ) -> Result<(), KernelError> {
        self.backend.read_mem(task, addr, out)?;
        Ok(())
    }

    /// Gives a node a clock of its own.
    ///
    /// Clocks are skewed once, before the first task runs (`docs/ARCHITECTURE.md` §4.5).
    pub fn set_node_clock(&mut self, node: NodeId, clock: NodeClock) {
        self.clock.set_node_clock(node, clock);
    }

    /// Reads the stopped task's registers.
    ///
    /// # Panics
    /// On a detached context.
    pub fn regs(&mut self) -> Result<Regs, KernelError> {
        let (_, task) = self.origin();
        Ok(self.backend.regs(task)?)
    }

    /// Replaces the stopped task's registers.
    ///
    /// For emulating a trapped instruction: write its results and move past it.
    ///
    /// # Panics
    /// On a detached context.
    pub fn set_regs(&mut self, regs: &Regs) -> Result<(), KernelError> {
        let (_, task) = self.origin();
        self.backend.set_regs(task, regs)?;
        Ok(())
    }

    /// Records an event attributed to the stop being handled.
    ///
    /// # Panics
    /// On a detached context; use [`Self::emit_for`] there.
    pub fn emit(&mut self, kind: EventKind) -> Result<(), KernelError> {
        let (node, task) = self.origin();
        self.emit_for(node, task, kind)
    }

    /// Records an event for a named node and task, such as a delivery or a deadline's event.
    pub fn emit_for(
        &mut self,
        node: NodeId,
        task: TaskId,
        kind: EventKind,
    ) -> Result<(), KernelError> {
        match &kind {
            EventKind::Diagnostic { code, message } => {
                self.recorded
                    .diagnostics
                    .push(Diagnostic { code: code.clone(), message: message.clone() });
            }
            // Taken from the event, so no model reports the violation point separately.
            EventKind::Assertion { condition: false, .. } => {
                self.recorded.violation_event.get_or_insert(*self.next_event);
            }
            // Faults and lossy-link drops both count, since the guest cannot tell them apart.
            // Duplicated, reordered or altered packets fire their own fault and count there.
            EventKind::FaultFired { .. }
            | EventKind::PacketFate { fate: PacketFate::Dropped, .. } => {
                self.recorded.adversity = self.recorded.adversity.saturating_add(1);
            }
            _ => {}
        }
        let event = Event { id: *self.next_event, tick: self.clock.now(), node, task, kind };
        self.sink.emit(&event)?;
        *self.next_event = EventId(self.next_event.get() + 1);
        // Later draws belong to the next event, aligning the journal with the trace.
        self.cursor.set_event(*self.next_event);
        Ok(())
    }

    /// Refuses a stop this model has no answer for, under the code that names the class.
    ///
    /// # Panics
    /// On a detached context: refusals are stop-scoped by definition.
    pub fn refuse(&self, code: &'static str, what: impl std::fmt::Display) -> KernelError {
        KernelError::World { code, message: format!("{what} on {}", self.origin().1) }
    }
}

impl std::fmt::Debug for WorldCtx<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorldCtx")
            .field("origin", &self.origin)
            .field("now", &self.clock.now())
            .finish_non_exhaustive()
    }
}
