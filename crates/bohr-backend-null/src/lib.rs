//! An in-process backend that executes a scripted guest.
//!
//! There is no operating system here and nothing is intercepted: a guest is a list of operations,
//! and this crate hands them to the kernel through the same boundary a real one does. That makes
//! the clock, the scheduler, the world and the trace testable in microseconds, on any operating
//! system, with no privileges — which is what keeps work on those layers independent of work on
//! interception.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

mod chaos;

use bohr_backend::{
    BackendCapabilities, BackendError, CoverageMap, DeterminismBackend, GuestExit, GuestSpec,
    Mapping, ProgressUnit, Regs, Resume, Stop, StopKind,
};
use bohr_types::{GuestAddr, NodeId, TaskId};

/// First address of a scripted guest's memory.
///
/// Every node's memory starts here: colliding addresses across nodes are the point, because that
/// is what a second address space looks like.
pub const MEMORY_BASE: u64 = 0x1000;

/// Size of one node's memory.
pub const MEMORY_BYTES: usize = 64 * 1024;

/// One step of a scripted guest.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GuestOp {
    /// Ask the world for something.
    Syscall {
        /// Syscall number, using the same numbering a real guest would.
        nr: u32,
        /// Argument registers, in ABI order.
        args: [u64; 6],
    },
    /// Create another task, which runs the next unused child program.
    Spawn,
    /// Continue the script at `pc` instead of the next operation.
    ///
    /// Control flow only; no stop is produced. A cycle that reaches no stopping operation never
    /// returns to the kernel, so a scripted spin goes through a syscall.
    Jump {
        /// Operation index to continue at.
        pc: usize,
    },
    /// Continue at `pc` when the eight bytes at `addr` equal `equals`; fall through otherwise.
    ///
    /// The scripted form of a guest branching on shared state, which is what cluster fixtures
    /// coordinate with. The same non-stopping caveat as [`GuestOp::Jump`] applies.
    JumpIfMem {
        /// Address of the eight bytes compared.
        addr: GuestAddr,
        /// Value that takes the branch.
        equals: u64,
        /// Operation index to continue at.
        pc: usize,
    },
    /// Continue at `pc` when the given bit of the byte at `addr` is set.
    ///
    /// One bit of state, which is what a coin flip is. [`GuestOp::JumpIfMem`] compares a whole
    /// word, so a script branching on something it drew would take that branch once in every two
    /// hundred and fifty-six runs — enough to test that branching works and far too rare to give a
    /// search anything to follow.
    JumpIfBit {
        /// Address of the byte examined.
        addr: GuestAddr,
        /// Which bit of it, counted from the least significant.
        bit: u8,
        /// Operation index to continue at.
        pc: usize,
    },
    /// Read the timestamp counter, as a real guest's `rdtsc` does.
    ///
    /// Here so the path the kernel takes for a trapped instruction is exercised on a machine with
    /// no interceptor. Without it, `StopKind::TimestampCounter` is a variant of the boundary that
    /// only Linux under `ptrace` can produce, and the layers above it are untested everywhere
    /// else.
    ReadTimestamp {
        /// Whether the `rdtscp` form is used, which also reports a processor number.
        rdtscp: bool,
    },
    /// Ask the processor to identify itself, as `cpuid` does.
    CpuId {
        /// Primary leaf.
        leaf: u32,
        /// Sub-leaf.
        subleaf: u32,
    },
    /// Receive a signal, as a guest that raised one on itself would.
    RaiseSignal {
        /// Signal number.
        signal: u32,
    },
    /// End this task.
    Exit(i32),
}

/// What a scripted guest does.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct NullProgram {
    /// Operations for the initial task.
    pub main: Vec<GuestOp>,
    /// Operations for tasks created by [`GuestOp::Spawn`], consumed in creation order.
    pub children: Vec<Vec<GuestOp>>,
}

impl NullProgram {
    /// A single-task program.
    #[must_use]
    pub fn single(ops: Vec<GuestOp>) -> Self {
        Self { main: ops, children: Vec::new() }
    }
}

#[derive(Clone, Debug)]
struct TaskState {
    node: NodeId,
    /// Operations this task may still execute before it must stop, where it was resumed with a
    /// budget. A scripted `Jump` cycle reaches no stopping operation, so without this the one
    /// instrument the boundary offers against a spinning guest did nothing here.
    budget: Option<u64>,
    ops: Vec<GuestOp>,
    pc: usize,
    /// The operation executed before this one, which is the other end of the edge just taken.
    prev_pc: usize,
    regs: Regs,
    live: bool,
}

/// One simulated machine: a program and an address space of its own.
#[derive(Clone, Debug)]
struct Node {
    program: NullProgram,
    memory: Vec<u8>,
    /// Memory as it was when the node first started; a restart begins from it again, the way a
    /// re-executed binary gets its data section back.
    initial: Option<Vec<u8>>,
    children_used: usize,
}

impl Node {
    fn new(program: NullProgram) -> Self {
        Self { program, memory: vec![0; MEMORY_BYTES], initial: None, children_used: 0 }
    }
}

/// Executes [`NullProgram`]s behind the backend boundary, one per node.
#[derive(Debug)]
pub struct NullBackend {
    nodes: BTreeMap<NodeId, Node>,
    tasks: BTreeMap<TaskId, TaskState>,
    current: Option<TaskId>,
    next_task: u32,
    /// Which transitions between operations this run took.
    ///
    /// A scripted program has a control-flow graph like any other — `Jump` and `JumpIfMem` are its
    /// branches — so the edges it walks are real coverage rather than a stand-in. That is what
    /// lets the whole search be exercised on a machine with no interceptor at all, which is
    /// §5.11 applied to the explorer.
    covered: CoverageMap,
}

impl NullBackend {
    /// A backend that will run `program` on a single node.
    #[must_use]
    pub fn new(program: NullProgram) -> Self {
        Self::cluster(vec![program])
    }

    /// A backend running one program per node, `NodeId(i)` running `programs[i]`.
    #[must_use]
    pub fn cluster(programs: Vec<NullProgram>) -> Self {
        let nodes = programs
            .into_iter()
            .enumerate()
            .map(|(i, program)| (NodeId(i as u32), Node::new(program)))
            .collect();
        Self {
            nodes,
            tasks: BTreeMap::new(),
            current: None,
            next_task: 0,
            covered: CoverageMap::new(),
        }
    }

    /// Marks the transition from one operation to the next.
    ///
    /// The index is AFL's: the previous location shifted and mixed with the current one, so that
    /// the *pair* is what identifies an edge. Hashing the pair rather than the destination is what
    /// tells a loop taken twice from two different callers apart, and that distinction is most of
    /// what makes coverage worth following.
    fn record_edge(&mut self, node: NodeId, from: usize, to: usize) {
        // The node is mixed into the low bits. Shifted up instead, it vanished: the map size is a
        // power of two, so the index keeps only the low bits and `node << 40` contributes none of
        // them -- every node of a cluster of identical programs landed on the others' counters,
        // and coverage stopped telling them apart at all.
        let mixed = u64::from(node.get()).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ ((from as u64) << 1)
            ^ (to as u64).wrapping_mul(31);
        #[cfg(feature = "chaos-coverage-ambient")]
        let mixed = mixed ^ crate::chaos::drift();
        let at = (mixed as usize) % bohr_backend::DEFAULT_COVERAGE_MAP_BYTES;
        let counter = &mut self.covered.as_bytes_mut()[at];
        *counter = counter.saturating_add(1);
    }

    /// Whatever the first node's guest has written into its memory.
    #[must_use]
    pub fn memory(&self) -> &[u8] {
        self.memory_of(NodeId::ZERO)
    }

    /// Whatever a node's guest has written into its memory.
    ///
    /// # Panics
    /// For a node this backend was not built with: the caller scripted the cluster and knows its
    /// size, so an unknown node is a bug in the test.
    #[must_use]
    pub fn memory_of(&self, node: NodeId) -> &[u8] {
        &self.nodes.get(&node).expect("a node this backend was built with").memory
    }

    /// Places bytes in the first node's memory before it starts.
    ///
    /// A scripted guest has no loader and no data section, so anything it passes by address — a
    /// path, a buffer to compare against — has to be put there by whoever wrote the script.
    pub fn preload(&mut self, addr: GuestAddr, data: &[u8]) -> Result<(), BackendError> {
        self.preload_on(NodeId::ZERO, addr, data)
    }

    /// Places bytes in a node's memory before it starts.
    pub fn preload_on(
        &mut self,
        node: NodeId,
        addr: GuestAddr,
        data: &[u8],
    ) -> Result<(), BackendError> {
        let start = Self::offset(addr, data.len())?;
        let node = self.nodes.get_mut(&node).ok_or_else(|| BackendError::CannotStart {
            program: "null".to_owned(),
            reason: "no program for that node".to_owned(),
        })?;
        node.memory[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn offset(addr: GuestAddr, len: usize) -> Result<usize, BackendError> {
        let start = addr
            .0
            .checked_sub(MEMORY_BASE)
            .and_then(|o| usize::try_from(o).ok())
            .ok_or(BackendError::BadAddress { addr, len })?;
        start
            .checked_add(len)
            .filter(|end| *end <= MEMORY_BYTES)
            .ok_or(BackendError::BadAddress { addr, len })?;
        Ok(start)
    }

    fn task_mut(&mut self, task: TaskId) -> Result<&mut TaskState, BackendError> {
        self.tasks.get_mut(&task).filter(|t| t.live).ok_or(BackendError::NoSuchTask { task })
    }

    fn node_of(&self, task: TaskId) -> Result<NodeId, BackendError> {
        self.tasks.get(&task).map(|t| t.node).ok_or(BackendError::NoSuchTask { task })
    }
}

impl DeterminismBackend for NullBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            name: "null",
            traps_rdtsc: true,
            traps_cpuid: true,
            // No real instruction stream exists, so there is nothing that could evade a trap.
            traps_rdrand: true,
            progress_unit: ProgressUnit::Operations,
            // A scripted guest is a data structure in this process. There is nothing for a
            // debugger to attach to, which is a different fact from a guest it could not reach.
            hands_over_to_debugger: false,
            max_tasks: 1024,
            checkpoints: false,
        }
    }

    fn spawn(&mut self, node: NodeId, _spec: &GuestSpec) -> Result<TaskId, BackendError> {
        if self.tasks.values().any(|t| t.live && t.node == node) {
            return Err(BackendError::CannotStart {
                program: "null".to_owned(),
                reason: "the node is already running".to_owned(),
            });
        }
        let (ops, task_node) = {
            let state = self.nodes.get_mut(&node).ok_or(BackendError::CannotStart {
                program: "null".to_owned(),
                reason: "no program for that node".to_owned(),
            })?;
            match &state.initial {
                // First start: freeze the preloaded image so a restart can begin from it again.
                None => state.initial = Some(state.memory.clone()),
                // Restart: fresh address space, fresh child budget, the way a re-executed binary
                // gets its data section back.
                Some(initial) => {
                    state.memory = initial.clone();
                    state.children_used = 0;
                }
            }
            (state.program.main.clone(), node)
        };
        let id = TaskId(self.next_task);
        self.next_task += 1;
        self.tasks.insert(
            id,
            TaskState {
                node: task_node,
                budget: None,
                ops,
                pc: 0,
                prev_pc: 0,
                regs: Regs::default(),
                live: true,
            },
        );
        Ok(id)
    }

    fn next_stop(&mut self) -> Result<Option<Stop>, BackendError> {
        let Some(task) = self.current.take() else {
            // `None` means every task has ended and the run is over
            // ([`DeterminismBackend::next_stop`]). Live tasks with none resumed is a caller that
            // forgot to resume one, and answering `None` to it converted a half-finished run into
            // a clean verdict.
            if self.tasks.values().any(|t| t.live) {
                return Err(BackendError::Lost(
                    "asked for the next stop with tasks still live and none resumed".to_owned(),
                ));
            }
            return Ok(None);
        };
        let mut spent = 0u64;
        loop {
            // Charged before the operation runs, so a cycle of non-stopping operations is bounded
            // by the budget rather than by the guest reaching a syscall of its own accord.
            if self.task_mut(task)?.budget.is_some_and(|budget| spent >= budget) {
                self.current = None;
                let kind = StopKind::BudgetExhausted { consumed: spent };
                return Ok(Some(Stop { task, kind }));
            }
            spent += 1;
            let (op, node, from, to) = {
                let state = self.task_mut(task)?;
                let op = state.ops.get(state.pc).cloned();
                let (from, to) = (state.prev_pc, state.pc);
                state.prev_pc = state.pc;
                state.pc += 1;
                (op, state.node, from, to)
            };
            self.record_edge(node, from, to);

            // Running off the end of a script is finishing, the same as exiting. Landing past it
            // by a jump is not: that is an index nobody meant, and reading it as a clean exit
            // turned a typo in a test into a passing run.
            let Some(op) = op else {
                if to > self.task_mut(task)?.ops.len() {
                    let reason = format!("a jump to operation {to} lands past the end of a script");
                    return Err(BackendError::BadProgram { reason });
                }
                self.task_mut(task)?.live = false;
                return Ok(Some(Stop {
                    task,
                    kind: StopKind::TaskExited { status: GuestExit::Exited(0) },
                }));
            };

            match op {
                GuestOp::Syscall { nr, args } => {
                    let state = self.task_mut(task)?;
                    state.regs.rax = u64::from(nr);
                    state.regs.args = args;
                    return Ok(Some(Stop { task, kind: StopKind::SyscallEntry { nr, args } }));
                }
                GuestOp::Spawn => {
                    let node = self.node_of(task)?;
                    let state = self.nodes.get_mut(&node).expect("a spawned task's node exists");
                    // A spawn with no child program left is a script that asked for a task it
                    // never wrote. Giving it an empty program made one that is born and dies
                    // clean, which is a test passing for want of the thing it meant to run.
                    let Some(ops) = state.program.children.get(state.children_used).cloned() else {
                        let asked = state.children_used + 1;
                        let reason = format!("spawn number {asked} has no child program");
                        return Err(BackendError::BadProgram { reason });
                    };
                    state.children_used += 1;
                    let child = TaskId(self.next_task);
                    self.next_task += 1;
                    self.tasks.insert(
                        child,
                        TaskState {
                            node,
                            budget: None,
                            ops,
                            pc: 0,
                            prev_pc: 0,
                            regs: Regs::default(),
                            live: true,
                        },
                    );
                    return Ok(Some(Stop {
                        task: child,
                        kind: StopKind::TaskCreated { parent: task, shares_memory: true },
                    }));
                }
                GuestOp::Jump { pc } => {
                    self.task_mut(task)?.pc = pc;
                }
                GuestOp::JumpIfMem { addr, equals, pc } => {
                    let node = self.node_of(task)?;
                    let start = Self::offset(addr, 8)?;
                    let memory = &self.nodes.get(&node).expect("a task's node exists").memory;
                    let mut word = [0u8; 8];
                    word.copy_from_slice(&memory[start..start + 8]);
                    if u64::from_le_bytes(word) == equals {
                        self.task_mut(task)?.pc = pc;
                    }
                }
                GuestOp::JumpIfBit { addr, bit, pc } => {
                    // Masked into range, the ninth bit silently became the first and the script
                    // branched on something nobody asked about.
                    if bit >= 8 {
                        let reason = format!("bit {bit} is not one of the eight in a byte");
                        return Err(BackendError::BadProgram { reason });
                    }
                    let node = self.node_of(task)?;
                    let start = Self::offset(addr, 1)?;
                    let byte = self.nodes.get(&node).expect("a task's node exists").memory[start];
                    if byte & (1 << bit) != 0 {
                        self.task_mut(task)?.pc = pc;
                    }
                }
                GuestOp::ReadTimestamp { rdtscp } => {
                    self.current = None;
                    return Ok(Some(Stop { task, kind: StopKind::TimestampCounter { rdtscp } }));
                }
                GuestOp::CpuId { leaf, subleaf } => {
                    self.current = None;
                    return Ok(Some(Stop {
                        task,
                        kind: StopKind::CpuIdentification { leaf, subleaf },
                    }));
                }
                GuestOp::RaiseSignal { signal } => {
                    self.current = None;
                    return Ok(Some(Stop { task, kind: StopKind::Signal { signal } }));
                }
                GuestOp::Exit(code) => {
                    self.task_mut(task)?.live = false;
                    return Ok(Some(Stop {
                        task,
                        kind: StopKind::TaskExited { status: GuestExit::Exited(code) },
                    }));
                }
            }
        }
    }

    fn resume(&mut self, task: TaskId, action: Resume) -> Result<(), BackendError> {
        let state = self.task_mut(task)?;
        match action {
            Resume::Terminate { .. } => {
                state.live = false;
                self.current = None;
            }
            Resume::Emulate { ret } => {
                state.regs.rax = ret as u64;
                self.current = Some(task);
            }
            Resume::Continue => {
                state.budget = None;
                self.current = Some(task);
            }
            Resume::ContinueWithBudget { budget } => {
                state.budget = Some(budget);
                self.current = Some(task);
            }
            // Refused for the same reason `Rewrite` is: a scripted guest has no handler to deliver
            // a signal to, and accepting the request would answer a question this backend cannot.
            Resume::DeliverSignal { .. } => {
                return Err(BackendError::InvalidResume { action: "DeliverSignal".to_owned() });
            }
            Resume::Rewrite { .. } => {
                return Err(BackendError::InvalidResume { action: "Rewrite".to_owned() });
            }
        }
        Ok(())
    }

    fn read_mem(
        &mut self,
        task: TaskId,
        addr: GuestAddr,
        out: &mut [u8],
    ) -> Result<(), BackendError> {
        let node = self.node_of(task)?;
        let start = Self::offset(addr, out.len())?;
        out.copy_from_slice(&self.nodes[&node].memory[start..start + out.len()]);
        Ok(())
    }

    fn write_mem(
        &mut self,
        task: TaskId,
        addr: GuestAddr,
        data: &[u8],
    ) -> Result<(), BackendError> {
        let node = self.node_of(task)?;
        let start = Self::offset(addr, data.len())?;
        let memory = &mut self.nodes.get_mut(&node).expect("a task's node exists").memory;
        memory[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn regs(&mut self, task: TaskId) -> Result<Regs, BackendError> {
        Ok(self.task_mut(task)?.regs)
    }

    fn set_regs(&mut self, task: TaskId, regs: &Regs) -> Result<(), BackendError> {
        self.task_mut(task)?.regs = *regs;
        Ok(())
    }

    fn mappings(&mut self, task: TaskId) -> Result<Vec<Mapping>, BackendError> {
        self.task_mut(task)?;
        Ok(vec![Mapping {
            start: GuestAddr(MEMORY_BASE),
            end: GuestAddr(MEMORY_BASE + MEMORY_BYTES as u64),
            executable: false,
            writable: true,
            path: None,
        }])
    }

    fn kill(&mut self, task: TaskId) -> Result<(), BackendError> {
        self.task_mut(task)?.live = false;
        if self.current == Some(task) {
            self.current = None;
        }
        Ok(())
    }

    fn coverage(&mut self) -> Option<&CoverageMap> {
        Some(&self.covered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend with nothing to hand over says so through the capability and answers with an
    /// empty list — which are two different claims, and a caller that read only the second would
    /// point somebody at a debugger session that cannot exist.
    #[test]
    fn a_scripted_guest_is_not_something_a_debugger_can_attach_to() {
        let mut backend = NullBackend::new(NullProgram::single(vec![GuestOp::Exit(0)]));
        assert!(!backend.capabilities().hands_over_to_debugger);
        assert!(backend.hand_over().expect("the default answers rather than failing").is_empty());
    }

    fn backend() -> NullBackend {
        NullBackend::new(NullProgram::single(vec![
            GuestOp::Syscall { nr: 228, args: [0; 6] },
            GuestOp::Exit(0),
        ]))
    }

    /// Nothing runs until it is resumed, and asking anyway is a mistake rather than an answer.
    ///
    /// `None` from the boundary means every task has ended. Returning it for "you have not
    /// resumed anybody" gave one word two meanings, and the kernel reads that word as a clean
    /// finish: a half-finished run became a passing verdict with no error anywhere on the way.
    #[test]
    fn a_task_only_advances_when_it_is_resumed() {
        let mut b = backend();
        let t = b.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
        assert!(
            matches!(b.next_stop(), Err(BackendError::Lost(_))),
            "asking with nobody resumed answered as though the run had finished"
        );

        b.resume(t, Resume::Continue).expect("resume");
        let stop = b.next_stop().expect("stop").expect("a stop");
        assert!(matches!(stop.kind, StopKind::SyscallEntry { nr: 228, .. }));
    }

    /// A budget that is accepted must be spent. The boundary offers it as the one bound on a guest
    /// that will not stop by itself, and a scripted `Jump` cycle reaches no stopping operation --
    /// so ignoring it meant the whole `BudgetExhausted` path was unreachable off Linux.
    #[test]
    fn a_progress_budget_stops_a_script_that_would_never_stop_itself() {
        let mut b = NullBackend::new(NullProgram::single(vec![GuestOp::Jump { pc: 0 }]));
        let t = b.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
        b.resume(t, Resume::ContinueWithBudget { budget: 32 }).expect("resume");

        let stop = b.next_stop().expect("stop").expect("a stop");
        let StopKind::BudgetExhausted { consumed } = stop.kind else {
            panic!("a spinning script ran past its budget: {:?}", stop.kind);
        };
        assert_eq!(consumed, 32, "the budget was not the bound");

        // A second budget starts again from nothing, rather than counting on from the first.
        b.resume(t, Resume::ContinueWithBudget { budget: 4 }).expect("resume");
        let stop = b.next_stop().expect("stop").expect("a stop");
        assert!(matches!(stop.kind, StopKind::BudgetExhausted { consumed: 4 }), "{:?}", stop.kind);
    }

    /// Two nodes running the same program walk the same operations, and the edges must still be
    /// told apart. Mixed into bits the map index throws away, the node vanished: a cluster of
    /// identical programs covered one node's worth of counters however many nodes it had.
    #[test]
    fn the_same_edge_on_two_nodes_is_two_edges() {
        let program = || NullProgram::single(vec![GuestOp::Exit(0)]);
        let mut one = NullBackend::cluster(vec![program(), program()]);
        let a = one.spawn(NodeId(0), &GuestSpec::default()).expect("spawn a");
        one.resume(a, Resume::Continue).expect("resume");
        one.next_stop().expect("stop");
        let after_first = one.coverage().expect("a map").edges();

        let b = one.spawn(NodeId(1), &GuestSpec::default()).expect("spawn b");
        one.resume(b, Resume::Continue).expect("resume");
        one.next_stop().expect("stop");
        assert!(
            one.coverage().expect("a map").edges() > after_first,
            "the second node covered nothing the first had not"
        );
    }

    /// A script is written by whoever wrote the test, so a defect in one is a defect in the test.
    /// Each of these was normalised into something that looked like a clean run.
    #[test]
    fn a_malformed_script_is_refused_rather_than_normalised() {
        let mut jumped = NullBackend::new(NullProgram::single(vec![GuestOp::Jump { pc: 99 }]));
        let t = jumped.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
        jumped.resume(t, Resume::Continue).expect("resume");
        assert!(
            matches!(jumped.next_stop(), Err(BackendError::BadProgram { .. })),
            "a jump past the end of a script read as a clean exit"
        );

        let mut unwritten = NullBackend::new(NullProgram {
            main: vec![GuestOp::Spawn, GuestOp::Exit(0)],
            children: Vec::new(),
        });
        let t = unwritten.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
        unwritten.resume(t, Resume::Continue).expect("resume");
        assert!(
            matches!(unwritten.next_stop(), Err(BackendError::BadProgram { .. })),
            "a spawn with no child program produced one that dies clean"
        );

        let mut bit = NullBackend::new(NullProgram::single(vec![GuestOp::JumpIfBit {
            addr: GuestAddr(MEMORY_BASE),
            bit: 9,
            pc: 0,
        }]));
        let t = bit.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
        bit.resume(t, Resume::Continue).expect("resume");
        assert!(
            matches!(bit.next_stop(), Err(BackendError::BadProgram { .. })),
            "a bit outside a byte was masked into one"
        );
    }

    /// The two resumptions a scripted guest cannot carry out are refused alike. One of them used
    /// to be swallowed as an ordinary continuation, which answered a question this backend has no
    /// way to answer.
    #[test]
    fn a_resumption_this_backend_cannot_perform_is_refused() {
        let mut b = backend();
        let t = b.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
        for action in [Resume::Rewrite { nr: 1, args: [0; 6] }, Resume::DeliverSignal { signal: 9 }]
        {
            assert!(
                matches!(b.resume(t, action.clone()), Err(BackendError::InvalidResume { .. })),
                "{action:?} was accepted"
            );
        }
    }

    /// The stops a scripted guest could not produce at all, which left the kernel's handling of
    /// each reachable only on Linux under an interceptor.
    #[test]
    fn a_script_can_produce_the_stops_the_boundary_declares() {
        let mut b = NullBackend::new(NullProgram::single(vec![
            GuestOp::ReadTimestamp { rdtscp: true },
            GuestOp::CpuId { leaf: 1, subleaf: 0 },
            GuestOp::RaiseSignal { signal: 11 },
            GuestOp::Exit(0),
        ]));
        let t = b.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");

        let mut seen = Vec::new();
        for _ in 0..3 {
            b.resume(t, Resume::Continue).expect("resume");
            seen.push(b.next_stop().expect("stop").expect("a stop").kind);
        }
        assert!(matches!(seen[0], StopKind::TimestampCounter { rdtscp: true }), "{:?}", seen[0]);
        assert!(matches!(seen[1], StopKind::CpuIdentification { leaf: 1, .. }), "{:?}", seen[1]);
        assert!(matches!(seen[2], StopKind::Signal { signal: 11 }), "{:?}", seen[2]);
    }

    #[test]
    fn memory_writes_are_visible_to_the_guest_and_bounded() {
        let mut b = backend();
        let t = b.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
        b.write_mem(t, GuestAddr(MEMORY_BASE), &[1, 2, 3, 4]).expect("write");

        let mut out = [0u8; 4];
        b.read_mem(t, GuestAddr(MEMORY_BASE), &mut out).expect("read");
        assert_eq!(out, [1, 2, 3, 4]);

        assert!(b.write_mem(t, GuestAddr(0), &[0]).is_err(), "below the mapping");
        let past_end = GuestAddr(MEMORY_BASE + MEMORY_BYTES as u64);
        assert!(b.write_mem(t, past_end, &[0]).is_err(), "past the mapping");
    }

    #[test]
    fn a_spawn_produces_a_task_stopped_before_its_first_operation() {
        let mut b = NullBackend::new(NullProgram {
            main: vec![GuestOp::Spawn, GuestOp::Exit(0)],
            children: vec![vec![GuestOp::Exit(7)]],
        });
        let parent = b.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
        b.resume(parent, Resume::Continue).expect("resume");

        let stop = b.next_stop().expect("stop").expect("a stop");
        let StopKind::TaskCreated { parent: p, .. } = stop.kind else {
            panic!("expected a task to be created, got {:?}", stop.kind);
        };
        assert_eq!(p, parent);

        b.resume(stop.task, Resume::Continue).expect("resume child");
        let child_stop = b.next_stop().expect("stop").expect("a stop");
        assert!(matches!(child_stop.kind, StopKind::TaskExited { status: GuestExit::Exited(7) }));
    }

    #[test]
    fn running_off_the_end_of_a_script_ends_the_task() {
        let mut b = NullBackend::new(NullProgram::single(vec![]));
        let t = b.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
        b.resume(t, Resume::Continue).expect("resume");
        let stop = b.next_stop().expect("stop").expect("a stop");
        assert!(matches!(stop.kind, StopKind::TaskExited { status: GuestExit::Exited(0) }));
    }

    #[test]
    fn nodes_have_address_spaces_of_their_own() {
        let program = || NullProgram::single(vec![GuestOp::Exit(0)]);
        let mut b = NullBackend::cluster(vec![program(), program()]);
        let a = b.spawn(NodeId(0), &GuestSpec::default()).expect("spawn a");
        let c = b.spawn(NodeId(1), &GuestSpec::default()).expect("spawn b");

        b.write_mem(a, GuestAddr(MEMORY_BASE), &[0xAA; 4]).expect("write");
        let mut out = [0u8; 4];
        b.read_mem(c, GuestAddr(MEMORY_BASE), &mut out).expect("read");
        assert_eq!(out, [0; 4], "the same address on another node is different memory");
    }

    #[test]
    fn a_dead_node_restarts_from_its_initial_memory() {
        let mut b = NullBackend::new(NullProgram::single(vec![
            GuestOp::Syscall { nr: 228, args: [0; 6] },
            GuestOp::Exit(0),
        ]));
        b.preload(GuestAddr(MEMORY_BASE), &[7; 4]).expect("preload");
        let t = b.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
        assert!(
            b.spawn(NodeId::ZERO, &GuestSpec::default()).is_err(),
            "a running node cannot be started twice"
        );

        b.write_mem(t, GuestAddr(MEMORY_BASE), &[1; 4]).expect("scribble");
        b.kill(t).expect("kill");

        let again = b.spawn(NodeId::ZERO, &GuestSpec::default()).expect("restart");
        let mut out = [0u8; 4];
        b.read_mem(again, GuestAddr(MEMORY_BASE), &mut out).expect("read");
        assert_eq!(out, [7; 4], "a restart begins from the preloaded image, not the scribbles");
    }

    #[test]
    fn scripted_jumps_branch_on_memory_without_stopping() {
        let mut b = NullBackend::new(NullProgram::single(vec![
            // Written flag falls through to the second syscall; unwritten takes the exit.
            GuestOp::JumpIfMem { addr: GuestAddr(MEMORY_BASE), equals: 1, pc: 2 },
            GuestOp::Exit(3),
            GuestOp::Syscall { nr: 228, args: [0; 6] },
            GuestOp::Exit(0),
        ]));
        b.preload(GuestAddr(MEMORY_BASE), &1u64.to_le_bytes()).expect("preload");
        let t = b.spawn(NodeId::ZERO, &GuestSpec::default()).expect("spawn");
        b.resume(t, Resume::Continue).expect("resume");
        let stop = b.next_stop().expect("stop").expect("a stop");
        assert!(
            matches!(stop.kind, StopKind::SyscallEntry { nr: 228, .. }),
            "the branch was taken without the kernel seeing the jump"
        );
    }
}
