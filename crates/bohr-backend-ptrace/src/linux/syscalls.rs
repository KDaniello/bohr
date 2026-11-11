//! The syscall table, and the seccomp filter generated from it.
//!
//! One table drives both the in-kernel filter and the supervisor's dispatch, so the two cannot
//! disagree about a syscall.

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use super::coverage::COVERAGE_FD;

/// `mmap`, the one call whose disposition depends on its arguments.
const MMAP: i64 = 9;

/// `MAP_ANONYMOUS`: the mapping is backed by nothing, so it cannot see a file.
const MAP_ANONYMOUS: u64 = 0x20;

/// The `mmap` calls that go straight to the host.
///
/// A file mapping does not: guest descriptor numbers are the world's, so a host `mmap` of one could
/// map a descriptor the supervisor holds, such as the coverage counters.
///
/// Two mappings are admitted. An anonymous one, which every allocator makes; the test is on the
/// flag, since the descriptor is ignored once it is set. And a mapping of the coverage channel,
/// which the supervisor hands the guest on purpose.
fn mapping_rules() -> Result<Vec<SeccompRule>, String> {
    let anonymous = SeccompCondition::new(
        3,
        SeccompCmpArgLen::Qword,
        SeccompCmpOp::MaskedEq(MAP_ANONYMOUS),
        MAP_ANONYMOUS,
    )
    .map_err(|e| e.to_string())?;
    let ours =
        SeccompCondition::new(4, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, COVERAGE_FD as u64)
            .map_err(|e| e.to_string())?;

    // Two rules, because rules are alternatives; conditions in one rule must all hold.
    Ok(vec![
        SeccompRule::new(vec![anonymous]).map_err(|e| e.to_string())?,
        SeccompRule::new(vec![ours]).map_err(|e| e.to_string())?,
    ])
}

/// `madvise`, admitted for every advice but one.
const MADVISE: i64 = 28;

/// `MADV_FREE`: a freed page keeps or loses its bytes by the host's memory pressure, so it stops
/// the guest and is carried out as `MADV_DONTNEED`.
const MADV_FREE: u64 = 8;

/// The rule admitting `madvise` with any advice but `MADV_FREE`.
fn advice_rules() -> Result<Vec<SeccompRule>, String> {
    let not_free = SeccompCondition::new(2, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, MADV_FREE)
        .map_err(|e| e.to_string())?;
    Ok(vec![SeccompRule::new(vec![not_free]).map_err(|e| e.to_string())?])
}

/// What the filter does about a syscall.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Disposition {
    /// Stops the guest so the world can answer. Never reaches the host kernel.
    Trace,
    /// Runs on the host untouched, because it cannot affect what the run does.
    Admit,
}

/// One row of the table.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    /// Syscall number on x86-64.
    pub nr: i64,
    /// Name, used in diagnostics so a refusal says what was refused.
    pub name: &'static str,
    /// What the filter does with it.
    pub disposition: Disposition,
}

const fn admit(nr: i64, name: &'static str) -> Entry {
    Entry { nr, name, disposition: Disposition::Admit }
}

const fn trace(nr: i64, name: &'static str) -> Entry {
    Entry { nr, name, disposition: Disposition::Trace }
}

/// Every syscall this backend handles.
///
/// Admitted calls touch only the guest's own address space or signal mask, which the world does
/// not model; admitting them keeps the stop rate low. A syscall missing from the table is traced
/// by default and refused by name, so it stops the run.
///
/// Task creation is traced, then carried out by the host: the model reads the word the thread
/// clears when it ends, and the new task is announced by its own trace event.
pub(crate) const TABLE: &[Entry] = &[
    // Traced by default; `mapping_rules` admits two narrow cases.
    trace(9, "mmap"),
    admit(25, "mremap"),
    // Traced by default; `advice_rules` admits every advice but `MADV_FREE`.
    trace(28, "madvise"),
    admit(10, "mprotect"),
    // Traced: the world tracks file mappings and must see one removed.
    trace(11, "munmap"),
    trace(26, "msync"),
    admit(12, "brk"),
    admit(13, "rt_sigaction"),
    admit(14, "rt_sigprocmask"),
    admit(131, "sigaltstack"),
    admit(15, "rt_sigreturn"),
    // Traced for the backend alone, which carries the clock page's reads past the new image and
    // lets the call go on; the world never sees it.
    trace(59, "execve"),
    admit(158, "arch_prctl"),
    admit(218, "set_tid_address"),
    admit(273, "set_robust_list"),
    // Traced and refused: the host kernel would write the current core into the guest.
    trace(334, "rseq"),
    trace(309, "getcpu"),
    trace(0, "read"),
    trace(1, "write"),
    trace(20, "writev"),
    trace(296, "pwritev"),
    trace(3, "close"),
    trace(5, "fstat"),
    trace(16, "ioctl"),
    trace(21, "access"),
    trace(7, "poll"),
    trace(8, "lseek"),
    trace(17, "pread64"),
    trace(18, "pwrite64"),
    trace(24, "sched_yield"),
    trace(35, "nanosleep"),
    trace(39, "getpid"),
    trace(56, "clone"),
    trace(62, "kill"),
    trace(63, "uname"),
    trace(72, "fcntl"),
    trace(79, "getcwd"),
    trace(83, "mkdir"),
    trace(264, "renameat"),
    trace(425, "io_uring_setup"),
    trace(426, "io_uring_enter"),
    trace(427, "io_uring_register"),
    trace(187, "readahead"),
    trace(140, "getpriority"),
    trace(141, "setpriority"),
    trace(142, "sched_setparam"),
    trace(144, "sched_setscheduler"),
    trace(285, "fallocate"),
    trace(277, "sync_file_range"),
    trace(90, "chmod"),
    trace(91, "fchmod"),
    trace(268, "fchmodat"),
    trace(294, "inotify_init1"),
    trace(254, "inotify_add_watch"),
    trace(255, "inotify_rm_watch"),
    trace(84, "rmdir"),
    trace(32, "dup"),
    trace(33, "dup2"),
    trace(292, "dup3"),
    trace(88, "symlink"),
    trace(89, "readlink"),
    trace(98, "getrusage"),
    // Traced: a resource limit is host state, and a guest may size tables from it.
    trace(97, "getrlimit"),
    trace(160, "setrlimit"),
    trace(302, "prlimit64"),
    trace(99, "sysinfo"),
    trace(137, "statfs"),
    trace(138, "fstatfs"),
    trace(102, "getuid"),
    trace(104, "getgid"),
    trace(107, "geteuid"),
    trace(108, "getegid"),
    trace(110, "getppid"),
    trace(111, "getpgrp"),
    trace(143, "sched_getparam"),
    trace(145, "sched_getscheduler"),
    trace(57, "fork"),
    trace(58, "vfork"),
    trace(60, "exit"),
    trace(73, "flock"),
    trace(74, "fsync"),
    trace(75, "fdatasync"),
    trace(77, "ftruncate"),
    trace(82, "rename"),
    trace(87, "unlink"),
    trace(96, "gettimeofday"),
    trace(157, "prctl"),
    trace(186, "gettid"),
    trace(201, "time"),
    trace(204, "sched_getaffinity"),
    trace(217, "getdents64"),
    trace(221, "fadvise64"),
    trace(234, "tgkill"),
    trace(202, "futex"),
    trace(228, "clock_gettime"),
    trace(230, "clock_nanosleep"),
    trace(231, "exit_group"),
    trace(2, "open"),
    trace(257, "openat"),
    trace(262, "newfstatat"),
    trace(263, "unlinkat"),
    trace(266, "symlinkat"),
    trace(267, "readlinkat"),
    trace(258, "mkdirat"),
    trace(293, "pipe2"),
    trace(316, "renameat2"),
    trace(332, "statx"),
    trace(41, "socket"),
    trace(42, "connect"),
    trace(43, "accept"),
    trace(50, "listen"),
    trace(52, "getpeername"),
    trace(288, "accept4"),
    trace(44, "sendto"),
    trace(45, "recvfrom"),
    trace(46, "sendmsg"),
    trace(47, "recvmsg"),
    trace(48, "shutdown"),
    trace(49, "bind"),
    trace(51, "getsockname"),
    trace(53, "socketpair"),
    trace(54, "setsockopt"),
    trace(55, "getsockopt"),
    trace(318, "getrandom"),
    trace(232, "epoll_wait"),
    trace(233, "epoll_ctl"),
    trace(281, "epoll_pwait"),
    trace(290, "eventfd2"),
    trace(291, "epoll_create1"),
    trace(435, "clone3"),
    trace(439, "faccessat2"),
];

/// Name of a syscall, where the table knows one.
pub(crate) fn name_of(nr: u32) -> Option<&'static str> {
    TABLE.iter().find(|e| e.nr == i64::from(nr)).map(|e| e.name)
}

/// Names for calls the table has no row for, used only to label a survey's report.
///
/// These calls are traced by default and refused. The list grows with what real targets ask for.
const UNMODELLED: &[(u32, &str)] = &[
    (283, "timerfd_create"),
    (286, "timerfd_settime"),
    (287, "timerfd_gettime"),
    (282, "signalfd"),
    (289, "signalfd4"),
    (319, "memfd_create"),
    (275, "splice"),
    (276, "tee"),
    (278, "vmsplice"),
    (40, "sendfile"),
    (326, "copy_file_range"),
    (437, "openat2"),
    (101, "ptrace"),
    (321, "bpf"),
];

/// Name of a syscall this build does not model, where one is known.
///
/// Separate from [`name_of`], so a survey never reports an unmodelled call as modelled.
pub(crate) fn name_of_unmodelled(nr: u32) -> Option<&'static str> {
    UNMODELLED.iter().find(|(n, _)| *n == nr).map(|(_, name)| *name)
}

/// Builds the in-kernel pre-filter.
///
/// The default action is to trace, so an unknown syscall reaches the supervisor and is refused by
/// name. Admitted calls skip the two stops a traced call costs.
///
/// `allocations` also traces `brk`, `mremap` and anonymous `mmap`, for a run where memory can run
/// out.
pub(crate) fn filter_program(allocations: bool) -> Result<BpfProgram, String> {
    /// `brk` and `mremap`, admitted unless allocations are watched.
    const ALLOCATING: [i64; 2] = [12, 25];
    let mut rules: std::collections::BTreeMap<i64, Vec<SeccompRule>> = TABLE
        .iter()
        .filter(|e| e.disposition == Disposition::Admit)
        .filter(|e| !(allocations && ALLOCATING.contains(&e.nr)))
        .map(|e| (e.nr, vec![]))
        .collect();
    let mut mapping = mapping_rules()?;
    if allocations {
        // The coverage map stays admitted; the anonymous mapping, the first rule, does not.
        mapping.remove(0);
    }
    rules.insert(MMAP, mapping);
    rules.insert(MADVISE, advice_rules()?);

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Trace(0),
        SeccompAction::Allow,
        TargetArch::x86_64,
    )
    .map_err(|e| e.to_string())?;

    filter.try_into().map_err(|e: seccompiler::BackendError| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No syscall is both in the table and in the unmodelled list.
    #[test]
    fn nothing_is_both_modelled_and_unmodelled() {
        for (nr, name) in UNMODELLED {
            assert_eq!(
                name_of(*nr),
                None,
                "{name} ({nr}) is in the table and must not be named as unmodelled"
            );
        }
    }

    #[test]
    fn the_unmodelled_list_has_no_duplicate_numbers() {
        let mut seen: Vec<u32> = UNMODELLED.iter().map(|(nr, _)| *nr).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "a number named twice would have two names");
    }

    #[test]
    fn the_table_has_no_duplicate_numbers() {
        let mut seen: Vec<i64> = TABLE.iter().map(|e| e.nr).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "a syscall listed twice would have two dispositions");
    }

    #[test]
    fn the_filter_builds_from_the_table() {
        let program = filter_program(false).expect("filter");
        assert!(!program.is_empty(), "an empty filter would trace nothing");
    }

    #[test]
    fn only_calls_that_cannot_reach_past_the_guest_are_admitted() {
        let mut admitted: Vec<&str> =
            TABLE.iter().filter(|e| e.disposition == Disposition::Admit).map(|e| e.name).collect();
        admitted.sort_unstable();
        assert_eq!(
            admitted,
            [
                "arch_prctl",
                "brk",
                "mprotect",
                "mremap",
                "rt_sigaction",
                "rt_sigprocmask",
                "rt_sigreturn",
                "set_robust_list",
                "set_tid_address",
                "sigaltstack",
            ],
            "an admitted call bypasses the supervisor, and this filter alone confines the guest. \
             Admit only calls that touch the guest's own address space, signal handling or exec."
        );
    }

    /// `mmap` is traced by default and admitted by two argument rules: anonymous mappings and the
    /// coverage channel.
    #[test]
    fn a_mapping_is_admitted_by_its_arguments_and_not_by_its_number() {
        let row = TABLE.iter().find(|e| e.nr == MMAP).expect("the table has a row for it");
        assert_eq!(
            row.disposition,
            Disposition::Trace,
            "the row says what reaches the supervisor when no condition matches"
        );
        assert_eq!(
            mapping_rules().expect("the rules build").len(),
            2,
            "one alternative for an anonymous mapping, one for the coverage channel"
        );
    }

    #[test]
    fn names_resolve_for_traced_calls() {
        assert_eq!(name_of(228), Some("clock_gettime"));
        assert_eq!(name_of(318), Some("getrandom"));
        assert_eq!(name_of(41), Some("socket"));
        // The ring calls are listed; the world answers them according to the scenario.
        assert_eq!(name_of(425), Some("io_uring_setup"));
        assert_eq!(name_of(426), Some("io_uring_enter"));
        assert_eq!(name_of(9999), None, "an unlisted syscall has no name to report");
    }
}
