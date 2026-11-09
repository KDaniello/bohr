# Platforms and host requirements

Living document. It states exactly what a machine must provide to run Bohr, what a guest program
must look like to be supported, which syscalls are handled, and which environments are known to be
hostile. Everything here is verified by `bohr doctor` and by CI, not asserted in prose.

Neighbouring documents:
[`ARCHITECTURE.md`](./ARCHITECTURE.md) ·
[`API.md`](./API.md) ·
[`DETERMINISM.md`](./DETERMINISM.md) ·
[`TESTING.md`](./TESTING.md).

---

## 1. Supported hosts

| | v1 |
|---|---|
| Operating system | Linux |
| Architecture | x86-64 |
| Minimum kernel | **5.4** |
| Recommended kernel | 5.15 or newer |
| libc | any; the supervisor ships as a static musl binary |
| Privileges | an ordinary user. **Root is never required and never requested.** |

Everything else — macOS, Windows, aarch64, 32-bit — is out of scope for v1
([`ARCHITECTURE.md`](./ARCHITECTURE.md) §13). The layers above the backend build and test on any OS
against `bohr-backend-null` ([`TESTING.md`](./TESTING.md) P2), so development is not Linux-only even
though the product is.

**Why 5.4 and not older.** It is the oldest kernel that has every feature in §2 with the semantics
we rely on, and it is the floor of the distributions our target users actually run. Supporting older
kernels means conditional interception paths, and conditional interception is how determinism
becomes environment-dependent.

---

## 2. Required kernel features

`bohr doctor` checks each one and prints the exact remedy on failure.

| Feature | Used for | Kernel | Failure |
|---|---|---|---|
| `ptrace` with `PTRACE_SEIZE`, `PTRACE_GET_SYSCALL_INFO` | the interception loop | 4.3 / 5.3 | `BOHR-3001` |
| `seccomp` with `SECCOMP_RET_TRACE` and `SECCOMP_FILTER_FLAG_TSYNC` | the syscall pre-filter (§5.6) | 3.5 / 3.17 | `BOHR-3003` |
| `arch_prctl(ARCH_SET_CPUID, 0)` | trapping `CPUID` | 4.12 **and** CPUID faulting exposed by the CPU | `BOHR-3004`, run marked degraded |
| `prctl(PR_SET_TSC, PR_TSC_SIGSEGV)` | trapping `RDTSC` | 2.6 | `BOHR-3004` |
| `personality(ADDR_NO_RANDOMIZE)` | disabling ASLR | ancient | `BOHR-3004` |
| user namespaces, unprivileged | isolating mount, uts, ipc | 3.8 | run marked degraded |
| mount namespace | private mount propagation | 3.8 | run marked degraded |
| `sched_setaffinity` | pinning to one core | ancient | warn only |
| `process_vm_readv` / `process_vm_writev` | fast guest memory access | 3.2 | falls back to `PTRACE_PEEKDATA`, ~20× slower, warns |
| `perf_event_open` with hardware branch counters | fine-grained progress measurement (§5.5) | 3.4 | `BOHR-2005`, degrades to syscall granularity |

### 2.1 CPUID faulting is a hardware capability, not a kernel version

`arch_prctl(ARCH_SET_CPUID, 0)` needs `X86_FEATURE_CPUID_FAULT`, which the kernel reads from a
model-specific register. Three things must hold at once, and the call returns `ENODEV` if any of
them does not:

| Requirement | Detail |
|---|---|
| the processor supports it | Intel from Ivy Bridge; AMD from Linux 6.17, which is where the kernel gained AMD support |
| the kernel is new enough | 4.12 on Intel, 6.17 on AMD |
| the platform exposes the register | a hypervisor that does not pass it through hides the feature from an otherwise capable CPU |

`grep cpuid_fault /proc/cpuinfo` answers all three at once.

Where it is absent, the guest sees the real processor. The consequence splits in two:

| | Effect |
|---|---|
| Same machine, same seed | unaffected — the host answers `CPUID` identically every run, so runs still reproduce |
| **Different machines** | **broken** — two hosts answer differently, so a bundle does not replay across them |

The backend therefore reports `traps_cpuid: false`, the run is recorded as
`determinism: degraded` naming this cause, and cross-machine reproduction is not claimed for it.
Nothing is silently downgraded: a degraded run never reports as clean.

Known to be absent under Hyper-V guests, which includes WSL2 and every container built on it.

### 2.2 Host settings that block us

These are settings, not features. We report them and print the inspection command; **we never change
a host setting ourselves.**

| Setting | Required | Symptom if wrong |
|---|---|---|
| `kernel.yama.ptrace_scope` | `0` or `1` (1 is fine: we are the parent) | `ptrace` denied, `BOHR-3001` |
| `kernel.unprivileged_userns_clone` | `1` where the distribution has it (Debian derivatives) | namespace creation fails, `BOHR-3002` |
| `user.max_user_namespaces` | `> 0` | same |
| `kernel.perf_event_paranoid` | `≤ 2` for branch counters | `BOHR-2005`, degraded progress measurement |
| `/proc/sys/vm/overcommit_memory` | any | none; noted because guests sometimes depend on it |

---

## 3. Environments

| Environment | Status | Notes |
|---|:---:|---|
| bare metal, ordinary distribution | **supported** | the reference environment |
| a virtual machine (KVM, VMware, Hyper-V) | **supported** | branch counters are often unavailable; expect degraded progress measurement |
| Docker, default profile | **needs flags** | the default seccomp profile blocks `ptrace`. Run with `--cap-add=SYS_PTRACE --security-opt seccomp=unconfined` |
| Docker, our published image | **supported** | ships the profile and documents the flags |
| GitHub Actions `ubuntu-latest` | **supported** | verified by CI; no branch counters, so L4 runs at syscall granularity |
| Kubernetes pods | **needs config** | `SYS_PTRACE` capability plus a relaxed seccomp profile; many clusters forbid both |
| WSL2 | **partial** | works for L0–L3; `perf_event_open` and some namespace configurations are unreliable. Not a supported target, and `doctor` says so |
| gVisor, Firecracker guests | **unsupported** | the syscall implementations differ from the kernel we model against |
| nested under another `ptrace` supervisor | **unsupported** | refused with a named error |

The Docker row matters more than it looks: the most common first contact with a tool like ours is
`docker run` in someone's CI, and a first contact that fails with "Operation not permitted" is a
user we never hear from again. The published image and the two flags are documented in the README,
not buried here.

---

## 4. Guest support matrix

### 4.1 Binaries

| | Status |
|---|:---:|
| dynamically linked ELF, glibc | supported, with the libraries its loader opens in the prepared state (`[state]`, [`API.md`](./API.md) §5.7): the loader is mapped by the host kernel, and every later `openat` is answered from the world |
| dynamically linked ELF, musl | supported, on the same terms as glibc |
| statically linked ELF | supported |
| PIE and non-PIE | supported |
| `setuid` binaries | refused — we cannot trace them without privilege, and doing so would be a security claim we do not make |
| interpreted programs (`#!` scripts) | supported; the interpreter is the guest |
| JIT-compiled runtimes (JVM, .NET, V8) | **partial** — they work, but they use `RDRAND`, `mmap`-heavy layouts and their own thread pools. Expect degraded determinism until measured per runtime |
| programs using `io_uring` | **supported for files under `[linux] io_uring = "ring"`; through their fallback otherwise.** With the key, a ring carries `READ`, `WRITE`, `OPENAT`, `CLOSE`, `STATX`, `FSYNC` and `UNLINKAT`, each by the route of its plain call, and refuses any other operation under `BOHR-2004` ([API](./API.md) §5.1). Without it, opening a ring is answered `ENOSYS`, as a kernel built without the option answers, and the run is not the one production takes — every such run says so on its own timeline ([`DETERMINISM.md`](./DETERMINISM.md) §2.8). `bohr doctor --target` warns where the name appears in the binary; `bohr survey` says whether it is called |
| programs that `ptrace` others | refused |
| killing a node that has more than one task | supported — the reproducer that defined this row no longer reproduces; see below |

**Killing a node that has more than one task, and what happened to the wall.** Several threads on
one node reading one socket is the ordinary shape of a service, and the socket half is answered:
each parks in turn and a delivery releases the longest-waiting. Killing such a node used to stall
the run — the supervisor blocked in its wait, one guest task already stopped and waiting to be
resumed, and the killed node's remaining thread a zombie whose death the supervisor never heard
about — and this row said so.

**The reproducer that defined the row no longer reproduces.** `examples/mdbx-hunt/scenario.toml`
was named here because it stalled with its two `[[fault]]` tables and ran clean without them. It
now runs clean with them: ten seeds, five tasks, a kill and a restart, every one completing and
answering its questions. Recorded in `benches/baseline.toml` under `[m4_2.multi_task_kill]`.

What that measurement is and is not. It is one scenario, so the honest statement is that the shape
this row was written from is gone rather than that every shape is. The narrower ones were always
covered — one task per node killed and restarted as often as a scenario likes, threads with no
kill, and a thread created and joined around either — and a real execution client, which is
thread-heavy, has since been killed several hundred times across campaigns without stalling.

Nothing here was fixed on purpose, which is the part worth writing down: the work that closed it
was elsewhere, and a limitation nobody re-measures outlives the defect behind it. This row is
re-measured when a campaign against a threaded target changes.

**Building a guest so a search can see inside it.** Everything above runs unmodified. Coverage is
the one thing that needs a rebuild, because no binary reports its own edges:

```
clang  -fsanitize-coverage=trace-pc-guard  ... your.c sdk/c/bohr-cov-rt.c
gcc    -fsanitize-coverage=trace-pc        ... your.c sdk/c/bohr-cov-rt.c
```

Clang names each edge with a guard variable; GCC offers only the plain form and the runtime takes
the edge from the return address instead. Both are reproducible here, because a guest already runs
statically linked with address randomisation off — the conditions that make a run repeat are the
same ones that make its map repeat.

**The runtime is compiled without the instrumentation it implements.** Built with it, the function
that records an edge records the edges inside itself and the first one never returns.

A guest built without any of this is still searched — by the schedule and by fault targeting,
neither of which needs coverage — and is reported as searched without it. That is a different
statement from a guest that covered nothing, and the two are never merged.

### 4.2 Syscall support

The table lives in code, in `crates/bohr-backend-ptrace/src/linux/syscalls.rs`, and generates both
the seccomp filter and the handler dispatch — the two cannot disagree
([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.6). This section describes the classes; `bohr doctor
--syscalls` prints the current table.

| Class | Handling | Examples |
|---|---|---|
| **emulated** | never reaches the host; answered from the world | `clock_gettime`, `getrandom`, `read`, `write`, `pread64`, `pwrite64`, `openat`, `ftruncate`, `fsync`, `futex`, `nanosleep`, `getpid`, `gettid`, `uname`, `sendto`, `recvfrom` |
| **virtualised** | translated, then executed against a private host resource | anonymous `mmap`, `mprotect`, `brk` |
| **admitted** | passed through because it cannot affect determinism | `mprotect`, `munmap`, `mremap`, `brk`, the signal mask, and `clone`, whose new task is announced by a trace event instead |
| **admitted by argument** | passed through only in the shapes that cannot reach a file | `mmap` alone: an anonymous mapping, and this engine's own coverage channel. Any other mapping names a descriptor rather than a page, so it is traced and refused |
| **refused** | `BOHR-2001` or a specific code | `io_uring_enter` and `io_uring_register` under `[linux] io_uring = "absent"`, `ptrace`, `kexec_load`, `bpf` |
| **unknown** | `BOHR-2001`, naming the syscall number and the task | anything not in the table |

**A lock whose word names its holder** — `FUTEX_LOCK_PI`, `FUTEX_TRYLOCK_PI`, `FUTEX_UNLOCK_PI` —
is answered, and the priority inheritance the kernel names it after is not modelled at all: there
are no priorities here and one task runs at a time. What is modelled is the half a guest can see —
the word holds an owner, a second thread queues behind it, and the lock passes from one to the next
inside the unlock rather than being dropped for them to race over. A store that keeps a mutex of
this kind in a file it maps needs exactly that. An operation outside the answered set returns
`EINVAL` and is named on the timeline, because a caller's own message for it says nothing about
which one: the abort happens inside its C library, several frames from anything that mentions a
futex.

**Readiness** — `epoll_create1`, `epoll_ctl`, `epoll_wait`, `epoll_pwait`, `eventfd2` — is answered
by the cluster model rather than by a node's own world, because a readiness set spans a node's
files and its sockets both and only the cluster sees them together. A single-node run without a
scenario has no sockets to be ready, and refuses them as before. Every asynchronous runtime is
built on this family; `tokio` 1.53 runs on it.

**Sockets** come in two kinds and the model treats them as differently as their transports do.
Datagrams — `sendto`, `recvfrom` — are drawn upon four times and may be lost, doubled, overtaken or
late. Streams — `listen`, `accept`, `accept4`, `connect`, `getpeername`, and `read`/`write` on what
they produce — are drawn upon not at all, because the transport they stand for retransmits until
those four are gone. The one adversity that reaches a stream is that the far end is unreachable: a
partition holds its bytes until the heal, and a closed, shut-down or killed peer leaves it reading
end of stream. Semantics are normative in [`API.md`](./API.md) §5.3, including what is deliberately
not modelled.

**Unknown is a hard failure, never a pass-through.** This is §5.7 and it is the decision that
distinguishes us from Hermit's failure mode. The scope discipline that makes it survivable: the
table grows only when a real hunting target needs an entry, prioritised by a "seen but unhandled"
report produced up front rather than one syscall per run.

That report is **`bohr survey -- <program>`**. It answers every call this build does not model with
`ENOSYS` so the next one can be reached, and prints the whole set with a count each. It is not a
run and cannot be confused with one: its worlds carry an identity of their own, so a bundle claiming
one is refused before it replays; it writes nothing; and it exits 4 when it found anything, because
what it found is a fact about this build rather than about the guest.

It collects **two kinds of wall**, and names which is which, because the fix differs:

| Wall | What it means | What it needs |
|---|---|---|
| `nothing models it` | no row in the table; an ordinary run refuses it by name | a row, and a model behind it |
| `answered ENOSYS` | a row exists and the model deliberately says it cannot, so the caller takes the fallback it keeps for an older kernel | a model; the row is already there |

The second kind is collected only by a survey — an ordinary run answers it silently, because the
fallback is the point of answering rather than refusing. But from the guest's side the two stop it
in the same place, and a survey that named only the first would let a program that got nowhere look
surveyed clean.

**What a survey is not evidence of**, printed with every report and repeated here because it is the
part that misleads. A call answered `ENOSYS` sends the guest down its fallback path, which is not
the path it would have taken — a program refused `io_uring_setup` uses `epoll` instead and never
issues `io_uring_enter` at all. And a call the run never reached is not a call the program cannot
make. The set is a lower bound twice over, and the way to raise confidence in it is more seeds and
longer runs rather than a stronger reading of one.

### 4.3 The filesystem a guest starts on

Held in the supervisor and never on the host: a run has to begin from the same state every time,
and a file left over from the last run is state. What is there at the start is the root and the
directories of the hierarchy standard — `/bin`, `/dev`, `/etc`, `/home`, `/lib`, `/opt`, `/proc`,
`/root`, `/run`, `/sbin`, `/srv`, `/sys`, `/tmp`, `/usr`, `/var`, `/var/tmp` — all empty. They are
there because every filesystem a program is ever started on has them, and a world that made a guest
create `/tmp` before writing to it would refuse an open that succeeds everywhere.

**A directory is a name, not a container.** It can be made, asked about, opened and synced, and a
file cannot be created where its directory does not exist. What it is not is a step in a walk:
resolution is by the whole path, so there are no links, no `..`, and no working directory to be
relative to. `getdents64` answers empty, whatever is there.

`/dev/urandom` and `/dev/random` resolve and read as the endless draw. **Nothing under `/proc` or
`/sys` exists** — an open answers `ENOENT`, which is behaviour a portable program already handles,
and a program that needs one of them takes whatever path it keeps for a kernel that does not offer
it. `statfs` and `fstatfs` report one device with ext4's identity, the block size every file's
`stat` reports, and free space derived from what the world actually holds.

**A mapping of a file is modelled**, because a store keeps its pages that way and reading one
through a window is not the same as reading it. An anonymous mapping is admitted and never seen
here, being backed by nothing — the test is on `MAP_ANONYMOUS` rather than on the descriptor,
because a caller may name any descriptor it likes once the flag is set.

The call is *replaced* rather than answered: the guest is given anonymous memory by the host,
because an address space belongs to the process and the only way to grow one is to let it ask, and
the world writes the file into that memory when the call comes back. From there the two copies are
kept in step, and which way the truth runs depends on what the mapping was for:

| | a mapping that only reads | a mapping that writes |
|---|---|---|
| the file is the truth | yes | no — the mapping is |
| refreshed when | the file changes under it | never; it is what changes |
| carried back when | never | the file is read, and at `msync` |
| durable when | it always was | `msync`, which takes the same path `fsync` does |

`msync` is a durability point and is drawn upon exactly as a sync is — the same failure, the same
stall. A guest whose only writes go through a mapping can therefore lose them to a crash, which is
the whole reason to model this rather than wave it through. A single `msync` over mappings of more
than one file is refused: a sync is one operation with one fate, and drawing once per mapping would
let live state decide tape layout.

Two things it does not do. **A mapping is made writable whatever protection was asked for**, because
the supervisor writes it through the same page protections the guest does and a mapping it could not
write is one it could not keep truthful — so a guest that writes through a mapping it declared
read-only is not faulted here, where the kernel would fault it. That is a defect this model does not
catch, never one it invents. And `mremap` is admitted, so a *file* mapping moved by one is lost
track of; nothing seen so far moves one.

---

## 5. Performance expectations

Slowdown versus native, measured as a ratio on the same machine in the same job
([`TESTING.md`](./TESTING.md) §11). Goal T3 is ≤ 5×; the reference point is Hermit at 3–6×.

| Workload shape | Expected | Why |
|---|---:|---|
| compute-bound, few syscalls | 1.2–2× | almost everything is filtered by seccomp; we pay only for switch points |
| an embedded database test suite | 3–4× | measured on `redb`: 20 000 intercepted calls against 0.36 s of work |
| balanced server workload | 3–5× | the target we are optimising for |
| syscall storm (a tight `getpid` loop) | 400–600× | every call is a stop, and a native `getpid` is 70 ns; this is a floor on interception, not a workload |
| thread-heavy with high contention | 4–8× | serialisation plus a switch decision per synchronisation point |
| with branch-counter progress measurement | +10–20% | one `perf_event_open` read per switch point |
| with coverage collection | +5–15% | shared-memory writes in the guest |

The **intercepted-call count** is reported in every run summary, and a run that hung also lists the
calls it made most. Interception is what a run pays for, so a jump in that count is the fastest
diagnosis of an unexpected slowdown: a syscall moved from admitted to traced, and that is a change
in the table rather than in the machine.

---

## 6. CI matrix

| Runner | Kernel | CPU | Purpose |
|---|---|---|---|
| `ubuntu-24.04` | 6.x | Intel | primary; every commit, L0–L4 |
| `ubuntu-22.04` | 5.15 | Intel | minimum recommended kernel |
| container on kernel 5.4 | 5.4 | Intel | the declared floor; proves §1 is true |
| `ubuntu-24.04-arm` | 6.x | ARM | **L0–L2 only** — proves the sans-io core is portable, per `TESTING.md` P2 |
| `macos-14` | — | ARM | L0–L2 only, same purpose |
| `windows-2022` | — | x86-64 | L0–L2 only, same purpose |
| self-hosted AMD | 6.x | AMD Zen | **not provisioned**, so cross-CPU replay against the Intel runner ([`TESTING.md`](./TESTING.md) §7.3) is unchecked |

The three non-Linux rows enforce a design rule rather than promise a platform: a change that puts a
syscall in `bohr-kernel` turns those jobs red.

---

## 7. Distribution artefacts

| Target | Form | Notes |
|---|---|---|
| `x86_64-unknown-linux-musl` | static binary | the one referenced in documentation; works in minimal CI containers |
| `x86_64-unknown-linux-gnu` | dynamic binary | for hosts where a static build is discouraged |
| container image | `ghcr.io/kdaniello/bohr` | carries the seccomp profile and documents the required flags |
| install script | `bohrup` | downloads, verifies `SHA256SUMS`, installs to `~/.bohr/bin` |

Reproducible builds ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §12): a tool that promises
reproducibility and cannot reproduce its own build has an obvious credibility problem, and the
question will be asked on the first release thread.

---

## 8. Adding a platform

The bar, in order:

1. `bohr doctor` reports a clean pass on the platform;
2. the L3 and L4 suites pass on a CI runner of that platform, not on a laptop;
3. cross-platform replay works in both directions against an existing supported platform
   ([`TESTING.md`](./TESTING.md) §7.3);
4. the performance table in §5 is measured, not copied;
5. this document and the README support matrix are updated in the same change.

Steps 3 and 4 are the ones that get skipped, and they are the ones that matter: a platform where
findings do not cross-replay is a platform that fragments the user base into people who cannot
exchange bug reports.
