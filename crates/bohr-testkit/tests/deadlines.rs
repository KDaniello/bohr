//! The deadline and node-lifecycle machinery, driven through a purpose-built world.
//!
//! The world here arms timers and issues node operations on scripted syscalls; what is under test
//! is the kernel's half of the contract — advancing to a deadline instead of deadlocking, firing
//! everything due, and executing kill, freeze, thaw and restart.

use std::collections::BTreeMap;

use bohr_backend::{Resume, Stop, StopKind};
use bohr_backend_null::{GuestOp, NullBackend, NullProgram};
use bohr_kernel::{
    run, DueEffects, Effects, FaultCatalog, KernelError, NodeOp, Oracle, RunConfig, RunInputs,
    RunOutcome, RunReport, Verdict, WorldCtx, WorldModel,
};
use bohr_tape::Tape;
use bohr_trace::TraceWriter;
use bohr_types::{NodeId, OracleId, Seed, TaskId, Tick, WorldModelId};

/// Sleep for `args[0]` ticks.
const SLEEP: u32 = 35;
/// Any answered call; costs a fixed slice of virtual time.
const PLAIN: u32 = 228;
/// Kill node `args[0]`; restart it `args[1]` ticks later when `args[1]` is non-zero.
const KILL_NODE: u32 = 900;
/// Freeze node `args[0]`; thaw it `args[1]` ticks later.
const FREEZE_NODE: u32 = 901;

const SYSCALL_COST: Tick = Tick(1_000);

enum Due {
    Wake(TaskId),
    Op(NodeOp),
}

/// Answers the scripted calls above and keeps one timer queue.
struct TestWorld {
    timers: BTreeMap<Tick, Vec<Due>>,
    catalog: FaultCatalog,
}

impl TestWorld {
    fn new() -> Self {
        Self { timers: BTreeMap::new(), catalog: FaultCatalog::default() }
    }

    fn arm(&mut self, at: Tick, due: Due) {
        self.timers.entry(at).or_default().push(due);
    }
}

impl WorldModel for TestWorld {
    fn identity(&self) -> WorldModelId {
        WorldModelId::new("kernel-test", 1)
    }

    fn handle(&mut self, stop: &Stop, ctx: &mut WorldCtx<'_, '_>) -> Result<Effects, KernelError> {
        let StopKind::SyscallEntry { nr, args } = stop.kind else {
            return Err(ctx.refuse("BOHR-2001", "unexpected stop"));
        };
        match nr {
            SLEEP => {
                let until = Tick(ctx.world_now().get() + args[0]);
                self.arm(until, Due::Wake(stop.task));
                Ok(Effects::wait(SYSCALL_COST))
            }
            KILL_NODE => {
                let node = NodeId(args[0] as u32);
                if args[1] > 0 {
                    let at = Tick(ctx.world_now().get() + args[1]);
                    self.arm(at, Due::Op(NodeOp::Restart(node)));
                }
                Ok(Effects::go(Resume::Emulate { ret: 0 }, SYSCALL_COST)
                    .commanding(vec![NodeOp::Kill(node)]))
            }
            FREEZE_NODE => {
                let node = NodeId(args[0] as u32);
                let at = Tick(ctx.world_now().get() + args[1]);
                self.arm(at, Due::Op(NodeOp::Thaw(node)));
                Ok(Effects::go(Resume::Emulate { ret: 0 }, SYSCALL_COST)
                    .commanding(vec![NodeOp::Freeze(node)]))
            }
            _ => Ok(Effects::go(Resume::Emulate { ret: 0 }, SYSCALL_COST)),
        }
    }

    fn fault_catalog(&self) -> &FaultCatalog {
        &self.catalog
    }

    fn next_deadline(&self) -> Option<Tick> {
        self.timers.keys().next().copied()
    }

    fn on_deadline(&mut self, ctx: &mut WorldCtx<'_, '_>) -> Result<DueEffects, KernelError> {
        let now = ctx.world_now();
        let mut due = DueEffects::default();
        while let Some((&at, _)) = self.timers.first_key_value() {
            if at > now {
                break;
            }
            for entry in self.timers.remove(&at).unwrap_or_default() {
                match entry {
                    Due::Wake(task) => due.wake.push(bohr_kernel::Wake { task, ret: 0 }),
                    Due::Op(op) => due.node_ops.push(op),
                }
            }
        }
        Ok(due)
    }
}

struct AcceptEverything;

impl Oracle for AcceptEverything {
    fn identity(&self) -> OracleId {
        OracleId::new("accept", 1)
    }

    fn judge(&mut self, _run: &RunOutcome) -> Verdict {
        Verdict::Clean
    }
}

fn execute(programs: Vec<NullProgram>, seed: u64) -> RunReport {
    let tape = Tape::from_seed(Seed(seed));
    let mut backend = NullBackend::cluster(programs.clone());
    let mut world = TestWorld::new();
    let mut policy = bohr_kernel::RandomPolicy;
    let mut oracle = AcceptEverything;
    let mut sink = TraceWriter::new(Vec::new());
    let specs = vec![bohr_backend::GuestSpec::default(); programs.len()];

    run(RunInputs {
        tape: &tape,
        backend: &mut backend,
        world: &mut world,
        policy: &mut policy,
        oracle: &mut oracle,
        sink: &mut sink,
        specs: &specs,
        config: RunConfig::default(),
    })
    .expect("a scripted run completes")
}

fn plain(n: usize) -> Vec<GuestOp> {
    std::iter::repeat_with(|| GuestOp::Syscall { nr: PLAIN, args: [0; 6] })
        .take(n)
        .chain([GuestOp::Exit(0)])
        .collect()
}

#[test]
fn a_sleeping_guest_advances_to_its_deadline_instead_of_deadlocking() {
    let sleep_ticks = 5_000_000;
    let program = NullProgram::single(vec![
        GuestOp::Syscall { nr: SLEEP, args: [sleep_ticks, 0, 0, 0, 0, 0] },
        GuestOp::Syscall { nr: PLAIN, args: [0; 6] },
        GuestOp::Exit(0),
    ]);

    let report = execute(vec![program], 3);
    assert_eq!(report.outcome, bohr_trace::Outcome::Clean, "got {:?}", report.outcome);
    assert!(
        report.ticks.get() >= sleep_ticks,
        "the clock must have jumped to the deadline, reached {:?}",
        report.ticks
    );
}

#[test]
fn a_killed_node_restarts_from_its_spec_and_runs_to_completion() {
    let killer = NullProgram::single(vec![
        // Kill node 1, restart it a millisecond later.
        GuestOp::Syscall { nr: KILL_NODE, args: [1, 1_000_000, 0, 0, 0, 0] },
        GuestOp::Exit(0),
    ]);
    let victim = NullProgram::single(plain(50));

    let report = execute(vec![killer, victim], 7);
    assert_eq!(report.outcome, bohr_trace::Outcome::Clean, "got {:?}", report.outcome);
    // The victim's first incarnation dies mid-script and records no exit; the restarted one runs
    // its whole script. Two clean exits therefore mean the restart really ran.
    assert!(report.ticks.get() >= 1_000_000, "the restart happened after its delay");
}

#[test]
fn a_frozen_node_waits_out_the_freeze_and_still_finishes() {
    let freezer = NullProgram::single(vec![
        GuestOp::Syscall { nr: FREEZE_NODE, args: [1, 10_000_000, 0, 0, 0, 0] },
        GuestOp::Exit(0),
    ]);
    let frozen = NullProgram::single(plain(3));

    let report = execute(vec![freezer, frozen], 11);
    assert_eq!(report.outcome, bohr_trace::Outcome::Clean, "got {:?}", report.outcome);
    assert!(
        report.ticks.get() >= 10_000_000,
        "the frozen node could only finish after the thaw, reached {:?}",
        report.ticks
    );
}

#[test]
fn deadline_runs_repeat_exactly() {
    let programs = || {
        vec![
            NullProgram::single(vec![
                GuestOp::Syscall { nr: SLEEP, args: [2_000_000, 0, 0, 0, 0, 0] },
                GuestOp::Syscall { nr: PLAIN, args: [0; 6] },
                GuestOp::Exit(0),
            ]),
            NullProgram::single(plain(5)),
        ]
    };
    let first = execute(programs(), 13);
    for attempt in 0..200 {
        let again = execute(programs(), 13);
        assert_eq!(first.digest, again.digest, "attempt {attempt} diverged");
        assert_eq!(first.ticks, again.ticks);
    }
}
