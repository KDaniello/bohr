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
    /// Threads are created by a call the model has already seen, and this is what ties that call to
    /// the task it produced: the identifier does not exist until the task does.
    fn task_created(&mut self, _task: TaskId, _parent: TaskId) {}

    /// A task ended, and what its ending releases.
    ///
    /// A thread ending is a wake: whoever joined it was waiting on a word the kernel clears when
    /// the thread goes. Nothing can be read from the task at this point — it is already gone — so
    /// the model answers from what it recorded when the task was created.
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
    /// Called by the kernel at the top of every scheduling iteration while
    /// [`Self::next_deadline`] reads due, and after advancing the clock to the next deadline when
    /// nothing is schedulable. The context is detached — there is no current task — so events are
    /// recorded through [`WorldCtx::emit_for`] and memory reached through the `_for` accessors.
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
    /// A wake is an effect of the stop rather than a stop of its own: the woken task is not
    /// executing and has nothing to report until the scheduler picks it.
    pub wake: Vec<Wake>,
    /// Whether the task asked to give up the processor.
    ///
    /// A policy that runs one task in long stretches has to be told: a guest spinning on a flag
    /// another task sets would otherwise spin until its budget ran out, having asked to be
    /// interrupted every time round the loop.
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
    /// Waiting on something only the world can release, and not a scheduling candidate until it
    /// does. Every task blocked with nothing left to release it is a deadlock, which is a finding.
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
/// The split follows event authorship: the world has already arranged its own consequences —
/// dropped what a crash loses, emitted the `FaultFired` and `NodeState` events — before returning
/// one of these. The kernel's half is what only it can do: task bookkeeping and backend calls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeOp {
    /// End every task of the node, now.
    Kill(NodeId),
    /// Keep the node's tasks out of scheduling until thawed. Wakes still land; frozen tasks
    /// simply are not offered to the policy, which is what makes a freeze a pause and not a drop.
    Freeze(NodeId),
    /// Let a frozen node run again.
    Thaw(NodeId),
    /// Re-spawn a dead node's initial task from the spec it was started with. The node keeps the
    /// clock it drew at spawn, so a restart cannot shift the tape position of later draws.
    Restart(NodeId),
    /// The node's process has ended of its own accord, taking every one of its tasks with it.
    ///
    /// Not a kill and never narrated as one: `exit_group` is what a program calls when it is done,
    /// and every thread of the process goes with it whether or not it was in the middle of
    /// something. Without this, a runtime that parks its workers on a futex and then exits -- the
    /// Go runtime does -- leaves them parked for ever, and a run that finished is reported as one
    /// where every task is waiting.
    Ended {
        /// The node whose process is ending.
        node: NodeId,
        /// The task that called it, which is spared: it has still to execute the call and deliver
        /// the status the whole process exits with.
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
    /// Where independent implementations of one specification must agree, a disagreement is a bug
    /// in one of them without anyone having to say which behaviour was correct.
    fn judge_group(&mut self, _runs: &[RunOutcome]) -> Verdict {
        Verdict::Skipped
    }
}

/// Something a world model noticed and wrote down, as an oracle reads it.
///
/// The message travels with the code because an oracle that sees only the code cannot say *which*
/// promise was broken — and "an assertion the guest makes about itself did not hold" is the same
/// sentence for every assertion any guest will ever write.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    /// The code that names the class, stable forever (`docs/API.md` §8).
    pub code: String,
    /// What was noticed, in the words the model used.
    pub message: String,
}

/// What a run accumulated for whoever judges it.
///
/// Gathered into one value because a context that carried a borrow per fact would grow an argument
/// every time a run learned to notice something new.
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
    /// **A campaign that injected nothing looks exactly like one that injected plenty and found
    /// nothing, and only this tells them apart.** Faults are drawn, so whether any fired is a fact
    /// about the run rather than about the configuration: a scenario can arm a policy that a run
    /// ends before, and reporting the clean runs without this would be reporting that a question
    /// was asked when it was not.
    pub(crate) const fn adversity(&self) -> u32 {
        self.adversity
    }

    /// The event where a promise the guest made about itself first did not hold.
    ///
    /// The *first*, and never the last event of the run: a violated assertion is followed by
    /// whatever the guest did next, and pointing a reader at that instead is pointing them past
    /// the answer.
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
    /// A task killed by a fault returns nothing at all, so an oracle reading only the exit status
    /// would call a crashed guest clean.
    pub killed_by: Option<u32>,
    /// What the run noticed and wrote down, in the order it did.
    pub diagnostics: Vec<Diagnostic>,
    /// What this run is compared by, where anything asked it to be.
    ///
    /// Empty for an ordinary run, which is every run against one implementation. Filled by the
    /// crate that carries runs out, because a projection is read off the world after the run ends
    /// and the kernel is finished with the world by then.
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
/// Called by the loop between stops with a copy of where the run stands; nothing it does is read
/// back, so an observer cannot change a run.
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
/// Exposes the clock, the tape and guest memory through methods rather than fields, so that a model
/// cannot take ownership of any of them or advance the cursor without the draw being journalled.
///
/// While handling a stop the context carries the stopped task as its origin, and the short
/// accessors ([`Self::task`], [`Self::now`], [`Self::emit`], the plain memory methods) speak about
/// it. Inside [`WorldModel::on_deadline`] there is no current task: the context is detached, those
/// accessors panic, and the model attributes everything explicitly through the `_for` methods.
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
    /// Panicking rather than erroring is deliberate: a model asking "which task?" while firing
    /// deadlines is a bug in the model, not a state a run can reach.
    #[track_caller]
    fn origin(&self) -> (NodeId, TaskId) {
        self.origin.expect("no current task while firing deadlines; attribute explicitly")
    }

    /// What the node this stop happened on reads for the time.
    ///
    /// Not the world clock: a node has its own offset and drift, and a guest that compares
    /// timestamps with another node is meant to see them disagree.
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
    /// [`Self::now`] on top of the wall epoch until the node's clock is stepped; after a step back
    /// it can read earlier than before, as a wall clock can.
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

    /// Writes into the memory of a named task — a delivery lands in the receiver's buffer while
    /// the receiver is not the task being handled.
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
    /// No world re-draws a clock during a run: clocks are skewed once, before the first task runs
    /// (`docs/ARCHITECTURE.md` §4.5).
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
    /// Needed where a trapped instruction is emulated rather than a syscall: the result has to be
    /// placed in the registers the instruction would have written, and the instruction pointer
    /// moved past it.
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

    /// Records an event attributed to a named node and task — cross-node facts such as a delivery
    /// on the receiver, and everything emitted while firing deadlines.
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
            // Read from the event rather than from the world's own bookkeeping, so a model that
            // records a broken promise gets the point of violation for free and no model has to
            // remember to report it twice.
            EventKind::Assertion { condition: false, .. } => {
                self.recorded.violation_event.get_or_insert(*self.next_event);
            }
            // Both vocabularies, because the guest cannot tell them apart and this number is about
            // the guest. A fault is something the world was told to do; a packet lost on a lossy
            // link is the medium being the medium — and a campaign whose only adversity is a lossy
            // link would otherwise report that it injected nothing. A packet doubled, overtaken or
            // altered fires a fault of its own, and is counted there once.
            EventKind::FaultFired { .. }
            | EventKind::PacketFate { fate: PacketFate::Dropped, .. } => {
                self.recorded.adversity = self.recorded.adversity.saturating_add(1);
            }
            _ => {}
        }
        let event = Event { id: *self.next_event, tick: self.clock.now(), node, task, kind };
        self.sink.emit(&event)?;
        *self.next_event = EventId(self.next_event.get() + 1);
        // Draws made after this point belong to the next event, which is what lets the journal be
        // read alongside the trace.
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
