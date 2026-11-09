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

A row may never sit at "we think it is fine". Either a test proves it, or it is `partial` with the
gap written down.

---

## 2. The map

### 2.1 Time

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| `clock_gettime`, `gettimeofday`, `time`, `clock_nanosleep` | wall time varies per run | seccomp-traced, answered from the node's own clock: an offset from the world clock and a rate, both drawn from the tape ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §4.2) | solved | `determinism::clock_stable` |
| **VDSO** — the same calls bypassing the kernel | a VDSO call never reaches seccomp, so it returns real time | zero `AT_SYSINFO_EHDR` in the auxiliary vector at exec; the loader then has no VDSO to map and the calls become real syscalls | solved | `system::vdso_disabled_forces_syscalls` |
| **`RDTSC` / `RDTSCP`** | a raw instruction reading the CPU cycle counter | `prctl(PR_SET_TSC, PR_TSC_SIGSEGV)`; the resulting `SIGSEGV` is caught, the instruction decoded, `rdx:rax` filled from logical time, `rip` advanced | solved | `system::rdtsc_emulated` |
| `timerfd`, `setitimer`, `alarm`, `poll`/`epoll` timeouts | timers fire at real moments | all timers are virtual: expiry is a tick in the world's timer queue | solved | `integration::virtual_time_compresses_sleep` |
| `sleep` of any form | real elapsed time | the clock jumps; the guest wakes instantly | solved | same |
| clock skew between nodes | a real cluster's nodes disagree | each node has an offset and a drift rate, both drawn from the tape (`ClockSkew`) | solved | `integration::clock_skew_visible_to_guest` |
| CPU frequency scaling affecting measured durations | a guest that measures work in cycles sees different numbers | logical time is a function of progress units, not of host frequency | solved | `portability::cross_cpu` |

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
| **`AT_RANDOM`** | 16 random bytes handed to every process in the auxiliary vector; glibc uses them for stack canaries and hash seeds | zeroed before the guest's first instruction | solved | `system::at_random_zeroed` |
| glibc / libstdc++ hash seeds derived from `AT_RANDOM` | iteration order of the guest's own hash maps | falls out of `AT_RANDOM` being fixed | solved | `system::hash_order_stable` |
| **`RDRAND` / `RDSEED`** | raw instructions; the CPU offers **no trap** for them | four steps: clear the CPUID feature bits so most libraries take a fallback path; search executable segments for `0F C7 /6` and `0F C7 /7`; **warn loudly** with addresses, including the ones nothing could establish; replace the established ones in the prepared files with `--patch-rdrand`, same length for same length | **honest hole**, and one a rewrite closes for the files a scenario prepares | `system::rdrand_detected_and_reported` |
| an in-guest PRNG seeded from any of the above | derived nondeterminism | eliminated by fixing the seeds above | solved | — |
| a PRNG seeded from a hardware source we did not cover | unknown unknowns | the repeat-and-compare gate is the backstop: it finds the *effect* even when we do not know the *cause* | partial | `determinism::repeat_1000` |

`RDRAND` is the single source that fundamentally requires a hypervisor, and it is the reason the
run header carries `determinism: full | degraded(reasons)` ([`API.md`](./API.md) §4.2). A run with an
un-neutralised `RDRAND` in a mapped page is **never** reported as deterministic. §5.8 in
[`ARCHITECTURE.md`](./ARCHITECTURE.md).

### 2.3 Scheduling and concurrency

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| thread interleaving | the OS scheduler is nondeterministic | total order: exactly one task runs at a time, chosen by a draw ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.4) | solved | `determinism::threads_racing_stable` |
| `futex` wake order | the kernel may wake waiters in any order | the wait queue is ours, ordered, and the wake choice is a draw | solved | `integration::futex_wake_order_from_tape` |
| core migration | a thread moving between cores changes timing and `CPUID` results | `sched_setaffinity` pins a guest and its supervisor to one core; a campaign gives each worker a core of its own (`BOHR_CORE`), one hardware thread of every physical core before a second thread of any | partial — which core is visible without a syscall: `RDPID` and the per-CPU segment limit report it and nothing traps them, so a guest that reads either sees its worker's position, and where `CPUID` is not trapped its APIC id says the same; one run on one host never moves | `system::single_core_pinned`; `hunt::a_pack_is_hunted_in_one_command` runs real guests from several workers |
| `sched_yield`, `sched_getcpu` | expose real scheduling | emulated; `sched_getcpu` always returns 0 | solved | `system::sched_calls_emulated` |
| `sched_setscheduler`, `sched_setparam` | a real policy changes when the host runs a thread | answered as an unprivileged process is: a normal policy is accepted and changes nothing, a real-time one is `EPERM` | solved | `bohr-world` `minimal` tests |
| futex timeouts | a timer is the host's clock | a timed wait arms a timer on the virtual clock; each of a task's waits is numbered, and a timer fires only into the wait that armed it | solved | `determinism::a_timed_wait_woken_early_leaves_no_timer_behind` |
| signals to another thread (`tgkill`) | delivery depends on where the host finds the thread | queued for the named task, its wait ended with `EINTR`, delivered at its next stop | solved | `determinism` guest `signals_a_thread` |
| a process failing from a thread other than its first | each thread reports its end in an order the host decides | the loop waits for a task the host is running before it picks, moves time or ends the run | solved | `determinism::a_failure_from_another_thread_is_found_on_every_run` |
| `rseq` (restartable sequences) | glibc registers it; behaviour depends on preemption points | registration is refused with `ENOSYS`, which glibc handles by falling back | solved | `system::rseq_neutralised` |
| CPU memory-model effects (store buffering, weak ordering) | genuine parallelism | **not reproduced** — we serialise | **by design** | documented in `ARCHITECTURE.md` §13 |
| lock contention timing | dependent on real parallelism | interleaving is explored instead of observed; this finds *more* orderings than reality, not fewer | by design | — |

The "by design" rows are the honest limit of a serialising simulator. A bug that requires two cores
to genuinely execute at once needs `loom`, and our documentation says so instead of implying
coverage we do not have.

### 2.4 Address space and process identity

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| **ASLR** | every mapping lands at a different address | `personality(ADDR_NO_RANDOMIZE)` before `exec` | solved | `system::aslr_disabled` |
| stack canaries | derived from `AT_RANDOM` | covered by §2.2 | solved | — |
| `mmap` placement without a hint | the kernel's choice of address varies with the layout of what came before | deterministic given no ASLR and a deterministic sequence of prior mappings; verified rather than assumed | partial | `system::mmap_layout_stable` |
| `brk` / heap layout | allocator behaviour depends on addresses | falls out of the above | partial | same |
| **pids and tids** | assigned by the host kernel | virtualised: guests see our numbering, assigned in creation order; PID namespace for isolation | solved | `system::pids_virtualised` |
| parent pid, process group, session id | same | virtualised | solved | same |
| thread stack addresses | the loader's arithmetic | deterministic once ASLR is off | partial | covered by repeat-and-compare |
| **environment variables and argv** | differ per invocation and per machine (`PWD`, `HOSTNAME`, `_`) | the environment presented to the guest is built from the config, not inherited; extra host variables must be listed explicitly | solved | `system::env_is_from_config` |
| the auxiliary vector generally | carries host details beyond `AT_RANDOM` | normalised at exec: `AT_SYSINFO_EHDR` zeroed, `AT_HWCAP`/`AT_HWCAP2` fixed to the presented profile, `AT_PLATFORM` fixed | solved | `system::auxv_normalised` |

### 2.5 CPU identity

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| **`CPUID`** | reports the real CPU: features, cache sizes, core count | two ways to the same fault: `arch_prctl(ARCH_SET_CPUID, 0)` where the platform exposes it, and `--patch-cpuid` where it does not — the instruction is replaced in a copy of the program and of the prepared files by one nothing else emits. Either way we catch, decode and answer from a **fixed leaf table** identical on every machine | solved | `portability::cross_cpu` |
| feature-dependent code paths (AVX-512, SHA extensions) | different code runs on different CPUs | falls out of the fixed CPUID answer: the same code path is taken everywhere | solved | same |
| **the `RDRAND` feature bit** | libraries probe it and use the instruction | cleared in our CPUID answer, which sends most libraries down a fallback path — the cheap 80% of the `RDRAND` problem | solved | `system::rdrand_bit_cleared` |
| core count from `CPUID` and `sysconf(_SC_NPROCESSORS_ONLN)` | a guest sizes thread pools by it | presented as 1; `sysconf` reads `/sys` and `/proc`, which are our virtual filesystem | solved | `system::cpu_count_is_one` |
| `/proc/cpuinfo` | the same information through the filesystem | virtual file generated from the presented profile | solved | same |
| cache line size affecting behaviour | rarely, but real | part of the fixed leaf table | solved | — |

The fixed CPUID table is committed in `crates/bohr-world/src/cpu.rs` and is part of the bundle's
host profile ([`API.md`](./API.md) §4.5). Changing it changes every stored run, so it is versioned
with the tape ABI.

**It sits in the world and not in the backend**, beside the kernel release the guest is told and the
`/proc/cpuinfo` it reads. Which processor a guest is shown is a decision about the world it runs in,
of the same kind as those two, and a backend that owned it would be one layer deciding what belongs
to the layer above. The backend's part is smaller and stays there: notice the instruction, decode
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
| real file contents and timestamps | differ per machine | mount namespace with a private root; a virtual disk owned by the world | solved | `integration::virtual_disk_roundtrip` |
| directory iteration order | filesystem-dependent | our virtual filesystem returns entries in a defined order | solved | `system::readdir_order_stable` |
| inode numbers, device numbers | host-assigned | virtualised | solved | `system::stat_fields_virtual` |
| free space, `statfs` | machine-dependent | from configuration | solved | same |
| **`fsync` semantics** | what survives a crash | strict model: unsynced data is **lost** on a node kill | solved | `integration::unsynced_data_lost_on_crash` |
| write tearing, pre-`fsync` reordering | real disks do both | modelled as faults drawn from the tape | solved | `integration::torn_write_visible` |
| file descriptor numbers | assigned by the kernel; usually deterministic but not guaranteed under our own interception | virtualised at the boundary | solved | `system::fds_virtualised` |
| `/proc/self/*` | exposes host state broadly | virtual, generated from world state | partial | `system::proc_self_virtual` — the covered set is listed in `PLATFORMS.md` §4 |
| memory-mapped files | writes bypass syscalls entirely | pages of mapped files are backed by the virtual disk; writes are visible at `msync`/`munmap`/unmapping | partial | `integration::mmap_write_visible_after_msync` |
| `fallocate`, `sync_file_range`, `chmod` and kin | allocation and modes are the host filesystem's | answered from the world's own file: `fallocate` grows it and a crash treats the reserved bytes as written; modes are held and never consulted | solved | `bohr-world` `minimal` tests |
| `statx`, `prctl` | the host's file and process state | answered from the world: `statx` as `fstat` is, `prctl` for the names and settings a runtime asks about | solved | `bohr-world` `minimal` tests |
| `inotify` | a host facility watching the host's files | answered `ENOSYS`, on purpose: the programs that use it fall back to polling | by design | `bohr check geth`, `walls` |

`mmap`-based I/O is `partial` and it is the most likely source of a surprise for embedded databases,
which is precisely the first hunting target list.

### 2.7 Network

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| real packets, real latency, real loss | the network is the definition of nondeterministic | a filter that traces by default, so no network call reaches the host; a fully in-memory virtual network | solved | `integration::five_node_cluster` |
| delivery order | reorders in reality | modelled; ties broken by `(tick, node, seq)` never by insertion order | solved | `world::delivery_tie_break` |
| **one-directional partitions** | A sees B, B does not see A | first-class in the partition model | solved | `integration::directional_partition` |
| ephemeral port assignment | kernel-chosen | assigned by us in a defined sequence | solved | `system::ports_virtualised` |
| DNS | resolver state, network access | a static hosts table from configuration | solved | `system::dns_from_config` |
| readiness (`epoll`, `poll`, `eventfd`) | the kernel's wait queues | the world's own readiness: level- and edge-triggered and one-shot watches, an `eventfd` write waking its pollers, all ordered | solved | golden `epoll` workload, `bohr check golden` |
| pipes | a kernel buffer between two descriptors | in a world with a `[cluster]`, a stream pair: a read blocks on empty, a write wakes it, a closed write end reads as the end; the direction is not enforced | partial | golden `pipe` workload |
| TCP behaviour (Nagle, window scaling, retransmits) | kernel stack details | a simplified stream model preserving ordering and framing, not the RFC | partial | documented; a guest depending on precise TCP timing is out of scope |
| `SO_REUSEPORT` load balancing | kernel hashes connections across listeners | our own defined distribution, drawn from the tape | solved | — |

### 2.8 Everything else

| Source | How it leaks | v1 mechanism | Status | Proof |
|---|---|---|:---:|---|
| `uname`, `/proc/version`, kernel release strings | guests branch on kernel version | fixed presented release (`5.15.0-bohr`) | solved | `system::uname_fixed` |
| `getpid`-derived seeds, `gettid`-derived sharding | very common idiom | covered by pid virtualisation | solved | — |
| signals from the host (`SIGWINCH`, job control) | arrive at arbitrary moments | the guest is isolated; signals we did not generate are not delivered | solved | `system::host_signals_not_delivered` |
| signal delivery timing generally | the kernel picks a moment | delivery points are ours and are drawn | solved | `integration::signal_delivery_point_from_tape` |
| `io_uring` | an asynchronous syscall path whose work leaves the guest through shared memory | by default **declined** at `io_uring_setup` with `ENOSYS`, and entering or registering a ring **refused** with `BOHR-2004`; under `[linux] io_uring = "ring"` a ring is opened, its poller always asleep, and every submission is read at `io_uring_enter`, carried out in queue order as its plain call, and completed before the call returns | full for the seven file operations under `"ring"`; partial by default — the guest runs its fallback, and each run records that | `determinism::io_uring_is_declined_at_the_door_and_refused_past_it`; `ring::a_ring_repeats`; `ring::a_sync_through_the_ring_meets_the_disk_model` |
| `vfork`, `clone3` exotic flag combinations | affect address-space sharing | supported combinations are enumerated; others fail with `BOHR-2002` | partial | `PLATFORMS.md` §4 |
| `ptrace` used *by* the guest | nested tracing | refused with a named error | deferred | — |
| direct device access, DMA, `/dev/mem` | below our level entirely | refused | **honest hole** — hypervisor only | — |
| hardware performance counters read by the guest | real counts | refused or emulated from progress units | partial | — |
| `madvise(MADV_FREE)` and lazy page reclaim | the kernel decides when | emulated as immediate | solved | — |
| the guest's own use of uninitialised memory | its values depend on what was there before | **by design not our problem** — but it produces divergence, so we detect it via repeat-and-compare and report it as a *guest bug*, which is a finding, not a Bohr failure | by design | `determinism::repeat_1000` catches it |

The last row inverts the usual reading. Uninitialised-memory use in the guest makes the digests
diverge, and the response is not to work around it but to report "the program is nondeterministic
here, at this event" — a genuine bug in the guest rather than a failure of the tool.

---

## 3. What the map means

**Syscall-level interception is sufficient for the overwhelming majority of server software.** Of
the rows above, exactly three are honest holes at this backend level: `RDRAND`/`RDSEED`, direct
device access, and true multi-core execution. The first is mitigated to the point where most
libraries never reach it; the second and third do not occur in the software we are hunting in.

Full determinism requires a hypervisor, which is version 2,
and the architecture is built for it in advance: the backend
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
