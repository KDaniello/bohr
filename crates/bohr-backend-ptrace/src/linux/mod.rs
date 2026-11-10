//! Syscall-level interception on Linux x86-64.
//!
//! Everything it learns about the guest goes out through the backend boundary; the layers above
//! do not depend on it.

mod coverage;
pub mod doctor;
mod mem;
mod spawn;

pub use spawn::CORE_VARIABLE;
mod syscalls;

use std::collections::BTreeMap;

use bohr_backend::{
    BackendCapabilities, BackendError, CoverageMap, DebuggerTarget, DeterminismBackend, GuestExit,
    GuestSpec, Mapping, ProgressUnit, Regs, Resume, Stop, StopKind,
};
use bohr_types::{GuestAddr, NodeId, TaskId};

/// `arch_prctl` code that makes the processor identification instruction fault. Hosts without the
/// underlying register refuse it, and the run records that.
const ARCH_SET_CPUID: libc::c_int = 0x1012;

/// A ptrace request as the C library declares it: unsigned in glibc, signed in musl.
#[cfg(target_env = "musl")]
type Request = libc::c_int;
#[cfg(not(target_env = "musl"))]
type Request = libc::c_uint;

/// The event a group-stop arrives as under `PTRACE_SEIZE`. Absent from `libc`.
const PTRACE_EVENT_STOP: libc::c_int = 128;

/// The bit an x32 syscall number carries, and the code segment a 64-bit one runs in.
const X32_SYSCALL_BIT: u64 = 0x4000_0000;
const CS_64_BIT: u64 = 0x33;

/// The foreign ABI a syscall arrived through, if any.
///
/// An i386 call runs in the compatibility code segment; an x32 call sets a bit in its number. The
/// syscall table here means neither, so such calls must not be answered as 64-bit ones.
fn foreign_syscall_abi(regs: &libc::user_regs_struct) -> Option<&'static str> {
    if regs.orig_rax & X32_SYSCALL_BIT != 0 {
        return Some("x32");
    }
    (regs.cs != CS_64_BIT).then_some("i386")
}

/// State of one traced task between stops.
#[derive(Debug)]
struct Task {
    pid: libc::pid_t,
    /// Thread group the task belongs to, which is the guest process a node maps onto.
    group: libc::pid_t,
    /// The node this task belongs to, and the program it is running. A hand-over prints both.
    node: NodeId,
    program: String,
    /// Return value owed to a syscall that was skipped, applied at its exit stop.
    pending_return: Option<i64>,
    /// A signal to deliver as the task leaves its emulated call.
    pending_signal: Option<u32>,
    /// Whether the exit of the running syscall is reported to the world as its own stop.
    ///
    /// Set when the world replaced the call: it wants the host's result, which exists only at
    /// exit.
    reporting_exit: bool,
    /// Bytes to step the instruction pointer past on the next resume, for an instruction the
    /// supervisor answered instead of the processor.
    pending_skip: Option<u64>,
    /// Registers as of the current stop, read once per stop. Resuming drops them.
    regs: Option<libc::user_regs_struct>,
    /// Whether the task has been seen stopped at least once. The kernel stops a new `clone`d task
    /// before its first instruction; that stop is not an event.
    attached: bool,
    /// Whether the task is in a job-control stop.
    ///
    /// Such a task waits for a signal to continue it. It is resumed with `PTRACE_LISTEN`, which
    /// keeps it stopped, instead of `PTRACE_CONT`.
    group_stopped: bool,
}

impl Task {
    fn new(pid: libc::pid_t, group: libc::pid_t, node: NodeId, program: String) -> Self {
        Self {
            pid,
            group,
            node,
            program,
            pending_return: None,
            pending_signal: None,
            reporting_exit: false,
            pending_skip: None,
            regs: None,
            attached: false,
            group_stopped: false,
        }
    }
}

/// Executes a real program under `ptrace`, with an in-kernel pre-filter.
///
/// Every task the guest creates is traced and stopped, and only one runs at a time, so the
/// scheduler decides the interleaving. See `docs/ARCHITECTURE.md` §5.4.
#[derive(Debug)]
pub struct PtraceBackend {
    tasks: BTreeMap<TaskId, Task>,
    /// Host pid to task identifier, for what a wait reports.
    by_pid: BTreeMap<libc::pid_t, TaskId>,
    /// The task that was last let run, and so the only one that can be executing.
    current: Option<TaskId>,
    next_task: u32,
    /// Whether this host exposes the register that makes the identification instruction fault.
    traps_cpuid_available: bool,
    /// Whether the running guest's processor identification actually faults.
    ///
    /// Separate from the host's capability: `execve` clears the setting, so the host probe says
    /// nothing about the guest. It stays false until a question is answered on this side.
    ///
    /// Setting it for the guest needs the guest itself to call `arch_prctl`, since no `ptrace`
    /// request sets it for another task. That means injecting the call at the post-exec stop,
    /// which is not done: no available host can trap `CPUID`, so the injection could not be
    /// verified.
    cpuid_trapped: bool,
    isolated: bool,
    /// Whether the guests were released to a debugger. This process then has to end them.
    handed_over: bool,
    notes: Vec<String>,
    /// Where the guest's coverage runtime writes. One channel for all nodes, since a campaign
    /// wants what the run covered as a whole.
    coverage: Option<coverage::CoverageChannel>,
    /// The counters, copied out of that channel on request.
    covered: CoverageMap,
    /// The cores this thread could use before a spawn pinned it beside its guest. Restored on
    /// drop, so workers started later are not all pinned to one core.
    affinity_before: Option<libc::cpu_set_t>,
    /// Whether `brk`, `mremap` and anonymous `mmap` stop, for a world that refuses allocations.
    allocations: bool,
    /// Tasks killed and forgotten, as `(pid, group)`, whose deaths no wait has collected yet.
    dying: Vec<(libc::pid_t, libc::pid_t)>,
    /// Exits already reaped and not yet reported, in the order they are reported.
    exits: std::collections::VecDeque<Stop>,
    /// The tasks those exits belong to: dead on the host, still known until each is reported.
    reaped: std::collections::BTreeSet<TaskId>,
    /// The signal the world ended a thread group with, by group, for the threads the kill takes.
    ended_by: BTreeMap<libc::pid_t, u32>,
    /// Reports collected while a dead group was reaped, for tasks outside it, in arrival order.
    held: std::collections::VecDeque<(libc::pid_t, libc::c_int)>,
}

impl Default for PtraceBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PtraceBackend {
    /// A backend that has probed what this host can do, with a coverage map of the default size.
    #[must_use]
    pub fn new() -> Self {
        Self::with_coverage_bytes(bohr_types::DEFAULT_COVERAGE_MAP_BYTES)
    }

    /// The same, with a coverage map of `bytes` counters.
    ///
    /// Size it from [`bohr_backend::DeterminismBackend::coverage_edges`].
    #[must_use]
    pub fn with_coverage_bytes(bytes: usize) -> Self {
        Self {
            tasks: BTreeMap::new(),
            by_pid: BTreeMap::new(),
            current: None,
            next_task: 0,
            // What the host can do; what the guest got is `cpuid_trapped`.
            traps_cpuid_available: cpuid_faulting_available(),
            cpuid_trapped: false,
            isolated: namespaces_available(),
            handed_over: false,
            notes: Vec::new(),
            coverage: coverage::CoverageChannel::open(bytes),
            covered: CoverageMap::with_bytes(bytes),
            affinity_before: None,
            allocations: false,
            dying: Vec::new(),
            exits: std::collections::VecDeque::new(),
            reaped: std::collections::BTreeSet::new(),
            ended_by: BTreeMap::new(),
            held: std::collections::VecDeque::new(),
        }
    }

    /// The same backend, stopping every allocation call so a world can refuse one.
    #[must_use]
    pub const fn watching_allocations(mut self) -> Self {
        self.allocations = true;
        self
    }

    /// Conditions this host imposed that the run should be recorded as degraded for.
    #[must_use]
    pub fn degradations(&self) -> &[String] {
        &self.notes
    }

    /// Records that a processor question reached this side and was answered.
    ///
    /// Removes the "not trapped" degradation note, which was written before anything was known.
    fn answered_cpu_identification(&mut self) {
        if self.cpuid_trapped {
            return;
        }
        self.cpuid_trapped = true;
        self.notes.retain(|note| note != "cpuid_not_trapped" && note != "cpuid_not_trappable");
    }

    /// Adds a traced task and gives it the identifier everything above uses.
    fn register(
        &mut self,
        pid: libc::pid_t,
        group: libc::pid_t,
        node: NodeId,
        program: String,
    ) -> TaskId {
        let id = TaskId(self.next_task);
        self.next_task += 1;
        self.tasks.insert(id, Task::new(pid, group, node, program));
        self.by_pid.insert(pid, id);
        id
    }

    /// Which node and program a thread group belongs to, for a task that arrived unannounced.
    ///
    /// A thread can report before the event that creates it, so it takes its group's node and
    /// program. Node zero is the fallback.
    fn whose_group(&self, pid: libc::pid_t) -> (NodeId, String) {
        self.tasks
            .values()
            .find(|task| task.group == pid)
            .map_or_else(|| (NodeId::ZERO, String::new()), |task| (task.node, task.program.clone()))
    }

    fn task(&self, id: TaskId) -> Result<&Task, BackendError> {
        self.tasks.get(&id).ok_or(BackendError::NoSuchTask { task: id })
    }

    fn task_mut(&mut self, id: TaskId) -> Result<&mut Task, BackendError> {
        self.tasks.get_mut(&id).ok_or(BackendError::NoSuchTask { task: id })
    }

    /// A fatal signal killed `id`'s whole thread group on the host: every other thread of it is
    /// reaped now, and one exit per thread is queued in task order, each with the group's signal.
    ///
    /// The host reports the threads' deaths in an order of its own, which the trace must not see.
    fn group_died(&mut self, id: TaskId, signal: u32) -> Result<(), BackendError> {
        let group = self.task(id)?.group;
        let signal = self.ended_by.remove(&group).unwrap_or(signal);
        let mut members: Vec<(TaskId, libc::pid_t)> = self
            .tasks
            .iter()
            .filter(|(_, task)| task.group == group)
            .map(|(member, task)| (*member, task.pid))
            .collect();
        // Collected in whatever order they end, with every other report that arrives meanwhile kept
        // for the ordinary wait. A guest is its pid namespace's first process, whose last thread
        // waits in the kernel until every other process there is reaped, a traced child of
        // another group included, so waiting for one thread in particular can wait for ever.
        let mut left: std::collections::BTreeSet<libc::pid_t> =
            members.iter().filter(|(member, _)| *member != id).map(|(_, pid)| *pid).collect();
        // Ten seconds, polled each millisecond: dying threads end at once.
        for _ in 0..10_000 {
            loop {
                let mut status = 0;
                // SAFETY: `status` is a local; any child of ours may report.
                let reported =
                    unsafe { libc::waitpid(-1, &mut status, libc::__WALL | libc::WNOHANG) };
                if reported <= 0 {
                    break;
                }
                let died = libc::WIFEXITED(status) || libc::WIFSIGNALED(status);
                if !(died && left.remove(&reported)) && !left.contains(&reported) {
                    self.held.push_back((reported, status));
                }
            }
            if left.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if !left.is_empty() {
            return Err(BackendError::Lost(format!(
                "{} threads of a process a signal ended did not end",
                left.len()
            )));
        }
        members.sort_by_key(|(member, _)| *member);
        for (member, _) in members {
            self.reaped.insert(member);
            if self.current == Some(member) {
                self.current = None;
            }
            self.exits.push_back(Stop {
                task: member,
                kind: StopKind::TaskExited { status: GuestExit::Signalled(signal) },
            });
        }
        Ok(())
    }

    /// Removes a task that has ended, so the table does not grow with every thread a guest creates.
    fn forget(&mut self, id: TaskId) {
        if let Some(task) = self.tasks.remove(&id) {
            self.by_pid.remove(&task.pid);
        }
        if self.current == Some(id) {
            self.current = None;
        }
    }

    /// Takes over a new task that is already stopped, attaching the options it did not inherit.
    fn settle(&mut self, id: TaskId) -> Result<(), BackendError> {
        let pid = self.task(id)?.pid;
        if set_options(pid).is_err() {
            // The task died before being taken over, with its group. Nothing above knows of it.
            self.forget(id);
            return Ok(());
        }
        self.task_mut(id)?.attached = true;
        Ok(())
    }

    /// Waits for a new task to reach the stop the kernel puts it in, then takes it over.
    ///
    /// A task cannot be resumed before that stop is collected, and the run loop may pick it at
    /// once.
    fn await_first_stop(&mut self, id: TaskId) -> Result<(), BackendError> {
        let pid = self.task(id)?.pid;
        let mut status = 0;
        // SAFETY: `pid` is a task the kernel just created under our tracing; `status` is ours.
        if unsafe { libc::waitpid(pid, &mut status, libc::__WALL) } < 0 {
            return Err(BackendError::Lost(format!(
                "waiting for a new task: {}",
                spawn::last_error()
            )));
        }
        self.settle(id)
    }

    /// Registers of a stopped task, read at most once per stop.
    fn cached_regs(&mut self, id: TaskId) -> Result<libc::user_regs_struct, BackendError> {
        let task = self.task_mut(id)?;
        if let Some(regs) = task.regs {
            return Ok(regs);
        }
        let regs = get_regs(task.pid)?;
        self.task_mut(id)?.regs = Some(regs);
        Ok(regs)
    }

    /// Resumes `task` as `action` says; a ptrace refusal is returned as it came.
    fn resume_now(&mut self, task: TaskId, action: Resume) -> Result<(), BackendError> {
        // Step over an instruction the supervisor answered, whichever way the task resumes.
        if let Some(width) = self.task_mut(task)?.pending_skip.take()
            && !matches!(action, Resume::Terminate { .. })
        {
            let mut regs = self.cached_regs(task)?;
            regs.rip = regs.rip.wrapping_add(width);
            self.store_regs(task, &regs)?;
        }

        match action {
            Resume::Continue | Resume::ContinueWithBudget { .. } => {
                self.release(task, libc::PTRACE_CONT, 0)?;
            }
            Resume::DeliverSignal { signal } => {
                self.release(task, libc::PTRACE_CONT, libc::c_long::from(signal))?;
            }
            // At a seccomp stop, a call numbered -1 is skipped and returns what is in `rax`, so the
            // answer is written here in one stop. A signal with the answer needs the exit stop,
            // because an event stop cannot carry one.
            Resume::Emulate { ret } => {
                let mut regs = self.cached_regs(task)?;
                regs.orig_rax = u64::MAX;
                regs.rax = ret as u64;
                self.store_regs(task, &regs)?;
                self.release(task, libc::PTRACE_CONT, 0)?;
            }
            Resume::EmulateAndSignal { ret, signal } => {
                let mut regs = self.cached_regs(task)?;
                regs.orig_rax = u64::MAX;
                self.store_regs(task, &regs)?;
                let state = self.task_mut(task)?;
                state.pending_return = Some(ret);
                state.pending_signal = Some(signal);
                self.release(task, libc::PTRACE_SYSCALL, 0)?;
            }
            Resume::Rewrite { nr, args } => {
                let mut regs = self.cached_regs(task)?;
                regs.orig_rax = u64::from(nr);
                regs.rdi = args[0];
                regs.rsi = args[1];
                regs.rdx = args[2];
                regs.r10 = args[3];
                regs.r8 = args[4];
                regs.r9 = args[5];
                self.store_regs(task, &regs)?;
                self.task_mut(task)?.reporting_exit = true;
                self.release(task, libc::PTRACE_SYSCALL, 0)?;
            }
            Resume::Terminate { signal } => {
                // The other threads die of the kill; they are reported dying of what ended this
                // one.
                let group = self.task(task)?.group;
                let others = self.tasks.iter().any(|(t, other)| *t != task && other.group == group);
                if let (Some(signal), true) = (signal, others) {
                    self.ended_by.insert(group, signal);
                }
                self.kill(task)?;
            }
        }
        Ok(())
    }

    /// Writes registers and keeps the cache in step with what the task now holds.
    fn store_regs(
        &mut self,
        id: TaskId,
        regs: &libc::user_regs_struct,
    ) -> Result<(), BackendError> {
        let task = self.task_mut(id)?;
        set_regs(task.pid, regs)?;
        task.regs = Some(*regs);
        Ok(())
    }

    /// Resumes the task; nothing read at the stop holds afterwards.
    ///
    /// A failure means the task is gone. It is returned as an error so the loop does not wait for
    /// a stop that will never come.
    fn release(
        &mut self,
        id: TaskId,
        request: Request,
        signal: libc::c_long,
    ) -> Result<(), BackendError> {
        let task = self.task_mut(id)?;
        task.regs = None;
        let pid = task.pid;
        // A job-control stop stays stopped: the guest asked for it.
        let request = if std::mem::take(&mut task.group_stopped) {
            libc::PTRACE_LISTEN as Request
        } else {
            request
        };
        // SAFETY: the task is stopped and owned by us; the request passes no pointer.
        let rc = unsafe { libc::ptrace(request, pid, 0, signal) };
        if rc < 0 {
            return Err(BackendError::Lost(format!("resuming {id}: {}", spawn::last_error())));
        }
        self.current = Some(id);
        Ok(())
    }
}

impl DeterminismBackend for PtraceBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            name: "ptrace",
            traps_rdtsc: true,
            traps_cpuid: self.cpuid_trapped,
            // The hardware random instructions cannot be trapped. A scan finds them, and a run
            // that keeps one is reported degraded.
            traps_rdrand: false,
            progress_unit: ProgressUnit::Syscalls,
            hands_over_to_debugger: true,
            max_tasks: 4096,
        }
    }

    fn spawn(&mut self, node: NodeId, spec: &GuestSpec) -> Result<TaskId, BackendError> {
        if self.affinity_before.is_none() {
            // SAFETY: the set is ours and sized for the call; pid 0 is this thread.
            self.affinity_before = unsafe {
                let mut set: libc::cpu_set_t = std::mem::zeroed();
                (libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0)
                    .then_some(set)
            };
        }
        let pid = spawn::spawn_traced(
            spec,
            self.isolated,
            self.coverage.as_ref().map(|c| c.fd()),
            self.allocations,
        )?;

        // Run to the exec, where the stack still carries the auxiliary vector. The guest's own
        // `execve` raises a seccomp event first, which is continued here, below the boundary.
        let mut status = 0;
        loop {
            // SAFETY: `pid` is the guest we attached to and `status` is a local we own.
            if unsafe { libc::waitpid(pid, &mut status, libc::__WALL) } < 0 {
                return Err(BackendError::Lost(format!(
                    "waiting for exec: {}",
                    spawn::last_error()
                )));
            }
            if is_event(status, libc::PTRACE_EVENT_EXEC) {
                break;
            }
            if !libc::WIFSTOPPED(status) {
                return Err(BackendError::CannotStart {
                    program: spec.argv.first().cloned().unwrap_or_default(),
                    reason: describe_status(status),
                });
            }
            // SAFETY: the task is stopped and owned by us.
            if unsafe { libc::ptrace(libc::PTRACE_CONT, pid, 0, 0) } < 0 {
                return Err(BackendError::CannotStart {
                    program: spec.argv.first().cloned().unwrap_or_default(),
                    reason: format!("resuming the guest: {}", spawn::last_error()),
                });
            }
        }

        // The guest reads the name it was started by in `AT_EXECFN`, not the path this host
        // executed it from.
        let mut regs = get_regs(pid)?;
        let label = spawn::execfn_label(spec.argv.first().map_or("", String::as_str));
        let (rsp, report) = spawn::canonical_stack(pid, regs.rsp, Some(&label))?;
        regs.rsp = rsp;
        set_regs(pid, &regs)?;
        if !report.vdso_disabled {
            self.notes.push("vdso_absent_from_auxv".to_owned());
        }
        if !report.at_random_zeroed {
            self.notes.push("at_random_absent_from_auxv".to_owned());
        }
        if !self.cpuid_trapped {
            // This guest was not trapped. The note names whether the host could have done it.
            self.notes.push(if self.traps_cpuid_available {
                "cpuid_not_trapped".to_owned()
            } else {
                "cpuid_not_trappable".to_owned()
            });
        }
        if !self.isolated {
            self.notes.push("namespaces_unavailable".to_owned());
        }

        let id = self.register(pid, pid, node, spec.argv.first().cloned().unwrap_or_default());
        self.task_mut(id)?.attached = true;
        Ok(id)
    }

    fn next_stop(&mut self) -> Result<Option<Stop>, BackendError> {
        loop {
            if let Some(exit) = self.exits.pop_front() {
                self.reaped.remove(&exit.task);
                self.forget(exit.task);
                return Ok(Some(exit));
            }
            if self.current.is_none() {
                return Ok(None);
            }

            let mut status = 0;
            // Any traced task may report: a new `clone`d thread stops before its first
            // instruction. `__WALL` includes threads, which the default wait skips. A report held
            // while a dead group was reaped comes first.
            let pid = match self.held.pop_front() {
                Some((pid, held)) => {
                    status = held;
                    pid
                }
                // SAFETY: `status` is a local we own.
                None => unsafe { libc::waitpid(-1, &mut status, libc::__WALL) },
            };
            if pid < 0 {
                return Err(BackendError::Lost(format!(
                    "waiting for any task: {}",
                    spawn::last_error()
                )));
            }

            // An unknown pid that stopped is a new task that arrived before its creation event.
            // It is adopted and left stopped; the event announces it later.
            //
            // An unknown pid that died is a task already forgotten at its kill, and is dropped.
            let Some(id) = self.by_pid.get(&pid).copied() else {
                if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                    self.dying.retain(|&(dead, _)| dead != pid);
                    continue;
                }
                let (node, program) = self.whose_group(pid);
                let id = self.register(pid, pid, node, program);
                self.settle(id)?;
                continue;
            };

            if libc::WIFEXITED(status) {
                self.forget(id);
                return Ok(Some(Stop {
                    task: id,
                    kind: StopKind::TaskExited {
                        status: GuestExit::Exited(libc::WEXITSTATUS(status)),
                    },
                }));
            }
            if libc::WIFSIGNALED(status) {
                self.group_died(id, libc::WTERMSIG(status) as u32)?;
                continue;
            }
            if !libc::WIFSTOPPED(status) {
                return Err(BackendError::Lost(describe_status(status)));
            }

            // Registers read at the previous stop are stale.
            self.task_mut(id)?.regs = None;

            // An adopted task stopped again before its creation event; it waits for that event.
            if !self.task(id)?.attached {
                self.settle(id)?;
                continue;
            }

            // A creation event announces the new task. Parent and child both stay stopped until
            // the run loop picks one.
            if is_event(status, libc::PTRACE_EVENT_CLONE)
                || is_event(status, libc::PTRACE_EVENT_FORK)
                || is_event(status, libc::PTRACE_EVENT_VFORK)
            {
                let shares_memory = is_event(status, libc::PTRACE_EVENT_CLONE);
                let child_pid = i32::try_from(event_message(pid)?).unwrap_or(-1);
                // The kernel's own answer: a `clone` without `CLONE_THREAD` is reported as a clone
                // event too, and is a process of its own.
                let group = thread_group_of(child_pid).unwrap_or(if shares_memory {
                    self.task(id)?.group
                } else {
                    child_pid
                });
                let thread = group == self.task(id)?.group;

                let parent = self.task(id)?;
                let (node, program) = (parent.node, parent.program.clone());
                let child = match self.by_pid.get(&child_pid).copied() {
                    // Adopted earlier with a guessed group and node; the event has the real ones.
                    Some(child) => {
                        let task = self.task_mut(child)?;
                        task.group = group;
                        task.node = node;
                        task.program = program;
                        child
                    }
                    None => self.register(child_pid, group, node, program),
                };
                if !self.task(child)?.attached {
                    self.await_first_stop(child)?;
                }

                self.current = None;
                return Ok(Some(Stop {
                    task: child,
                    kind: StopKind::TaskCreated { parent: id, shares_memory, thread },
                }));
            }

            // A guest that calls `execve` gets a fresh auxiliary vector with the vDSO and a new
            // `AT_RANDOM`, so both are neutralised again, as at spawn.
            //
            // The exec is not reported above the boundary: the task keeps its identity, and the
            // filter admits `execve` without asking the world.
            if is_event(status, libc::PTRACE_EVENT_EXEC) {
                let mut regs = get_regs(pid)?;
                let (rsp, report) = spawn::canonical_stack(pid, regs.rsp, None)?;
                regs.rsp = rsp;
                set_regs(pid, &regs)?;
                if !report.vdso_disabled {
                    self.notes.push("vdso_absent_from_auxv".to_owned());
                }
                if !report.at_random_zeroed {
                    self.notes.push("at_random_absent_from_auxv".to_owned());
                }
                self.release(id, libc::PTRACE_CONT, 0)?;
                continue;
            }

            if is_event(status, libc::PTRACE_EVENT_SECCOMP) {
                let regs = self.cached_regs(id)?;
                // The number and registers mean what they say only for the x86-64 ABI. An i386
                // `write` (4) would otherwise reach the world as x86-64 `stat` (4) with arguments
                // from the wrong registers.
                if let Some(abi) = foreign_syscall_abi(&regs) {
                    return Err(BackendError::Lost(format!(
                        "a syscall arrived through the {abi} ABI, which this build does not read"
                    )));
                }
                self.current = None;
                return Ok(Some(Stop {
                    task: id,
                    kind: StopKind::SyscallEntry {
                        nr: regs.orig_rax as u32,
                        args: [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9],
                    },
                }));
            }

            // A job-control stop arrives as an event under `PTRACE_SEIZE`. It is marked so the
            // task is resumed with `PTRACE_LISTEN` and stays stopped.
            if is_event(status, PTRACE_EVENT_STOP) {
                self.task_mut(id)?.group_stopped = true;
                self.current = None;
                let kind = StopKind::Signal { signal: libc::WSTOPSIG(status) as u32 };
                return Ok(Some(Stop { task: id, kind }));
            }

            let sig = libc::WSTOPSIG(status);

            // The exit of a skipped syscall. The world's return value is written here, because the
            // kernel overwrites `rax` on every syscall exit.
            if sig == libc::SIGTRAP | 0x80 {
                if let Some(ret) = self.task_mut(id)?.pending_return.take() {
                    let mut regs = self.cached_regs(id)?;
                    regs.rax = ret as u64;
                    self.store_regs(id, &regs)?;
                }
                // A replaced call's result goes up to the world before the guest runs again.
                if std::mem::take(&mut self.task_mut(id)?.reporting_exit) {
                    let ret = self.cached_regs(id)?.rax as i64;
                    self.current = None;
                    return Ok(Some(Stop { task: id, kind: StopKind::SyscallExit { ret } }));
                }
                // A signal from the world is delivered on the return, after the answer is in place.
                let signal = self.task_mut(id)?.pending_signal.take().unwrap_or(0);
                self.release(id, libc::PTRACE_CONT, libc::c_long::from(signal))?;
                continue;
            }

            if sig == libc::SIGSEGV {
                let rip = self.cached_regs(id)?.rip;
                if let Some((leaf, subleaf, width)) = decode_cpu_identification(pid, rip) {
                    // The instruction was rewritten before the run to fault; it is answered now.
                    self.answered_cpu_identification();
                    self.task_mut(id)?.pending_skip = Some(width);
                    self.current = None;
                    let kind = StopKind::CpuIdentification { leaf, subleaf };
                    return Ok(Some(Stop { task: id, kind }));
                }
                if let Some((rdtscp, width)) = decode_timestamp_read(pid, rip) {
                    self.task_mut(id)?.pending_skip = Some(width);
                    self.current = None;
                    return Ok(Some(Stop {
                        task: id,
                        kind: StopKind::TimestampCounter { rdtscp },
                    }));
                }
            }

            self.current = None;
            return Ok(Some(Stop { task: id, kind: StopKind::Signal { signal: sig as u32 } }));
        }
    }

    fn resume(&mut self, task: TaskId, action: Resume) -> Result<(), BackendError> {
        // Dead with its group; its exit is already queued.
        if self.reaped.contains(&task) {
            return Ok(());
        }
        match self.resume_now(task, action) {
            // A fatal signal to its thread group took the task out of its stop: its death is
            // waited for like any other, with the group's.
            Err(lost) => match self.task(task).map(|t| t.pid) {
                Ok(pid) if left_its_stop(pid) => {
                    self.task_mut(task)?.regs = None;
                    self.current = Some(task);
                    Ok(())
                }
                _ => Err(lost),
            },
            done => done,
        }
    }

    fn read_mem(
        &mut self,
        task: TaskId,
        addr: GuestAddr,
        out: &mut [u8],
    ) -> Result<(), BackendError> {
        let pid = self.task(task)?.pid;
        mem::read(pid, addr.0, out)
    }

    fn read_mem_vectored(
        &mut self,
        task: TaskId,
        pieces: &[(GuestAddr, usize)],
        out: &mut [u8],
    ) -> Result<(), BackendError> {
        let pid = self.task(task)?.pid;
        mem::read_pieces(pid, pieces, out)
    }

    fn write_mem(
        &mut self,
        task: TaskId,
        addr: GuestAddr,
        data: &[u8],
    ) -> Result<(), BackendError> {
        let pid = self.task(task)?.pid;
        mem::write(pid, addr.0, data)
    }

    fn regs(&mut self, task: TaskId) -> Result<Regs, BackendError> {
        let r = self.cached_regs(task)?;
        Ok(Regs {
            rip: r.rip,
            rax: r.rax,
            args: [r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9],
            rdx: r.rdx,
            rcx: r.rcx,
            rbx: r.rbx,
        })
    }

    fn set_regs(&mut self, task: TaskId, regs: &Regs) -> Result<(), BackendError> {
        let mut r = self.cached_regs(task)?;
        r.rip = regs.rip;
        r.rax = regs.rax;
        r.rcx = regs.rcx;
        r.rbx = regs.rbx;
        // Arguments are written back too. `rdx` comes from the arguments, so its two copies agree.
        r.rdi = regs.args[0];
        r.rsi = regs.args[1];
        r.rdx = regs.args[2];
        r.r10 = regs.args[3];
        r.r8 = regs.args[4];
        r.r9 = regs.args[5];
        self.store_regs(task, &r)
    }

    fn mappings(&mut self, task: TaskId) -> Result<Vec<Mapping>, BackendError> {
        let pid = self.task(task)?.pid;
        let text = std::fs::read_to_string(format!("/proc/{pid}/maps"))
            .map_err(|e| BackendError::Lost(format!("reading mappings: {e}")))?;
        Ok(text.lines().filter_map(parse_mapping).collect())
    }

    fn kill(&mut self, task: TaskId) -> Result<(), BackendError> {
        // Already dead and reaped, so its pid may be another process's: nothing is signalled, and
        // the exit the caller now does not expect is withdrawn.
        if self.reaped.remove(&task) {
            self.exits.retain(|exit| exit.task != task);
            self.forget(task);
            return Ok(());
        }
        let (pid, group) = {
            let t = self.task(task)?;
            (t.pid, t.group)
        };
        // `tgkill` names the thread group and the thread; `kill` would read a thread id as a pid.
        //
        // SAFETY: both identifiers belong to a task we started.
        unsafe {
            libc::syscall(libc::SYS_tgkill, group, pid, libc::SIGKILL);
        }
        // No wait here: the other threads of the group are stopped for this supervisor, and the
        // leader is reaped only after them. The death arrives at the ordinary wait and is dropped.
        self.dying.push((pid, group));
        self.forget(task);
        Ok(())
    }

    fn hand_over(&mut self) -> Result<Vec<DebuggerTarget>, BackendError> {
        // One target per thread group: a debugger attaches to a process and finds its threads. A
        // node that forked has several groups, and each line names its node.
        let mut groups: BTreeMap<libc::pid_t, (NodeId, String)> = BTreeMap::new();
        for task in self.tasks.values() {
            groups.entry(task.group).or_insert((task.node, task.program.clone()));
        }

        for task in self.tasks.values() {
            // Cleared before any detach, so guests already released survive this process exiting
            // on an error.
            //
            // SAFETY: the task is stopped and owned by us; the request passes no pointer.
            unsafe {
                libc::ptrace(
                    libc::PTRACE_SETOPTIONS,
                    task.pid,
                    0,
                    spawn::trace_options() & !libc::PTRACE_O_EXITKILL,
                );
            }
        }

        for task in self.tasks.values() {
            // `PTRACE_DETACH` restarts the tracee; a `SIGSTOP` delivered with the detach keeps it
            // stopped for the debugger.
            // https://man7.org/linux/man-pages/man2/ptrace.2.html
            //
            // SAFETY: both identifiers belong to a task we started, and the task is stopped.
            let released = unsafe {
                libc::syscall(libc::SYS_tgkill, task.group, task.pid, libc::SIGSTOP);
                libc::ptrace(libc::PTRACE_DETACH, task.pid, 0, libc::SIGSTOP)
            };
            if released < 0 {
                return Err(BackendError::Lost(format!(
                    "releasing a guest to a debugger: {}",
                    spawn::last_error()
                )));
            }
        }
        self.handed_over = true;

        Ok(groups
            .into_iter()
            .map(|(pid, (node, program))| {
                DebuggerTarget::new(
                    node,
                    format!("gdb -p {pid} {program}").trim().to_owned(),
                    limits(),
                )
            })
            .collect())
    }

    fn coverage(&mut self) -> Option<&CoverageMap> {
        let channel = self.coverage.as_ref()?;
        channel.read_into(self.covered.as_bytes_mut());
        Some(&self.covered)
    }

    fn coverage_edges(&self) -> u64 {
        self.coverage.as_ref().map_or(0, coverage::CoverageChannel::guards)
    }
}

/// What a session with a released guest cannot do, printed with every target.
///
/// The guest's seccomp filter traps every syscall, and a debugger does not handle those traps, so
/// each syscall returns as if it did not exist.
fn limits() -> Vec<String> {
    vec![
        "the guest cannot be continued: every syscall it attempts returns as though the call did \
         not exist, because the filter it was started under expects a supervisor that is no \
         longer there"
            .to_owned(),
        "reading the clock faults, and so does asking the processor to identify itself: both were \
         set to trap and stay that way after the hand-over"
            .to_owned(),
        "the world it was running in — its files, its sockets, its peers — was this process's \
         bookkeeping and is gone; a debugger sees a process with almost no operating system state"
            .to_owned(),
        "determinism ends here: nothing done from now on belongs to the run that was reproduced"
            .to_owned(),
    ]
}

/// Kills every task of the run, and reaps it unless it was released to a debugger.
///
/// `PTRACE_O_EXITKILL` acts only when this process exits, which is too late for a command that
/// makes many runs.
impl Drop for PtraceBackend {
    fn drop(&mut self) {
        if let Some(set) = self.affinity_before.take() {
            // SAFETY: a set this thread reported for itself; pid 0 is this thread.
            unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) };
        }
        for task in self.tasks.values() {
            // SAFETY: both identifiers belong to a task we started; a dead one refuses the signal.
            unsafe { libc::syscall(libc::SYS_tgkill, task.group, task.pid, libc::SIGKILL) };
        }
        if self.handed_over {
            return;
        }
        let known = self.tasks.values().map(|task| (task.pid, task.group));
        let left = reap(known.chain(self.dying.iter().copied()));
        if left > 0 {
            eprintln!(
                "bohr: {left} tasks of a finished run did not end within a second, left behind"
            );
        }
    }
}

/// Kills and reaps `tasks` and everything they started, never blocking; returns how many are left.
///
/// Every wait polls. An isolated guest is the init of its pid namespace, and an exiting init waits
/// for this caller to reap its traced threads, so a blocking wait would deadlock.
fn reap(tasks: impl Iterator<Item = (libc::pid_t, libc::pid_t)>) -> usize {
    // The listed tasks, their threads and their children, transitively: a thread or fork created
    // after the last wait is in no table.
    let mut left = std::collections::BTreeSet::<libc::pid_t>::new();
    let mut queue: Vec<libc::pid_t> = tasks.flat_map(|(pid, group)| [pid, group]).collect();
    while let Some(pid) = queue.pop() {
        if !left.insert(pid) {
            continue;
        }
        let Ok(threads) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
        for thread in threads.flatten() {
            let Some(tid) = thread.file_name().to_str().and_then(|s| s.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            queue.push(tid);
            if let Ok(children) = std::fs::read_to_string(thread.path().join("children")) {
                queue.extend(
                    children.split_whitespace().filter_map(|c| c.parse::<libc::pid_t>().ok()),
                );
            }
        }
    }
    for &pid in &left {
        // SAFETY: a task of this run or one it started; one already gone refuses the signal.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    // A thousand polls a millisecond apart, about a second. Counted, because the backend never
    // reads the host clock.
    for _ in 0..1000 {
        if left.is_empty() {
            break;
        }
        left.retain(|&pid| {
            let mut status = 0;
            // SAFETY: a task of this run; `status` is a local we own.
            let rc = unsafe { libc::waitpid(pid, &raw mut status, libc::__WALL | libc::WNOHANG) };
            if rc == 0 {
                return true;
            }
            if rc > 0 && libc::WIFSTOPPED(status) {
                // SAFETY: the task is ours and stopped; continued with SIGKILL pending, it dies.
                unsafe { libc::ptrace(libc::PTRACE_CONT, pid, 0, 0) };
                return true;
            }
            false
        });
        if !left.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    left.len()
}

/// Sets the trace options every task carries. A `clone`d task inherits none, so they are set at
/// its first stop.
fn set_options(pid: libc::pid_t) -> Result<(), BackendError> {
    // SAFETY: the task is stopped, which is when options may be set.
    let rc = unsafe { libc::ptrace(libc::PTRACE_SETOPTIONS, pid, 0, spawn::trace_options()) };
    if rc < 0 {
        return Err(BackendError::Lost(format!("setting trace options: {}", spawn::last_error())));
    }
    Ok(())
}

/// The value a `PTRACE_EVENT_*` stop carries, which for a creation event is the new host pid.
fn event_message(pid: libc::pid_t) -> Result<libc::c_ulong, BackendError> {
    let mut message: libc::c_ulong = 0;
    // SAFETY: the task is stopped at an event; `message` is a local we own.
    let rc =
        unsafe { libc::ptrace(libc::PTRACE_GETEVENTMSG, pid, 0, std::ptr::from_mut(&mut message)) };
    if rc < 0 {
        return Err(BackendError::Lost(format!("reading an event: {}", spawn::last_error())));
    }
    Ok(message)
}

/// Whether the instruction at the fault address is a processor identification, with its leaves.
///
/// A trapped `CPUID` arrives as `SIGSEGV`, like a trapped timestamp read; the bytes at the fault
/// address tell them apart.
fn decode_cpu_identification(pid: libc::pid_t, rip: u64) -> Option<(u32, u32, u64)> {
    let mut code = [0u8; 2];
    if mem::read(pid, rip, &mut code).is_err() {
        return None;
    }
    // Either `CPUID` itself, where the processor faults on it, or the trap byte it was rewritten
    // to. Matched by bytes, because the kernel maps the program and its offsets are unknown here.
    if code != [0x0F, 0xA2] && code[0] != bohr_x86::cpuid::TRAP[0] {
        return None;
    }
    // The leaf is in `eax`, the sub-leaf in `ecx`.
    let regs = get_regs(pid).ok()?;
    Some((regs.rax as u32, regs.rcx as u32, 2))
}

/// Whether the instruction at the fault address is a timestamp counter read.
///
/// `PR_SET_TSC` makes those reads fault with `SIGSEGV`; any other `SIGSEGV` belongs to the guest.
fn decode_timestamp_read(pid: libc::pid_t, rip: u64) -> Option<(bool, u64)> {
    let mut code = [0u8; 3];
    if mem::read(pid, rip, &mut code).is_err() {
        return None;
    }
    match code {
        [0x0F, 0x31, _] => Some((false, 2)),
        [0x0F, 0x01, 0xF9] => Some((true, 3)),
        _ => None,
    }
}

/// Whether this host lets an unprivileged process create the namespaces a guest is confined to.
///
/// Probed once per process: creating a network namespace is expensive.
fn namespaces_available() -> bool {
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| {
        // The whole launch sequence: `unshare` can succeed while writing the identifier maps fails.
        probe_in_child(|| {
            // SAFETY: between `fork` and `_exit` in a child that performs nothing else, which is
            // the state `enter_namespaces` requires.
            unsafe { spawn::enter_namespaces() }
        })
    })
}

/// Whether this host can make the identification instruction fault.
///
/// Probed in a child, because the setting is per task and would otherwise apply to the supervisor.
fn cpuid_faulting_available() -> bool {
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| {
        probe_in_child(|| {
            // SAFETY: the probe only sets the flag, in a child that exits immediately.
            unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_CPUID, 0) == 0 }
        })
    })
}

/// Runs a host capability probe in a throwaway child and reports whether it succeeded.
///
/// The supervisor never carries the state the probe changes.
fn probe_in_child(probe: impl FnOnce() -> bool) -> bool {
    // SAFETY: fork is async-signal-safe; the child performs the probe and never returns.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return false;
        }
        if pid == 0 {
            libc::_exit(i32::from(!probe()));
        }
        let mut status = 0;
        if libc::waitpid(pid, &mut status, 0) < 0 {
            return false;
        }
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
    }
}

/// The thread group a task belongs to, as `/proc` names it.
fn thread_group_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|line| line.strip_prefix("Tgid:")?.trim().parse().ok())
}

/// Whether a task the supervisor held stopped has left the stop without being resumed: killed with
/// its thread group, its death not yet collected.
fn left_its_stop(pid: libc::pid_t) -> bool {
    // SAFETY: the struct is plain data; `regs` is ours and large enough.
    let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    // SAFETY: as above; a task not in a stop answers `ESRCH` and is not touched.
    let rc = unsafe { libc::ptrace(libc::PTRACE_GETREGS, pid, 0, std::ptr::from_mut(&mut regs)) };
    rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn get_regs(pid: libc::pid_t) -> Result<libc::user_regs_struct, BackendError> {
    // SAFETY: the struct is plain data and is fully written by the kernel on success.
    let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    // SAFETY: the task is stopped; `regs` is ours and large enough.
    let rc = unsafe { libc::ptrace(libc::PTRACE_GETREGS, pid, 0, std::ptr::from_mut(&mut regs)) };
    if rc < 0 {
        return Err(BackendError::Lost(format!("reading registers: {}", spawn::last_error())));
    }
    Ok(regs)
}

fn set_regs(pid: libc::pid_t, regs: &libc::user_regs_struct) -> Result<(), BackendError> {
    // SAFETY: the task is stopped; `regs` is ours and is only read by the kernel.
    let rc = unsafe { libc::ptrace(libc::PTRACE_SETREGS, pid, 0, std::ptr::from_ref(regs)) };
    if rc < 0 {
        return Err(BackendError::Lost(format!("writing registers: {}", spawn::last_error())));
    }
    Ok(())
}

fn is_event(status: libc::c_int, event: libc::c_int) -> bool {
    libc::WIFSTOPPED(status)
        && libc::WSTOPSIG(status) == libc::SIGTRAP
        && (status >> 16) & 0xff == event
}

fn describe_status(status: libc::c_int) -> String {
    if libc::WIFEXITED(status) {
        let code = libc::WEXITSTATUS(status);
        if let Some(step) = spawn::setup_failure(code) {
            return step.to_owned();
        }
        return format!("the guest exited with {code} before it could be traced");
    }
    format!("unexpected wait status {status:#x}")
}

fn parse_mapping(line: &str) -> Option<Mapping> {
    let mut parts = line.split_whitespace();
    let range = parts.next()?;
    let perms = parts.next()?;
    let (start, end) = range.split_once('-')?;
    Some(Mapping {
        start: GuestAddr(u64::from_str_radix(start, 16).ok()?),
        end: GuestAddr(u64::from_str_radix(end, 16).ok()?),
        executable: perms.contains('x'),
        writable: perms.contains('w'),
        path: parts.nth(3).map(str::to_owned),
    })
}

/// Every syscall this build handles, for a diagnosis to compare a target against.
pub fn syscall_table() -> impl Iterator<Item = (u32, &'static str)> {
    syscalls::TABLE.iter().map(|e| (e.nr as u32, e.name))
}

/// The name of a syscall, where this build knows one.
pub fn syscall_name(nr: u32) -> Option<&'static str> {
    syscalls::name_of(nr)
}

/// The name of a syscall this build does not model, where one is known.
///
/// Used only to label a survey's report.
pub fn unmodelled_syscall_name(nr: u32) -> Option<&'static str> {
    syscalls::name_of_unmodelled(nr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The namespace probe and a launch agree on this host.
    ///
    /// A launch denies `setgroups`, writes both identifier maps and sets the host name; the probe
    /// must perform the same steps. The test can only fail on a host where one step is refused.
    #[test]
    fn the_namespace_probe_promises_only_what_a_launch_performs() {
        let promised = namespaces_available();
        // SAFETY: between `fork` and `_exit` in a child that performs nothing else.
        let performed = probe_in_child(|| unsafe { spawn::enter_namespaces() });
        assert_eq!(
            promised, performed,
            "the probe and the launch disagree about this host, so one of them is wrong"
        );
    }
}
