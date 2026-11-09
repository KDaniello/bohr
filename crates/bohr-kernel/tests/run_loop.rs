//! The run loop, driven where it lives: a scripted backend and a scripted world, one test per way a
//! run can end and per operation a world can order on a node.

use std::collections::{BTreeMap, VecDeque};

use bohr_backend::{
    BackendCapabilities, BackendError, DeterminismBackend, GuestExit, GuestSpec, Mapping,
    ProgressUnit, Regs, Resume, Stop, StopKind,
};
use bohr_kernel::{
    DueEffects, Effects, FaultCatalog, KernelError, NodeOp, Observer, Oracle, Progress,
    RandomPolicy, RunConfig, RunInputs, RunOutcome, RunReport, StopReason, Verdict, Wake, WorldCtx,
    WorldModel, run,
};
use bohr_tape::Tape;
use bohr_trace::{EventKind, Outcome, TraceWriter};
use bohr_types::{EventId, GuestAddr, NodeId, OracleId, Seed, TaskId, Tick, WorldModelId};

/// One thing a scripted task does when it next runs.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// Enters the system call with this number; the world below decides what it means.
    Call(u32),
    /// Exits with this status.
    Exit(i32),
    /// Creates a thread, which runs the backend's child script.
    Spawn,
    /// Runs until its budget of this many progress units is gone.
    Burn(u64),
}

/// A backend that runs scripts instead of programs.
struct Scripted {
    /// What the first task of each node runs, by node; a restarted node runs its script again.
    scripts: Vec<Vec<Step>>,
    child: Vec<Step>,
    next_id: u32,
    queues: BTreeMap<TaskId, VecDeque<Step>>,
    running: Option<TaskId>,
    spawns: BTreeMap<NodeId, u32>,
}

impl Scripted {
    fn new(scripts: Vec<Vec<Step>>, child: Vec<Step>) -> Self {
        Self {
            scripts,
            child,
            next_id: 1,
            queues: BTreeMap::new(),
            running: None,
            spawns: BTreeMap::new(),
        }
    }

    fn fresh(&mut self, steps: Vec<Step>) -> TaskId {
        let task = TaskId(self.next_id);
        self.next_id += 1;
        self.queues.insert(task, steps.into());
        task
    }
}

impl DeterminismBackend for Scripted {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            name: "scripted",
            traps_rdtsc: false,
            traps_cpuid: false,
            traps_rdrand: false,
            progress_unit: ProgressUnit::Syscalls,
            hands_over_to_debugger: false,
            max_tasks: 1024,
            checkpoints: false,
        }
    }

    fn spawn(&mut self, node: NodeId, _spec: &GuestSpec) -> Result<TaskId, BackendError> {
        *self.spawns.entry(node).or_default() += 1;
        let script = self.scripts[node.get() as usize].clone();
        Ok(self.fresh(script))
    }

    fn next_stop(&mut self) -> Result<Option<Stop>, BackendError> {
        let Some(task) = self.running.take() else {
            return Ok(None);
        };
        let step =
            self.queues.get_mut(&task).and_then(VecDeque::pop_front).unwrap_or(Step::Exit(0));
        let stop = match step {
            Step::Call(nr) => Stop { task, kind: StopKind::SyscallEntry { nr, args: [0; 6] } },
            Step::Burn(consumed) => Stop { task, kind: StopKind::BudgetExhausted { consumed } },
            Step::Exit(code) => {
                self.queues.remove(&task);
                Stop { task, kind: StopKind::TaskExited { status: GuestExit::Exited(code) } }
            }
            Step::Spawn => {
                let child = self.fresh(self.child.clone());
                Stop {
                    task: child,
                    kind: StopKind::TaskCreated { parent: task, shares_memory: true },
                }
            }
        };
        Ok(Some(stop))
    }

    fn resume(&mut self, task: TaskId, action: Resume) -> Result<(), BackendError> {
        if matches!(action, Resume::Terminate { .. }) {
            self.queues.remove(&task);
        } else {
            self.running = Some(task);
        }
        Ok(())
    }

    fn read_mem(&mut self, _: TaskId, _: GuestAddr, _: &mut [u8]) -> Result<(), BackendError> {
        Ok(())
    }

    fn write_mem(&mut self, _: TaskId, _: GuestAddr, _: &[u8]) -> Result<(), BackendError> {
        Ok(())
    }

    fn regs(&mut self, _: TaskId) -> Result<Regs, BackendError> {
        Err(BackendError::Lost("a scripted task has no registers".to_owned()))
    }

    fn set_regs(&mut self, _: TaskId, _: &Regs) -> Result<(), BackendError> {
        Err(BackendError::Lost("a scripted task has no registers".to_owned()))
    }

    fn mappings(&mut self, _: TaskId) -> Result<Vec<Mapping>, BackendError> {
        Ok(Vec::new())
    }

    /// Ends a task and reports nothing about it afterwards, as both real backends do: the task is
    /// already forgotten and there is nobody left to tell.
    fn kill(&mut self, task: TaskId) -> Result<(), BackendError> {
        self.queues.remove(&task);
        if self.running == Some(task) {
            self.running = None;
        }
        Ok(())
    }
}

/// What each call number means to the scripted world.
mod call {
    pub(super) const PLAIN: u32 = 1;
    pub(super) const BLOCK: u32 = 2;
    pub(super) const WAKE_ALL: u32 = 3;
    pub(super) const ARM_WAKE: u32 = 4;
    pub(super) const NOTE: u32 = 5;
    pub(super) const KILL_NODE_1: u32 = 10;
    pub(super) const FREEZE_NODE_1: u32 = 11;
    pub(super) const THAW_NODE_1: u32 = 12;
    pub(super) const RESTART_NODE_1: u32 = 13;
    pub(super) const END_MY_NODE: u32 = 14;
    pub(super) const IMPOSSIBLE: u32 = 15;
}

const COST: Tick = Tick(1_000);

/// A world that answers by call number and keeps one timer.
#[derive(Default)]
struct Answers {
    blocked: Vec<TaskId>,
    deadline: Option<Tick>,
    catalog: FaultCatalog,
}

impl WorldModel for Answers {
    fn identity(&self) -> WorldModelId {
        WorldModelId::new("scripted", 1)
    }

    fn handle(&mut self, stop: &Stop, ctx: &mut WorldCtx<'_, '_>) -> Result<Effects, KernelError> {
        let StopKind::SyscallEntry { nr, .. } = stop.kind else {
            return Ok(Effects::go(Resume::Continue, Tick::ZERO));
        };
        let done = Effects::go(Resume::Emulate { ret: 0 }, COST);
        Ok(match nr {
            call::BLOCK => {
                self.blocked.push(ctx.task());
                Effects::wait(COST)
            }
            call::WAKE_ALL => {
                done.releasing(self.blocked.drain(..).map(|task| Wake { task, ret: 0 }).collect())
            }
            call::ARM_WAKE => {
                self.deadline = Some(ctx.world_now().saturating_add(Tick(5 * COST.get())));
                done
            }
            call::NOTE => {
                ctx.emit(EventKind::Diagnostic {
                    code: "TEST".to_owned(),
                    message: "n".to_owned(),
                })?;
                done
            }
            call::KILL_NODE_1 => done.commanding(vec![NodeOp::Kill(NodeId(1))]),
            call::FREEZE_NODE_1 => done.commanding(vec![NodeOp::Freeze(NodeId(1))]),
            call::THAW_NODE_1 => done.commanding(vec![NodeOp::Thaw(NodeId(1))]),
            call::RESTART_NODE_1 => done.commanding(vec![NodeOp::Restart(NodeId(1))]),
            call::END_MY_NODE => {
                done.commanding(vec![NodeOp::Ended { node: ctx.node(), by: ctx.task() }])
            }
            call::IMPOSSIBLE => {
                return Err(KernelError::Invariant("the world was asked the impossible"));
            }
            _ => done,
        })
    }

    fn fault_catalog(&self) -> &FaultCatalog {
        &self.catalog
    }

    fn next_deadline(&self) -> Option<Tick> {
        self.deadline
    }

    fn on_deadline(&mut self, _ctx: &mut WorldCtx<'_, '_>) -> Result<DueEffects, KernelError> {
        self.deadline = None;
        Ok(DueEffects {
            wake: self.blocked.drain(..).map(|task| Wake { task, ret: 0 }).collect(),
            node_ops: Vec::new(),
        })
    }
}

struct Clean;

impl Oracle for Clean {
    fn identity(&self) -> OracleId {
        OracleId::new("clean", 1)
    }

    fn judge(&mut self, _run: &RunOutcome) -> Verdict {
        Verdict::Clean
    }
}

#[derive(Default)]
struct Watching {
    reports: Vec<Progress>,
}

impl Observer for Watching {
    fn progress(&mut self, progress: &Progress) {
        self.reports.push(*progress);
    }
}

/// What a scripted run gave back, with the backend it ran on.
struct Ran {
    result: Result<RunReport, KernelError>,
    backend: Scripted,
    progress: Vec<Progress>,
}

fn run_scripts(scripts: Vec<Vec<Step>>, child: Vec<Step>, config: RunConfig) -> Ran {
    let tape = Tape::from_seed(Seed(1));
    let mut backend = Scripted::new(scripts.clone(), child);
    let mut world = Answers::default();
    let mut policy = RandomPolicy;
    let mut oracle = Clean;
    let mut sink = TraceWriter::new(Vec::new());
    let mut watching = Watching::default();
    let specs = vec![GuestSpec::default(); scripts.len()];
    let result = run(RunInputs {
        tape: &tape,
        backend: &mut backend,
        world: &mut world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        observer: Some(&mut watching),
        specs: &specs,
        config,
    });
    Ran { result, backend, progress: watching.reports }
}

fn report(scripts: Vec<Vec<Step>>, config: RunConfig) -> RunReport {
    run_scripts(scripts, Vec::new(), config).result.expect("the run completes")
}

fn calls(nr: u32, n: usize) -> Vec<Step> {
    vec![Step::Call(nr); n]
}

// ---- how a run ends -----------------------------------------------------------------------

#[test]
fn a_guest_that_exits_finishes_the_run() {
    let r = report(
        vec![vec![Step::Call(call::PLAIN), Step::Call(call::PLAIN), Step::Exit(0)]],
        RunConfig::default(),
    );
    assert_eq!(r.stop_reason, StopReason::GuestFinished);
    assert_eq!(r.outcome, Outcome::Clean);
    assert_eq!(r.metrics.stops, 2, "two calls answered by the world");
    assert_eq!(r.ticks, Tick(2 * COST.get()));
}

#[test]
fn every_task_waiting_with_nothing_armed_is_a_deadlock() {
    let r = report(vec![vec![Step::Call(call::BLOCK)]], RunConfig::default());
    assert_eq!(r.stop_reason, StopReason::Deadlock);
    assert!(matches!(r.outcome, Outcome::GuestHung(_)), "{:?}", r.outcome);
}

#[test]
fn an_armed_timer_releases_waiting_tasks_instead_of_a_deadlock() {
    let r = report(
        vec![vec![Step::Call(call::ARM_WAKE), Step::Call(call::BLOCK), Step::Exit(0)]],
        RunConfig::default(),
    );
    assert_eq!(r.stop_reason, StopReason::GuestFinished);
    assert!(r.ticks >= Tick(5 * COST.get()), "time jumped to the timer");
}

#[test]
fn virtual_time_past_its_budget_is_a_hang() {
    let config = RunConfig { max_ticks: Tick(5 * COST.get()), ..RunConfig::default() };
    let r = report(vec![calls(call::PLAIN, 50)], config);
    assert_eq!(r.stop_reason, StopReason::TickBudget);
    assert!(matches!(r.outcome, Outcome::GuestHung(_)));
}

#[test]
fn events_past_their_budget_end_the_run() {
    let config = RunConfig { max_events: 3, ..RunConfig::default() };
    let r = report(vec![calls(call::NOTE, 20)], config);
    assert_eq!(r.stop_reason, StopReason::EventBudget);
    assert_eq!(r.events, 3);
}

#[test]
fn a_requested_break_stops_before_the_event_it_names() {
    let config = RunConfig { break_at: Some(EventId(2)), ..RunConfig::default() };
    let r = report(vec![calls(call::NOTE, 20)], config);
    assert_eq!(r.stop_reason, StopReason::BreakRequested);
    assert!(matches!(r.outcome, Outcome::StoppedOnRequest(_)));
    assert_eq!(r.events, 2);
}

#[test]
fn a_horizon_ends_a_run_that_would_not_end_itself() {
    let config = RunConfig { horizon: Some(Tick(3 * COST.get())), ..RunConfig::default() };
    let r = report(vec![calls(call::PLAIN, 50)], config);
    assert_eq!(r.stop_reason, StopReason::HorizonReached);
    assert_eq!(r.outcome, Outcome::Clean, "a horizon is judged, not abandoned");
}

#[test]
fn the_named_node_ending_ends_the_run() {
    let config = RunConfig { ends_with: Some(NodeId(0)), ..RunConfig::default() };
    let r = report(
        vec![vec![Step::Call(call::PLAIN), Step::Exit(0)], vec![Step::Call(call::BLOCK)]],
        config,
    );
    assert_eq!(
        r.stop_reason,
        StopReason::NodeFinished,
        "not a deadlock: the server is left serving"
    );
}

// ---- what a world orders on a node --------------------------------------------------------

#[test]
fn a_killed_node_is_not_a_crash_of_the_guest() {
    let r = report(
        vec![
            vec![Step::Call(call::KILL_NODE_1), Step::Call(call::PLAIN), Step::Exit(0)],
            vec![Step::Call(call::BLOCK)],
        ],
        RunConfig::default(),
    );
    assert_eq!(r.stop_reason, StopReason::GuestFinished);
    assert_eq!(r.outcome, Outcome::Clean, "the injector's signal is not the guest's");
}

#[test]
fn a_frozen_node_left_frozen_is_waited_for_and_thawed_runs() {
    let frozen = report(
        vec![
            vec![Step::Call(call::FREEZE_NODE_1), Step::Exit(0)],
            vec![Step::Call(call::PLAIN), Step::Exit(0)],
        ],
        RunConfig::default(),
    );
    assert_eq!(frozen.stop_reason, StopReason::Deadlock, "a frozen task is not runnable");

    let thawed = report(
        vec![
            vec![Step::Call(call::FREEZE_NODE_1), Step::Call(call::THAW_NODE_1), Step::Exit(0)],
            vec![Step::Call(call::PLAIN), Step::Exit(0)],
        ],
        RunConfig::default(),
    );
    assert_eq!(thawed.stop_reason, StopReason::GuestFinished);
}

#[test]
fn a_restarted_node_runs_its_program_again() {
    let ran = run_scripts(
        vec![
            vec![Step::Call(call::KILL_NODE_1), Step::Call(call::RESTART_NODE_1), Step::Exit(0)],
            vec![Step::Call(call::PLAIN), Step::Exit(0)],
        ],
        Vec::new(),
        RunConfig::default(),
    );
    let r = ran.result.expect("the run completes");
    assert_eq!(r.stop_reason, StopReason::GuestFinished);
    assert_eq!(ran.backend.spawns[&NodeId(1)], 2, "spawned once and again");
}

/// A killed task leaves the node's books at once: no backend reports its exit, so a record kept for
/// that exit would count against the node for ever, and a restarted node that exited cleanly could
/// never be restarted again.
#[test]
fn a_restarted_node_that_exits_can_be_restarted_again() {
    let ran = run_scripts(
        vec![
            vec![
                Step::Call(call::KILL_NODE_1),
                Step::Call(call::RESTART_NODE_1),
                Step::Call(call::ARM_WAKE),
                Step::Call(call::BLOCK),
                // The restarted node has exited by the second timer, whatever the schedule.
                Step::Call(call::ARM_WAKE),
                Step::Call(call::BLOCK),
                Step::Call(call::RESTART_NODE_1),
                Step::Exit(0),
            ],
            vec![Step::Call(call::BLOCK), Step::Exit(0)],
        ],
        Vec::new(),
        RunConfig::default(),
    );
    ran.result.expect("a node that exited is dead, and may be restarted");
    assert_eq!(ran.backend.spawns[&NodeId(1)], 3);
}

#[test]
fn a_node_whose_process_ended_takes_its_waiting_threads_with_it() {
    let ran = run_scripts(
        vec![vec![Step::Spawn, Step::Call(call::END_MY_NODE), Step::Exit(0)]],
        vec![Step::Call(call::BLOCK)],
        RunConfig { ends_with: Some(NodeId(0)), ..RunConfig::default() },
    );
    let r = ran.result.expect("the run completes");
    assert_eq!(
        r.stop_reason,
        StopReason::NodeFinished,
        "the parked thread is retired, not waited for"
    );
}

// ---- what the loop reports about itself ---------------------------------------------------

#[test]
fn a_violated_invariant_says_where() {
    let ran = run_scripts(
        vec![vec![Step::Call(call::PLAIN), Step::Call(call::IMPOSSIBLE)]],
        Vec::new(),
        RunConfig::default(),
    );
    match ran.result {
        Err(KernelError::InvariantAt { what, tick, task, .. }) => {
            assert_eq!(what, "the world was asked the impossible");
            assert_eq!(tick, COST.get(), "after the first call");
            assert_eq!(task, "task 1");
        }
        other => panic!("expected a located invariant, got {other:?}"),
    }
}

#[test]
fn restarting_a_live_node_is_refused_where_it_happened() {
    let ran = run_scripts(
        vec![vec![Step::Call(call::RESTART_NODE_1)], vec![Step::Call(call::BLOCK)]],
        Vec::new(),
        RunConfig::default(),
    );
    let message = ran.result.expect_err("a live node cannot be restarted").to_string();
    assert!(message.contains("restarting a node that is not dead"), "{message}");
    assert!(message.contains("BOHR-5001") && message.contains("event"), "{message}");
}

#[test]
fn computation_costs_time_only_where_a_rate_is_given() {
    let script = vec![Step::Burn(1_000), Step::Exit(0)];
    let free = report(vec![script.clone()], RunConfig::default());
    assert_eq!(free.ticks, Tick::ZERO, "without a rate, computation costs no virtual time");

    let config = RunConfig { ticks_per_progress_unit: Some(3), ..RunConfig::default() };
    let priced = report(vec![script], config);
    assert_eq!(priced.ticks, Tick(3_000));
    assert_eq!(priced.metrics.ticks_by_task[&TaskId(1)], Tick(3_000));
}

#[test]
fn the_loop_counts_what_it_did_and_tells_an_observer_once_more_at_the_end() {
    let ran = run_scripts(
        vec![vec![Step::Spawn, Step::Call(call::PLAIN), Step::Exit(0)]],
        vec![Step::Call(call::PLAIN), Step::Exit(0)],
        RunConfig::default(),
    );
    let r = ran.result.expect("the run completes");
    assert!(r.metrics.decisions >= 4, "{:?}", r.metrics);
    assert_eq!(r.metrics.stops, 2);
    let last = ran.progress.last().expect("an observer hears the end");
    assert!(last.finished);
    assert_eq!(last.events, r.events);
}

#[test]
fn the_same_tape_gives_the_same_run() {
    let scripts = || {
        vec![
            vec![Step::Spawn, Step::Call(call::NOTE), Step::Call(call::BLOCK), Step::Exit(0)],
            vec![Step::Call(call::NOTE), Step::Call(call::WAKE_ALL), Step::Exit(0)],
        ]
    };
    let child = vec![Step::Call(call::NOTE), Step::Exit(0)];
    let a = run_scripts(scripts(), child.clone(), RunConfig::default()).result.unwrap();
    let b = run_scripts(scripts(), child, RunConfig::default()).result.unwrap();
    assert_eq!(a.digest, b.digest);
    assert_eq!(a.metrics, b.metrics);
}
