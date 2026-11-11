# The nondeterminism source map

Living document, and the project's working checklist. **Until this table is closed, there is no
determinism.** Every row names a way the outside world can leak into a guest's execution, what we do
about it, how confident we are, and which test proves it.

This document exists separately from [`ARCHITECTURE.md`](./ARCHITECTURE.md) because the coverage it
records is what the tool's guarantee amounts to.

Neighbouring documents:
[`ARCHITECTURE.md`](./ARCHITECTURE.md) ·
[`API.md`](./API.md) ·
[`TESTING.md`](./TESTING.md) ·
[`PLATFORMS.md`](./PLATFORMS.md).

---

## 1. Status vocabulary

| Symbol | Meaning |
|:---:|---|
| **solved** | handled, with a test that fails if the handling is removed |
| **partial** | handled for the cases our targets exercise; a known shape is not covered, and it is named |
| **honest hole** | not solvable at this backend level. Detected and reported, never hidden |
| **deferred** | out of scope for v1 by decision, with the revisit condition stated |
| **by design** | not a hole: we deliberately do not reproduce this |
| **unproven** | handled as written, and no test fails when the handling is removed: a claim, not a result |
| **open** | not handled: the call is refused and stops the run, or the source reaches the guest as the host has it |

A row is never `solved` without a test that exists and fails when the mechanism is removed;
`scripts/check-docs.sh` refuses a cited `file::test` that no test is. A mechanism no test holds is
`unproven`, and a source no mechanism handles is `open`.

---

## 2. The map

### 2.1 Time

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| `clock_gettime`, `gettimeofday`, `time`, `clock_nanosleep` | wall time varies per run | seccomp-traced, answered from the node's own clock: an offset from the world clock and a rate, both drawn from the tape ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §4.2) | solved | `determinism::a_guest_that_replaces_itself_still_repeats` |
| **VDSO** — the same calls bypassing the kernel | a VDSO call never reaches seccomp, so it returns real time | the host's VDSO is never mapped: at exec the guest maps bohr's own, built from `crates/bohr-backend-ptrace/vdso/`, and a clock page beside it, and `AT_SYSINFO_EHDR` names it. The supervisor writes the node's clock into the page before each resume; the k-th read since returns it plus k reads' cost, and at the next stop the world moves until the node's own clock has passed the last of them, so a drifting clock never reads less after a stop than before it. An `execve` replaces the page; the backend carries what it counted to the next stop. A host that refuses the mapping leaves the guest without it, and the run is recorded as `clock_page_withheld`, as is a run under `BOHR_CLOCK_PAGE=off`; such a run, and every run recorded before v0.9.7, is run again with every read a system call. | solved | `determinism::a_guest_that_replaces_itself_still_repeats`, which reads the clock from 0 |
| **`RDTSC` / `RDTSCP`** | a raw instruction reading the CPU cycle counter | `prctl(PR_SET_TSC, PR_TSC_SIGSEGV)`; the resulting `SIGSEGV` is caught, the instruction decoded, `rdx:rax` filled from logical time, `rip` advanced | solved | `determinism::timestamp_counter_reproduces` |
| `poll`/`epoll` and futex timeouts, `nanosleep` | timers fire at real moments | virtual: expiry is a tick in the world's timer queue | solved | `determinism::poll_alone_answers_what_linux_answers`; `determinism::a_day_of_sleep_costs_no_real_time` |
| `timerfd`, `setitimer`, `alarm` | a timer the host kernel fires | not modelled: the call is refused and stops the run, as every unmodelled call does | open | `determinism::an_unmodelled_syscall_stops_the_run` |
| `sleep` of any form | real elapsed time | the clock jumps; the guest wakes instantly | solved | `determinism::a_day_of_sleep_costs_no_real_time` |
| clock skew between nodes | a real cluster's nodes disagree | each node has an offset and a drift rate, both drawn from the tape (`ClockSkew`) | solved | `determinism::node_clocks_are_their_own_not_the_worlds` |
| the date | a real clock reads today | every wall clock starts at 2025-01-01 00:00:00 UTC, fixed in the kernel (`WALL_EPOCH`), so a step back moves it and every host reads the same dates | solved | `goldens::minimal_world_digests_match_the_committed_goldens`, whose runs digest a wall-clock read |
| CPU frequency scaling affecting measured durations | a guest that measures work in cycles sees different numbers | logical time is a function of progress units, not of host frequency | solved | `run_loop::computation_costs_time_only_where_a_rate_is_given` |

**The one shape we handle badly and say so:** a guest that busy-polls a clock expecting real time to
pass, with no other syscall in the loop, spins forever in virtual time. We detect it (a clock read
count with no intervening progress) and emit `BOHR-2006` naming the task, rather than hanging
silently. It is `partial` because we diagnose rather than fix; fixing it means guessing what the
guest meant.

### 2.2 Randomness

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| `getrandom(2)` | kernel entropy | traced and emulated; bytes drawn from the tape at site `GuestRandom` | solved | `determinism::urandom_stable` |
| `/dev/urandom`, `/dev/random` | read through the filesystem | virtual filesystem nodes backed by the same tape draw | solved | same |
| **`AT_RANDOM`** | 16 random bytes handed to every process in the auxiliary vector; glibc uses them for stack canaries and hash seeds | zeroed before the guest's first instruction | unproven | none: nothing a test prints or digests depends on the canary or the pointer guard |
| glibc / libstdc++ hash seeds derived from `AT_RANDOM` | iteration order of the guest's own hash maps | falls out of `AT_RANDOM` being fixed | unproven | none |
| **`RDRAND` / `RDSEED`** | raw instructions; the CPU offers **no trap** for them | four steps: clear the CPUID feature bits so most libraries take a fallback path; search executable segments for `0F C7 /6` and `0F C7 /7`; **warn loudly** with addresses, including the ones nothing could establish; replace the established ones in the prepared files with `--patch-rdrand`, same length for same length | **honest hole**, and one a rewrite closes for the files a scenario prepares | `doctor::hardware_randomness_is_reported_where_it_is_and_only_there`; `entropy::a_real_one_between_ordinary_instructions_is_rewritten`; the `degraded` header: none |
| an in-guest PRNG seeded from any of the above | derived nondeterminism | eliminated by fixing the seeds above | unproven | follows from the rows above; no test seeds one |
| a PRNG seeded from a hardware source we did not cover | unknown unknowns | the repeat-and-compare gate is the backstop: it finds the *effect* even when we do not know the *cause* | partial | `determinism::repeat_1000` |

`RDRAND` is the single source that fundamentally requires a hypervisor, and it is the reason the
run header carries `determinism: full | degraded(reasons)` ([`API.md`](./API.md) §4.2). A run with an
un-neutralised `RDRAND` in a mapped page is **never** reported as deterministic. §5.8 in
[`ARCHITECTURE.md`](./ARCHITECTURE.md).

### 2.3 Scheduling and concurrency

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| thread interleaving | the OS scheduler is nondeterministic | total order: exactly one task runs at a time, chosen by a draw ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.4) | solved | `determinism::threads_reproduce_and_follow_the_seed`; `determinism::the_per_commit_guests_reproduce` |
| `futex` wake order | the kernel may wake waiters in any order | the wait queue is ours, ordered, and the wake choice is a draw | solved | `determinism::futex_wake_order_comes_from_the_tape` |
| core migration | a thread moving between cores changes timing and `CPUID` results | `sched_setaffinity` pins a guest and its supervisor to one core; a campaign gives each worker a core of its own (`BOHR_CORE`), one hardware thread of every physical core before a second thread of any | partial — which core is visible without a syscall: `RDPID` and the per-CPU segment limit report it and nothing traps them, so a guest that reads one sees its worker's position, and where `CPUID` is not trapped its APIC id says the same; one run on one host never moves | the pinning: none; `hunt::a_pack_is_hunted_in_one_command` runs real guests from several workers |
| `sched_yield` | exposes real scheduling | answered by the world, which picks the next task | unproven | none: `determinism::a_thread_that_yields_is_scheduled_again` passes as well when the call reaches the host |
| `sched_getcpu`, `getcpu` | expose the core a thread runs on | `rseq` is refused, so glibc's `sched_getcpu` asks `getcpu`, which the world answers core 0 and node 0 | solved | `host_independence::the_host_does_not_reach_the_guest`, run on two cores |
| `sched_setscheduler`, `sched_setparam` | a real policy changes when the host runs a thread | answered as an unprivileged process is: a normal policy is accepted and changes nothing, a real-time one is `EPERM` | unproven | none |
| futex timeouts | a timer is the host's clock | a timed wait arms a timer on the virtual clock; each of a task's waits is numbered, and a timer fires only into the wait that armed it | solved | `determinism::a_timed_wait_woken_early_leaves_no_timer_behind` |
| signals to another thread (`tgkill`) | delivery depends on where the host finds the thread | queued for the named task, its wait ended with `EINTR`, delivered at its next stop | solved | `determinism::what_was_refused_on_purpose_now_runs_and_repeats`, whose guest `signals_a_thread` checks the waiter handled it |
| a process failing from a thread other than its first | each thread reports its end in an order the host decides | the loop waits for a task the host is running before it picks, moves time or ends the run | solved | `determinism::a_failure_from_another_thread_is_found_on_every_run` |
| `rseq` (restartable sequences) | glibc registers it, and the host kernel then writes the current core into the guest's memory | traced and refused with `ENOSYS`, as a kernel without it refuses it; glibc runs without the area | solved | `host_independence::the_host_does_not_reach_the_guest`; `syscalls::only_calls_that_cannot_reach_past_the_guest_are_admitted` holds it traced |
| CPU memory-model effects (store buffering, weak ordering) | genuine parallelism | **not reproduced** — we serialise | **by design** | documented in `ARCHITECTURE.md` §13 |
| lock contention timing | dependent on real parallelism | interleaving is explored instead of observed; this finds *more* orderings than reality, not fewer | by design | — |

The "by design" rows are the honest limit of a serialising simulator. A bug that requires two cores
to genuinely execute at once needs `loom`, and our documentation says so instead of implying
coverage we do not have.

### 2.4 Address space and process identity

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| **ASLR** | every mapping lands at a different address | `personality(ADDR_NO_RANDOMIZE)` before `exec` | unproven | none: the digest keeps whether an address is null, not the address |
| stack canaries | derived from `AT_RANDOM` | covered by §2.2 | unproven | — |
| `mmap` placement without a hint | the kernel's choice of address varies with the layout of what came before | deterministic given no ASLR and a deterministic sequence of prior mappings; verified rather than assumed | partial | none: an address is not digested |
| `brk` / heap layout | allocator behaviour depends on addresses | falls out of the above | partial | same |
| **pids and tids** | assigned by the host kernel | virtualised: guests see our numbering, assigned in creation order; PID namespace for isolation | solved | `goldens::minimal_world_digests_match_the_committed_goldens`, whose runs digest what `getpid` and `gettid` answer |
| parent pid, process group, session id | same | virtualised | unproven | none |
| thread stack addresses | the loader's arithmetic | deterministic once ASLR is off | partial | none: an address is not digested |
| **environment variables and argv** | differ per invocation and per machine (`PWD`, `HOSTNAME`, `_`) | the environment presented to the guest is built from the config, not inherited; extra host variables must be listed explicitly | unproven | none |
| the auxiliary vector generally | carries host details beyond `AT_RANDOM` | laid out again at exec: one list of twenty entries in one order, the processor's entries fixed (`AT_HWCAP` the x86-64-v2 baseline, `AT_HWCAP2` 0, `AT_MINSIGSTKSZ` 8192), the ids 0, and the block rewritten under the random bytes, so the host's entry count no longer moves the stack | solved | `host_independence::the_host_does_not_reach_the_guest`; `determinism::a_guest_that_replaces_itself_still_repeats` |
| the executed path | the kernel copies it to the top of the new stack (`AT_EXECFN`), so a checkout's directory moves every address below it | the host path reaches `execve` alone, made absolute and padded with leading slashes to 512 bytes; `AT_EXECFN` is rewritten to the name the scenario starts the program by, and `argv[0]` and `/proc/self/exe` are that name; a hunt's `--binary` and a trapped copy run in place of the name, not as it. Not covered: a `#!` script started directly, which the kernel hands its interpreter by the executed path, so the script's `$0` is the padded host path; start the interpreter with the script as its argument | solved for programs | `host_independence::the_host_does_not_reach_the_guest`, from two directories of different length |
| the host's resource limits | a stack limit the guest runs under while the world answers another; an unlimited stack also switches the mapping layout | the guest starts with an 8 MiB stack and unlimited address space and data, which `getrlimit` answers; a host whose hard limit is lower is refused | solved | `host_independence::the_host_does_not_reach_the_guest`, under `ulimit -s unlimited` |
| the supervisor's signal state | an ignored signal survives `execve`, and the supervisor ignores `SIGPIPE` | every disposition reset to its default and the mask cleared before exec | solved | `host_independence::the_host_does_not_reach_the_guest` |
| a fatal signal in a threaded process | the host reports the threads' deaths in an order of its own | the backend reaps the whole thread group at the first death and reports one exit per thread in task order, each with the group's signal | solved | `host_independence::a_fault_that_ends_a_threaded_process_repeats` |

### 2.5 CPU identity

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| **`CPUID`** | reports the real CPU: features, cache sizes, core count | two ways to the same fault: `arch_prctl(ARCH_SET_CPUID, 0)` where the platform exposes it, and `--patch-cpuid` where it does not — the instruction is replaced in a copy of the program and of the prepared files by one nothing else emits. Either way we catch, decode and answer from a **fixed leaf table** identical on every machine | solved | `presented_processor::the_vendor_reaches_the_guest_whole`; `cpu::the_profile_is_the_one_that_was_committed`; `cli::a_processor_question_is_answered_where_the_host_cannot_trap_it`; the `ARCH_SET_CPUID` trap itself: none |
| feature-dependent code paths (AVX-512, SHA extensions) | different code runs on different CPUs | falls out of the fixed CPUID answer: the same code path is taken everywhere | solved | `cpu::nothing_above_the_presented_level` |
| **the `RDRAND` feature bit** | libraries probe it and use the instruction | cleared in our CPUID answer, which sends most libraries down a fallback path — the cheap 80% of the `RDRAND` problem | solved | `cpu::hardware_randomness_is_absent`; `presented_processor::the_guest_is_told_there_is_no_hardware_randomness` |
| core count from `CPUID` and `sysconf(_SC_NPROCESSORS_ONLN)` | a guest sizes thread pools by it | presented as 1; `sysconf` reads `/sys` and `/proc`, which are our virtual filesystem | partial | `cpu::the_guest_is_shown_one_processor`, for `CPUID`; what `sysconf` answers: none |
| `/proc/cpuinfo` | the same information through the filesystem | absent: nothing under `/proc` but a process's own files exists, so an open answers `ENOENT` on every host | unproven | none |
| cache line size affecting behaviour | rarely, but real | part of the fixed leaf table | solved | `cpu::the_profile_is_the_one_that_was_committed` |

The fixed CPUID table is committed in `crates/bohr-world/src/cpu.rs` and is part of the bundle's
host profile ([`API.md`](./API.md) §4.5). Changing it changes every stored run, so it is versioned
with the tape ABI.

**It sits in the world and not in the backend**, beside the kernel release the guest is told.
Which processor a guest is shown is a decision about the world it runs in, of the same kind as the
release, and a backend that owned it would be one layer deciding what belongs to the layer above. The backend's part is smaller and stays there: notice the instruction, decode
which leaf was asked for, hand the question up.

**CPUID faulting is a hardware capability the platform may withhold.** It requires
`X86_FEATURE_CPUID_FAULT`, and a hypervisor that does not pass the underlying MSR through makes it
unavailable regardless of kernel or physical CPU — `arch_prctl` then returns `ENODEV`. Where that
happens the guest sees the real processor: runs on one machine still reproduce, runs across machines
do not, and the bundle records `determinism: degraded` naming the cause rather than claiming a
portability it does not have. Details and detection in [`PLATFORMS.md`](./PLATFORMS.md) §2.1.

### 2.6 Filesystem

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| real file contents and timestamps | differ per machine | mount namespace with a private root; a virtual disk owned by the world | solved | `determinism::virtual_disk_roundtrip` |
| directory iteration order | filesystem-dependent | our virtual filesystem returns entries in a defined order | unproven | none |
| inode numbers, device numbers | host-assigned | virtualised | unproven | none |
| free space, `statfs` | machine-dependent | the world's own numbers: one device, its block size, free space from what it holds | unproven | none: `filesystem::the_filesystem_reports_its_own_numbers` checks that the calls answer, not what |
| **`fsync` semantics** | what survives a crash | unsynced data is lost, kept, or kept sector by sector on a crash, by one fixed-width draw; `reorder_unsynced = false` keeps all or none | solved | `determinism::unsynced_data_is_lost_on_a_crash`, `bohr check header_without_record` |
| write tearing, pre-`fsync` reordering | real disks do both | modelled as faults drawn from the tape | solved | `determinism::a_torn_write_is_visible_after_the_crash` |
| file descriptor numbers | assigned by the kernel; usually deterministic but not guaranteed under our own interception | virtualised: the world hands out the lowest free number, as POSIX requires | solved | `descriptors::every_kind_of_descriptor_takes_the_lowest_free_number`; `deterministic_run::descriptors_are_the_lowest_free_number` |
| `/proc/self/*` | exposes host state broadly | virtual, generated from world state | partial | `proc_is_not_on_the_disk::the_same_run_without_a_crash_reads_it_too`; `cli::names_and_descriptors_answer_the_way_the_kernel_does` — the covered set is listed in `PLATFORMS.md` §4 |
| memory-mapped files | writes bypass syscalls entirely | pages of mapped files are backed by the virtual disk; writes are visible at `msync`/`munmap`/unmapping | partial | `determinism::a_mapped_file_and_its_descriptor_agree` |
| `fallocate`, `sync_file_range`, `chmod` and kin | allocation and modes are the host filesystem's | answered from the world's own file: `fallocate` grows it and a crash treats the reserved bytes as written; modes are held and never consulted | unproven | `disk_space::a_full_disk_refuses_only_new_space`, for a reservation; the rest: none |
| `statx`, `prctl` | the host's file and process state | answered from the world: `statx` as `fstat` is, `prctl` for the names and settings a runtime asks about | unproven | none |
| `inotify` | a host facility watching the host's files | answered `ENOSYS`, on purpose: the programs that use it fall back to polling | by design | `survey::a_call_answered_on_purpose_is_still_a_wall`; `bohr check geth`, `walls` |

`mmap`-based I/O is `partial` and it is the most likely source of a surprise for embedded databases,
which is precisely the first hunting target list.

### 2.7 Network

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| real packets, real latency, real loss | the network is the definition of nondeterministic | a filter that traces by default, so no network call reaches the host; a fully in-memory virtual network | solved | `determinism::five_node_cluster_repeats_exactly`; `syscalls::only_calls_that_cannot_reach_past_the_guest_are_admitted` |
| delivery order | reorders in reality | modelled; ties broken by `(tick, node, seq)` never by insertion order | solved | `timers::firing_order_is_tick_then_node_then_arming_order`; `determinism::same_tick_timers_fire_in_a_recorded_order` |
| **one-directional partitions** | A sees B, B does not see A | first-class in the partition model | solved | `determinism::a_directional_partition_cuts_one_way_only` |
| ephemeral port assignment | kernel-chosen | assigned by us in a defined sequence | unproven | none |
| DNS | resolver state, network access | none of its own: a resolver reads what the prepared state holds under `/etc`, and a query has no server in the virtual network to reach | open | — |
| readiness (`poll`, `epoll`, `eventfd`) | the kernel's wait queues | the world's own readiness: a watch woken by what it waits for, an `eventfd` write waking its pollers, all ordered | solved | `poll::a_poll_is_woken_by_the_datagram_it_waits_for`; `descriptors::a_blocking_counter_at_nought_waits`; golden `epoll` workload, `bohr check golden` |
| edge-triggered and one-shot `epoll` watches | the kernel reports a change once | modelled with the rest of readiness | unproven | none |
| datagram corruption | bit errors are physical | drawn at one width per datagram where the scenario arms it: whether, where and by which mask | solved | `bohr check no_checksum` |
| pipes | a kernel buffer between two descriptors | in a world with a `[cluster]`, a stream pair: a read blocks on empty, a write wakes it, a closed write end reads as the end; the direction is not enforced | partial | golden `pipe` workload |
| TCP behaviour (Nagle, window scaling, retransmits) | kernel stack details | a simplified stream model preserving ordering and framing, not the RFC | partial | documented; a guest depending on precise TCP timing is out of scope |
| `SO_REUSEPORT` load balancing | kernel hashes connections across listeners | a connection or datagram to a shared port goes to a holder drawn from the tape | unproven | none: `determinism::a_port_is_shared_only_where_everyone_asked_for_it` binds the port and sends nothing to it |

### 2.8 Everything else

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| `uname`, `/proc/version`, kernel release strings | guests branch on kernel version | fixed presented release (`5.15.0-bohr`) | unproven | none |
| `getpid`-derived seeds, `gettid`-derived sharding | very common idiom | covered by pid virtualisation | solved | `goldens::minimal_world_digests_match_the_committed_goldens`, as for pids |
| signals from the host (`SIGWINCH`, job control) | arrive at arbitrary moments | the guest is isolated; signals we did not generate are not delivered | unproven | none |
| signal delivery timing generally | the kernel picks a moment | delivered at the receiving task's next stop, so the moment follows the drawn schedule; no moment of its own is drawn, and one between two stops is never tried | partial | none |
| `io_uring` | an asynchronous syscall path whose work leaves the guest through shared memory | by default **declined** at `io_uring_setup` with `ENOSYS`, and entering or registering a ring **refused** with `BOHR-2004`; under `[linux] io_uring = "ring"` a ring is opened, its poller always asleep, and every submission is read at `io_uring_enter`, carried out in queue order as its plain call, and completed before the call returns | full for the seven file operations under `"ring"`; partial by default — the guest runs its fallback, and each run records that | `determinism::io_uring_is_declined_at_the_door_and_refused_past_it`; `ring::a_ring_repeats`; `ring::a_sync_through_the_ring_meets_the_disk_model` |
| `vfork`, `clone3` exotic flag combinations | affect address-space sharing | supported combinations are enumerated; others fail with `BOHR-2002` | partial | `PLATFORMS.md` §4 |
| `ptrace` used *by* the guest | nested tracing | refused with a named error | deferred | — |
| direct device access, DMA, `/dev/mem` | below our level entirely | refused | **honest hole** — hypervisor only | — |
| hardware performance counters read by the guest | real counts | refused or emulated from progress units | partial | — |
| `madvise(MADV_FREE)` and lazy page reclaim | the kernel decides when | the filter stops `madvise` with `MADV_FREE` alone, and the world turns it into `MADV_DONTNEED`, which it permits; the page reads zeros on every host | solved | `host_independence::the_host_does_not_reach_the_guest` |
| the guest's own use of uninitialised memory | its values depend on what was there before | **by design not our problem** — but it produces divergence, so we detect it via repeat-and-compare and report it as a *guest bug*, which is a finding, not a Bohr failure | by design | none: no test runs a guest that reads it |

The last row inverts the usual reading. Uninitialised-memory use in the guest makes the digests
diverge, and the response is not to work around it but to report "the program is nondeterministic
here, at this event" — a genuine bug in the guest rather than a failure of the tool.

---

## 3. What the map means

**Syscall-level interception is sufficient for the overwhelming majority of server software.** Of
the rows above, three are honest holes at this backend level: `RDRAND`/`RDSEED`, direct device
access, and true multi-core execution. The first is mitigated to the point where most libraries
never reach it; the second and third do not occur in the software we are hunting in. The `open`
rows are not holes of the level: the timer descriptors and a resolver are each within a trace's
reach, and the map says they are not reached rather than calling them `solved`.

Full determinism requires a hypervisor, and the architecture is built for it in advance: the backend
boundary ([`API.md`](./API.md) §3) is the seam, and nothing above it knows how determinism is
achieved.

---

## 4. Adding a row

When a new nondeterminism source is discovered — and it will be, because that is what the repeat
gate is for — the change is:

1. add the row here with status `partial` and a description of the observed divergence;
2. add the failing test to [`TESTING.md`](./TESTING.md) §7 with the seed that found it;
3. add the bundle to `testdata/bundles/`;
4. fix it, move the status, and add an injection to the mutation harness
   ([`TESTING.md`](./TESTING.md) §7.4) so the class cannot silently return.

The document is closed when every row is `solved`, `by design`, or a named honest hole. It is not
closed today, and the status column is what states that.

## 5. How real each fault is

A finding is a defect of the target when the faults it needed are ones a real host produces. Each
kind of fault the world draws carries a grade, `FaultKind::grade` in `bohr-trace`, and the source
the grade rests on. `bohr ablate` finds which kinds a finding needed, and the weakest grade among
them is the finding's (`API.md` §7.7).

| Kind | Grade | Why |
|---|---|---|
| `node_kill` | real | a process killed, or a host losing power: under `sync_semantics = "strict"` everything the program synced survives and nothing else is promised, which is what `fsync(2)` promises |
| `node_freeze` | real | `SIGSTOP`, a garbage-collection pause, a VM paused by its hypervisor |
| `node_thaw` | real | the end of a freeze |
| `net_partition` | real | a cut link or a node isolated by a switch or a firewall |
| `net_drop`, `net_delay`, `net_duplicate`, `net_reorder` | real | drawn on datagrams only, which IP delivers at most once, late or out of order, and UDP does not repair (RFC 768); a stream is never dropped, duplicated or reordered here |
| `disk_stall` | real | an `fsync` waiting behind other writeback on a loaded device |
| `disk_full` | real | `write(2)` and `fallocate(2)` answer `ENOSPC` when they need new space; an overwrite of what a file holds or of what `fallocate` reserved allocates nothing on ext4 and is never refused |
| `disk_error` | real | `fsync(2)` answers `EIO` and Linux, since 4.13, reports the writeback error once and marks the failed pages clean, so the unsynced data is gone |
| `memory_exhausted` | real | `mmap(2)` answers `ENOMEM` under a resource limit or strict overcommit |
| `clock_step`, `clock_drift` | real, within the scenario's bound | NTP steps a clock it finds far off and slews one that drifts, by up to 500 ppm |
| `chain_reorg` | real | a chain's fork choice replaces blocks it had made head |
| `disk_tear` | plausible | a device promises atomic writes of one sector, 512 bytes or 4 KiB; a write spanning sectors can land partly when power fails |
| `disk_reorder` | plausible | unsynced writes to different files reach the device in no promised order; a file system with delayed allocation lands them as its writeback chooses |
| `node_restart` | plausible | an operator restoring a node from an older snapshot or backup |
| `net_corrupt` | adversarial | the UDP checksum catches random corruption, so a datagram altered and still delivered is one an attacker altered |

No current kind is graded model-only: each is something a host, a network or an operator does. A
kind added later is graded when it is added, and a kind the model produces in a way no host does is
graded model-only rather than given a grade it has not earned.
