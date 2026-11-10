//! Starting a guest with every host-derived variable already removed.
//!
//! Everything here happens between `fork` and `execve`, or at the exec stop right after, so the
//! guest's first instruction runs in a chosen state.

use std::{
    ffi::{CString, OsStr},
    os::unix::ffi::OsStrExt,
};

use bohr_backend::{BackendError, GuestSpec};
use bohr_types::GuestAddr;

use super::{coverage, syscalls};

/// Auxiliary vector entry naming the VDSO. Zeroing it leaves the loader nothing to map, so calls
/// that would have been answered in userspace become real syscalls and reach the filter.
const AT_SYSINFO_EHDR: u64 = 33;

/// Auxiliary vector entry carrying sixteen random bytes. glibc derives stack canaries and hash
/// seeds from them, so leaving them alone would randomise the guest's own iteration orders.
const AT_RANDOM: u64 = 25;

/// Terminator of the auxiliary vector.
const AT_NULL: u64 = 0;

/// The variable a campaign names a worker's core in; without it, core nought.
pub const CORE_VARIABLE: &str = "BOHR_CORE";

/// The core this supervisor and its guests run on: position `BOHR_CORE` (default 0) among the
/// allowed cores, in the order [`spread`] gives.
///
/// A guest can read its core without a syscall (`RDPID`, the per-CPU segment limit), and neither is
/// trapped; `docs/DETERMINISM.md` lists this.
fn guest_core() -> usize {
    // SAFETY: the set is ours and sized for the call; pid 0 is this thread.
    let allowed: Vec<usize> = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return 0;
        }
        (0..libc::CPU_SETSIZE as usize).filter(|c| libc::CPU_ISSET(*c, &set)).collect()
    };
    let nth = std::env::var(CORE_VARIABLE).ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    // Read once per process: the topology is fixed and a worker spawns thousands of guests.
    static ORDER: std::sync::OnceLock<Vec<usize>> = std::sync::OnceLock::new();
    let order = ORDER.get_or_init(|| {
        spread(&allowed, |cpu| {
            let read = |what: &str| {
                std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{cpu}/topology/{what}"))
                    .ok()
                    .and_then(|text| text.trim().parse::<usize>().ok())
            };
            read("physical_package_id").zip(read("core_id"))
        })
    });
    if order.is_empty() { 0 } else { order[nth % order.len()] }
}

/// The cores in the order workers take them: one hardware thread of every physical core before a
/// second thread of any.
///
/// Linux often numbers a core's two threads next to each other, so ordering by number would put
/// two workers on one core. A CPU whose core cannot be read counts as its own core.
pub(super) fn spread(
    allowed: &[usize],
    core_of: impl Fn(usize) -> Option<(usize, usize)>,
) -> Vec<usize> {
    let mut seen: std::collections::BTreeMap<(usize, usize), usize> =
        std::collections::BTreeMap::new();
    let mut ranked: Vec<(usize, usize)> = allowed
        .iter()
        .map(|&cpu| {
            let rank = core_of(cpu).map_or(0, |core| {
                let n = seen.entry(core).or_default();
                *n += 1;
                *n - 1
            });
            (rank, cpu)
        })
        .collect();
    ranked.sort_unstable();
    ranked.into_iter().map(|(_, cpu)| cpu).collect()
}

#[cfg(test)]
mod spread_tests {
    use super::spread;

    #[test]
    fn every_physical_core_is_taken_before_a_second_thread_of_any() {
        // Six cores, two threads each, with sibling threads numbered next to each other.
        let allowed: Vec<usize> = (0..12).collect();
        let order = spread(&allowed, |cpu| Some((0, cpu / 2)));
        assert_eq!(order, [0, 2, 4, 6, 8, 10, 1, 3, 5, 7, 9, 11]);
        // Sibling threads numbered six apart give the same first six cores.
        let order = spread(&allowed, |cpu| Some((0, cpu % 6)));
        assert_eq!(order[..6], [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_core_that_cannot_be_read_is_a_core_of_its_own() {
        assert_eq!(spread(&[3, 1, 2], |_| None), [1, 2, 3]);
    }
}

/// Namespaces the guest is placed in, so it sees the machine bohr defines instead of the host.
///
/// The pid namespace makes thread numbers reproducible: glibc stores a thread's id in memory and
/// waits on it.
///
/// There is no network namespace. The seccomp filter already sends every network call to the model
/// or refuses it. A network namespace also costs about 180 KB of unreclaimable kernel memory, freed
/// late, and campaigns that start hundreds of guests ran the host out of it.
pub(super) const GUEST_NAMESPACES: libc::c_int = libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNS
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC;

/// Host name the guest sees.
const GUEST_HOSTNAME: &[u8] = b"bohr";

/// Exit codes the child uses when a step of its own setup fails.
///
/// Between `fork` and `execve` the exit code is the only way to report, so each step has its own.
mod failed {
    pub(super) const PERSONALITY: i32 = 101;
    pub(super) const AFFINITY: i32 = 102;
    pub(super) const TRACEME: i32 = 103;
    pub(super) const NO_NEW_PRIVS: i32 = 104;
    pub(super) const SECCOMP: i32 = 105;
    pub(super) const TSC: i32 = 106;
    pub(super) const EXECVE: i32 = 107;
    pub(super) const NAMESPACES: i32 = 108;
    pub(super) const HANDOVER: i32 = 109;
    pub(super) const COVERAGE: i32 = 110;
}

/// What a child exit code means, where it means anything.
pub(super) fn setup_failure(code: i32) -> Option<&'static str> {
    Some(match code {
        failed::PERSONALITY => "address randomisation could not be disabled",
        failed::AFFINITY => "the guest could not be pinned to one core",
        failed::TRACEME => "the guest could not be put under tracing",
        failed::NO_NEW_PRIVS => "privilege escalation could not be sealed off",
        failed::SECCOMP => "the syscall filter could not be installed",
        failed::TSC => "the timestamp counter could not be made to fault",
        failed::EXECVE => "the program could not be executed",
        failed::NAMESPACES => "the guest could not be placed in its own namespaces",
        failed::HANDOVER => "the guest could not be handed over to the supervisor",
        failed::COVERAGE => "the coverage counters could not be placed in the guest",
        _ => return None,
    })
}

/// Starts the guest and returns the pid the supervisor traces.
///
/// The guest is a grandchild: a process enters a new pid namespace only by being created in it, so
/// an intermediate creates the namespace and the guest, then exits. The supervisor attaches to
/// the grandchild, since `PTRACE_TRACEME` would name the intermediate.
pub(super) fn spawn_traced(
    spec: &GuestSpec,
    isolate: bool,
    coverage: Option<libc::c_int>,
    allocations: bool,
) -> Result<libc::pid_t, BackendError> {
    let program = spec.executable.as_ref().or(spec.argv.first()).ok_or_else(|| {
        BackendError::CannotStart {
            program: "<none>".to_owned(),
            reason: "no program given".to_owned(),
        }
    })?;

    let argv = to_c_strings(&spec.argv)?;
    let mut envp: Vec<CString> = spec
        .env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")).unwrap_or_default())
        .collect();
    // The descriptor number of the coverage counters, the same one it is moved to below. The map
    // size stays out of the environment, so it cannot change the run's digest; the guest reads it
    // from the region's header.
    if coverage.is_some() {
        let named = format!("{}={}", coverage::COVERAGE_FD_VAR, coverage::COVERAGE_FD);
        envp.push(CString::new(named).unwrap_or_default());
    }
    let path = CString::new(program.as_bytes()).map_err(|_| BackendError::CannotStart {
        program: program.clone(),
        reason: "program path contains a NUL".to_owned(),
    })?;

    let filter = syscalls::filter_program(allocations).map_err(|e| BackendError::CannotStart {
        program: program.clone(),
        reason: format!("cannot build the syscall filter: {e}"),
    })?;

    // The supervisor runs on the guest's core: each intercepted call is two stops, and on one core
    // they are plain context switches. 200,000 `getpid` calls took 12.5 s on two cores and 1.55 s
    // on one. A refusal is ignored; the run is only slower.
    let core = guest_core();
    // SAFETY: the set is ours and sized for the call; pid 0 is this thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }

    // Two pipes: one carries the guest's pid out, the other tells the guest it is traced. Without
    // the second, the guest could reach its filter before the tracer attaches, and the filter
    // would fail the syscall.
    let mut report = [0; 2];
    let mut go = [0; 2];
    // Close-on-exec, so unused ends do not reach the guest's `/proc/self/fd`. Ends the guest needs
    // clear the flag when they are moved into place.
    // SAFETY: both arrays are ours and large enough for a pipe pair.
    if unsafe { libc::pipe2(report.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(BackendError::CannotStart { program: program.clone(), reason: last_error() });
    }
    // SAFETY: as above; the first pair is closed on every failing path out of here.
    if unsafe { libc::pipe2(go.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        let reason = last_error();
        // SAFETY: descriptors this call opened and nothing else holds.
        unsafe {
            libc::close(report[0]);
            libc::close(report[1]);
        }
        return Err(BackendError::CannotStart { program: program.clone(), reason });
    }

    // The guest's parent exits before the attach. As a subreaper this process adopts the guest,
    // which `ptrace_scope = 1` (Ubuntu's default) requires for attaching.
    // SAFETY: a flag on this process, with no pointer passed.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };

    // SAFETY: fork is async-signal-safe. Neither child returns to the caller: every path ends in
    // execve or _exit.
    let intermediate = unsafe { libc::fork() };
    if intermediate < 0 {
        let reason = last_error();
        // Closed on every failure, so a campaign at the descriptor limit does not leak four a try.
        // SAFETY: descriptors this call opened and nothing else holds.
        unsafe {
            libc::close(report[0]);
            libc::close(report[1]);
            libc::close(go[0]);
            libc::close(go[1]);
        }
        return Err(BackendError::CannotStart { program: program.clone(), reason });
    }
    if intermediate == 0 {
        // SAFETY: the child of a fork, single-threaded by definition, calling only
        // async-signal-safe functions.
        unsafe {
            intermediate_setup(&path, &argv, &envp, &filter, isolate, report[1], go[0], coverage)
        }
    }

    // SAFETY: descriptors this process owns. Closing the guest's ends lets the read below end when
    // the writer exits.
    unsafe {
        libc::close(report[1]);
        libc::close(go[0]);
    }

    let mut buf = [0u8; 4];
    // SAFETY: `buf` is ours and the descriptor is open.
    let read = unsafe { libc::read(report[0], buf.as_mut_ptr().cast(), buf.len()) };
    // SAFETY: descriptors this process owns.
    unsafe { libc::close(report[0]) };
    let mut status = 0;
    // SAFETY: the intermediate is our child; reaping it leaves no zombie behind.
    unsafe { libc::waitpid(intermediate, &mut status, 0) };

    if read != buf.len() as isize {
        // SAFETY: our own descriptor.
        unsafe { libc::close(go[1]) };
        return Err(BackendError::CannotStart {
            program: program.clone(),
            reason: describe_setup_failure(status),
        });
    }
    let guest = libc::pid_t::from_le_bytes(buf);

    let options = trace_options();
    // SAFETY: `guest` is our descendant, so attaching is permitted.
    let seized = unsafe { libc::ptrace(libc::PTRACE_SEIZE, guest, 0, options) };
    // Release the guest to install its filter only after the attach: with no tracer, a tracing
    // filter turns every syscall into ENOSYS.
    // SAFETY: our own descriptor, written once and then closed.
    unsafe {
        libc::write(go[1], b"g".as_ptr().cast(), 1);
        libc::close(go[1]);
    }
    if seized < 0 {
        return Err(BackendError::CannotStart {
            program: program.clone(),
            reason: format!("attaching to the guest: {}", last_error()),
        });
    }
    Ok(guest)
}

/// Trace options every task carries, set when the guest is attached before it runs.
pub(super) const fn trace_options() -> libc::c_int {
    libc::PTRACE_O_TRACESECCOMP
        | libc::PTRACE_O_TRACESYSGOOD
        | libc::PTRACE_O_TRACEEXEC
        | libc::PTRACE_O_TRACECLONE
        | libc::PTRACE_O_TRACEFORK
        | libc::PTRACE_O_TRACEVFORK
        | libc::PTRACE_O_EXITKILL
}

/// What the intermediate's exit status says about why no guest arrived.
fn describe_setup_failure(status: libc::c_int) -> String {
    if libc::WIFEXITED(status) {
        let code = libc::WEXITSTATUS(status);
        if let Some(step) = setup_failure(code) {
            return step.to_owned();
        }
    }
    "the guest was not started".to_owned()
}

/// Creates the namespaces, then the guest inside them, and reports the guest outwards.
///
/// # Safety
///
/// Must only be called in the child of a `fork`. It never returns.
unsafe fn intermediate_setup(
    path: &CString,
    argv: &[CString],
    envp: &[CString],
    filter: &[seccompiler::sock_filter],
    isolate: bool,
    report: libc::c_int,
    go: libc::c_int,
    coverage: Option<libc::c_int>,
) -> ! {
    // SAFETY: every call below is made under this function's own contract (`# Safety` above),
    // which its caller upholds.
    unsafe {
        // Support was checked before the fork, so a failure here stops the launch.
        if isolate && !enter_namespaces() {
            libc::_exit(failed::NAMESPACES);
        }

        // Only the guest is inside the new pid namespace.
        let guest = libc::fork();
        if guest < 0 {
            libc::_exit(failed::HANDOVER);
        }
        if guest == 0 {
            libc::close(report);
            child_setup(path, argv, envp, filter, go, coverage)
        }

        // The outer pid, which the supervisor sees; inside its namespace the guest is pid 1.
        let bytes = guest.to_le_bytes();
        if libc::write(report, bytes.as_ptr().cast(), bytes.len()) != bytes.len() as isize {
            libc::_exit(failed::HANDOVER);
        }
        libc::_exit(0)
    }
}

/// Everything the guest must have arranged before its first instruction.
///
/// # Safety
///
/// Must only be called in the child of a `fork`, where the process is single-threaded and the
/// address space is still shared with nothing.
unsafe fn child_setup(
    path: &CString,
    argv: &[CString],
    envp: &[CString],
    filter: &[seccompiler::sock_filter],
    go: libc::c_int,
    coverage: Option<libc::c_int>,
) -> ! {
    // SAFETY: every call below is made under this function's own contract (`# Safety` above),
    // which its caller upholds.
    unsafe {
        // Moves the counters to the descriptor the environment names. `dup2` clears close-on-exec
        // on the copy, so it survives `execve`; the original keeps it, so other spawns do not
        // inherit it.
        if let Some(fd) = coverage
            && libc::dup2(fd, coverage::COVERAGE_FD) < 0
        {
            libc::_exit(failed::COVERAGE);
        }
        // Address randomisation would place every mapping differently on every run.
        if libc::personality(libc::ADDR_NO_RANDOMIZE as libc::c_ulong) == -1 {
            libc::_exit(failed::PERSONALITY);
        }

        // One core: migration changes timing and what the processor reports. The supervisor pinned
        // itself before the fork, so the first allowed core is the chosen one (core 0 otherwise).
        let mut inherited: libc::cpu_set_t = std::mem::zeroed();
        libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut inherited);
        let core =
            (0..libc::CPU_SETSIZE as usize).find(|c| libc::CPU_ISSET(*c, &inherited)).unwrap_or(0);
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == -1 {
            libc::_exit(failed::AFFINITY);
        }

        // No core file: the trace records the crash, and a campaign of crashing guests would flood
        // the host's crash collector.
        let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &none);

        // Installing a filter requires no-new-privileges, which persists across the exec.
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
            libc::_exit(failed::NO_NEW_PRIVS);
        }
        // Timestamp reads fault, and the supervisor answers them from logical time. Set before the
        // filter, which also filters `prctl`.
        if libc::prctl(libc::PR_SET_TSC, libc::PR_TSC_SIGSEGV, 0, 0, 0) == -1 {
            libc::_exit(failed::TSC);
        }

        // Wait for the tracer. With none attached, `SECCOMP_RET_TRACE` fails the syscall with
        // ENOSYS, which shows up later as a segfault in the loader.
        let mut ready = [0u8; 1];
        if libc::read(go, ready.as_mut_ptr().cast(), 1) != 1 {
            libc::_exit(failed::HANDOVER);
        }
        libc::close(go);

        // Both crates define the kernel's `struct sock_filter`, so the cast is between equal types.
        let prog = libc::sock_fprog {
            len: u16::try_from(filter.len()).unwrap_or(u16::MAX),
            filter: filter.as_ptr().cast::<libc::sock_filter>().cast_mut(),
        };
        if libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            std::ptr::from_ref(&prog),
        ) == -1
        {
            libc::_exit(failed::SECCOMP);
        }

        let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());
        let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
        envp_ptrs.push(std::ptr::null());

        libc::execve(path.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
        libc::_exit(failed::EXECVE)
    }
}

/// Places the caller in fresh namespaces and gives it a fixed view of the machine.
///
/// # Safety
///
/// Must only be called between `fork` and `execve`, where the process is single-threaded.
pub(super) unsafe fn enter_namespaces() -> bool {
    // SAFETY: every call below is made under this function's own contract (`# Safety` above),
    // which its caller upholds.
    unsafe {
        let uid = libc::getuid();
        let gid = libc::getgid();
        if libc::unshare(GUEST_NAMESPACES) == -1 {
            return false;
        }

        // A new user namespace has no identifier mapping. The group map can only be written after
        // `setgroups` is denied.
        let mut line = [0u8; 32];
        if !write_proc(c"/proc/self/setgroups", b"deny")
            || !write_proc(c"/proc/self/uid_map", identity_map(&mut line, uid))
            || !write_proc(c"/proc/self/gid_map", identity_map(&mut line, gid))
        {
            return false;
        }

        libc::sethostname(GUEST_HOSTNAME.as_ptr().cast(), GUEST_HOSTNAME.len()) == 0
            && libc::setdomainname(GUEST_HOSTNAME.as_ptr().cast(), GUEST_HOSTNAME.len()) == 0
    }
}

/// Formats `0 <id> 1`, the one mapping line an unprivileged process is allowed to install.
fn identity_map(buf: &mut [u8; 32], id: libc::uid_t) -> &[u8] {
    let mut digits = [0u8; 10];
    let mut n = id;
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }

    buf[..2].copy_from_slice(b"0 ");
    let end = 2 + (digits.len() - i);
    buf[2..end].copy_from_slice(&digits[i..]);
    buf[end..end + 2].copy_from_slice(b" 1");
    &buf[..end + 2]
}

/// Writes one line to a `procfs` control file, which accepts a whole line or nothing.
///
/// # Safety
///
/// Must only be called between `fork` and `execve`.
unsafe fn write_proc(path: &std::ffi::CStr, line: &[u8]) -> bool {
    // SAFETY: every call below is made under this function's own contract (`# Safety` above),
    // which its caller upholds.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return false;
        }
        let written = libc::write(fd, line.as_ptr().cast(), line.len());
        libc::close(fd);
        written == line.len() as isize
    }
}

/// Rewrites the auxiliary vector on the guest's stack.
///
/// Called at the stop after `execve`, when the stack pointer points at `argc`. Walks past the
/// arguments and environment to the vector and neutralises the two entries that carry host state.
pub(super) fn neutralise_auxv(pid: libc::pid_t, rsp: u64) -> Result<AuxvReport, BackendError> {
    let mut addr = rsp;
    let argc = read_word(pid, addr)?;
    // Checked arithmetic: `argc` comes from guest memory. The kernel bounds it by `ARG_MAX`, but a
    // release build would wrap silently, and the walk runs again at every `execve`.
    let bad = || BackendError::BadAddress { addr: GuestAddr(rsp), len: 8 };
    addr = argc
        .checked_add(1)
        .and_then(|slots| slots.checked_mul(8))
        .and_then(|argv| addr.checked_add(8)?.checked_add(argv))
        .ok_or_else(bad)?; // argv, including its NULL

    // envp, including its NULL.
    loop {
        let word = read_word(pid, addr)?;
        addr = addr.checked_add(8).ok_or_else(bad)?;
        if word == 0 {
            break;
        }
    }

    let mut report = AuxvReport::default();
    loop {
        let key = read_word(pid, addr)?;
        if key == AT_NULL {
            break;
        }
        let value_at = addr + 8;
        match key {
            AT_SYSINFO_EHDR => {
                write_word(pid, value_at, 0)?;
                report.vdso_disabled = true;
            }
            AT_RANDOM => {
                let buf = read_word(pid, value_at)?;
                write_bytes(pid, buf, &[0u8; 16])?;
                report.at_random_zeroed = true;
            }
            _ => {}
        }
        addr += 16;
    }
    Ok(report)
}

/// What the auxiliary vector rewrite did.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct AuxvReport {
    /// Whether a VDSO entry was present and cleared.
    pub vdso_disabled: bool,
    /// Whether the random bytes were present and zeroed.
    pub at_random_zeroed: bool,
}

fn read_word(pid: libc::pid_t, addr: u64) -> Result<u64, BackendError> {
    let mut buf = [0u8; 8];
    super::mem::read(pid, addr, &mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn write_word(pid: libc::pid_t, addr: u64, value: u64) -> Result<(), BackendError> {
    write_bytes(pid, addr, &value.to_le_bytes())
}

fn write_bytes(pid: libc::pid_t, addr: u64, data: &[u8]) -> Result<(), BackendError> {
    super::mem::write(pid, addr, data)
}

fn to_c_strings(items: &[String]) -> Result<Vec<CString>, BackendError> {
    items
        .iter()
        .map(|s| {
            CString::new(OsStr::new(s).as_bytes()).map_err(|_| BackendError::CannotStart {
                program: s.clone(),
                reason: "argument contains a NUL".to_owned(),
            })
        })
        .collect()
}

pub(super) fn last_error() -> String {
    std::io::Error::last_os_error().to_string()
}
