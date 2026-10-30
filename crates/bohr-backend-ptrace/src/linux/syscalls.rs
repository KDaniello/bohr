//! The syscall table, and the seccomp filter generated from it.
//!
//! One table drives both the in-kernel filter and the supervisor's dispatch. A syscall that is
//! handled but not filtered, or filtered but not handled, is a hole in determinism that nothing
//! else would notice, so neither list is written by hand.

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use super::coverage::COVERAGE_FD;

/// `mmap`, the one call whose disposition depends on its arguments rather than on its number.
const MMAP: i64 = 9;

/// `MAP_ANONYMOUS`: the mapping is backed by nothing, so it cannot see a file.
const MAP_ANONYMOUS: u64 = 0x20;

/// When a mapping may go straight to the host.
///
/// **A mapping of a file must not.** Admitting every `mmap` sends one the guest made of a
/// descriptor *this* process holds — and the numbering of world descriptors and of the supervisor's
/// own are unrelated, so a guest with enough files open could map the coverage counters into its
/// own address space and write them. That is not a syscall the model failed to answer; it is a
/// guest reaching through the boundary, and no run above it would notice.
///
/// Two mappings are still admitted, for reasons that are about the address space rather than about
/// any file. An anonymous one is backed by nothing, which is what every allocator asks for and what
/// keeps the stop rate low enough to be usable — and the test is on the flag rather than on the
/// descriptor, because a caller may name any descriptor it likes once the flag is set. The coverage
/// map is the second: it is this engine's own channel, opened by the supervisor and handed over
/// deliberately, and the guest writing to it is the entire point of it existing.
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

    // Separate rules rather than one: a filter's rules for a syscall are alternatives, and these
    // two are alternatives. Conditions inside one rule would have to hold together.
    Ok(vec![
        SeccompRule::new(vec![anonymous]).map_err(|e| e.to_string())?,
        SeccompRule::new(vec![ours]).map_err(|e| e.to_string())?,
    ])
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

/// Every syscall this backend has an opinion about.
///
/// Admitted calls shape only the guest's own address space or its signal mask, neither of which the
/// world models; letting them through is what keeps the stop rate low enough to be usable. Anything
/// absent from this table is traced by the filter's default action and refused by name, so an
/// unmodelled syscall stops the run rather than quietly reaching the host.
///
/// Task creation is traced but carried out by the host: the model reads the call for the word a
/// thread clears when it ends, and then lets it through, because a real thread is what the guest
/// needs and the new task is announced by a trace event of its own.
pub(crate) const TABLE: &[Entry] = &[
    // Traced, and admitted back by two narrow rules the filter adds: see `mapping_rules`. The row
    // says what reaches the supervisor by default, and for this one call that is the honest
    // answer.
    trace(9, "mmap"),
    admit(25, "mremap"),
    admit(28, "madvise"),
    admit(10, "mprotect"),
    // Traced for the same reason `mmap` is: a mapping of a file is the world's, and a world that
    // did not see it go would keep writing into an address the guest had given back.
    trace(11, "munmap"),
    trace(26, "msync"),
    admit(12, "brk"),
    admit(13, "rt_sigaction"),
    admit(14, "rt_sigprocmask"),
    admit(131, "sigaltstack"),
    admit(15, "rt_sigreturn"),
    admit(59, "execve"),
    admit(158, "arch_prctl"),
    admit(218, "set_tid_address"),
    admit(273, "set_robust_list"),
    admit(334, "rseq"),
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
    trace(84, "rmdir"),
    trace(32, "dup"),
    trace(33, "dup2"),
    trace(292, "dup3"),
    trace(88, "symlink"),
    trace(89, "readlink"),
    trace(98, "getrusage"),
    // Traced rather than admitted, which it was: a limit is a fact about the host, and a guest
    // that sized a table from the real one would behave differently on a machine with a different
    // `ulimit`.
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

/// Names for calls the table deliberately has no row for.
///
/// Documentation and nothing else. The table above decides what the in-kernel filter admits and
/// what the supervisor answers, and a call here belongs to neither list — it reaches the supervisor
/// through the filter's default action and is refused. So a name that is wrong here cannot make a
/// run wrong; it can only make a survey's report harder to read, which is the entire reason the
/// list exists. Kept short and grown from what real targets are seen asking for.
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
    (285, "fallocate"),
    (277, "sync_file_range"),
    (101, "ptrace"),
    (321, "bpf"),
    (426, "io_uring_enter"),
    (427, "io_uring_register"),
];

/// Name of a syscall this build does not model, where one is known.
///
/// Separate from [`name_of`] on purpose: the two answer different questions, and a single lookup
/// that fell through from one list to the other would let a survey report a call as modelled.
pub(crate) fn name_of_unmodelled(nr: u32) -> Option<&'static str> {
    UNMODELLED.iter().find(|(n, _)| *n == nr).map(|(_, name)| *name)
}

/// Builds the in-kernel pre-filter.
///
/// The default action is to trace, not to allow: an unknown syscall must reach the supervisor to be
/// refused by name. Bare tracing would stop twice on every syscall including the ones nothing
/// models, which is the cost this filter exists to avoid.
pub(crate) fn filter_program() -> Result<BpfProgram, String> {
    let mut rules: std::collections::BTreeMap<i64, Vec<SeccompRule>> = TABLE
        .iter()
        .filter(|e| e.disposition == Disposition::Admit)
        .map(|e| (e.nr, vec![]))
        .collect();
    rules.insert(MMAP, mapping_rules()?);

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

    /// The two lists answer opposite questions, so a number in both is a contradiction: it would
    /// be reported as modelled by one lookup and as absent by the other. Mechanical, because the
    /// table grows and nobody re-reads thirty numbers when it does.
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
        let program = filter_program().expect("filter");
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
                "execve",
                "madvise",
                "mprotect",
                "mremap",
                "rseq",
                "rt_sigaction",
                "rt_sigprocmask",
                "rt_sigreturn",
                "set_robust_list",
                "set_tid_address",
                "sigaltstack",
            ],
            "an admitted call runs without the supervisor ever seeing it, and the guest is confined \
             by this filter alone — it shares the host's network namespace. Every name here shapes \
             only the guest's own address space, its signal handling or its own exec. A call that \
             does more belongs in the traced half, and adding one here is how the confinement would \
             be lost quietly."
        );
    }

    /// `mmap` is not on that list, and the reason is the whole point of the conditions.
    ///
    /// It shapes the guest's address space, which would put it there — but only until it names a
    /// file, and then it reaches a descriptor rather than a page. The filter admits it back by the
    /// two shapes that cannot: a mapping backed by nothing, and this engine's own coverage channel.
    /// Everything else stops at the supervisor and is refused by name.
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
        // Opening a ring is listed, because the world now answers it rather than refusing the
        // run: a kernel without `io_uring` is a world a guest meets, and no ring means nothing
        // submits work past the interception. Entering one stays unlisted and refused by name --
        // this table decides what the filter admits, and a call that must reach the supervisor to
        // be refused belongs in neither list, which the default action already arranges.
        assert_eq!(name_of(425), Some("io_uring_setup"));
        assert_eq!(name_of(426), None, "an unlisted syscall has no name to report");
    }
}
