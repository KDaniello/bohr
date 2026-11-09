# Architecture

What Bohr is made of, which entities it operates on, where the boundaries between its components
run, and the rules its code is written by.

Neighbouring documents:
[`API.md`](./API.md) — contracts, formats and data paths ·
[`TESTING.md`](./TESTING.md) — the test pyramid ·
[`DETERMINISM.md`](./DETERMINISM.md) — the nondeterminism source map ·
[`PLATFORMS.md`](./PLATFORMS.md) — host requirements and the syscall support matrix.

---

## 0. Name and namespaces

The project, the binary and the crate prefix are all `bohr`. Every other surface is derived from it:

| Surface | Value |
|---|---|
| Rust crates | `bohr-types`, `bohr-tape`, `bohr-kernel`, … |
| Binary | `bohr` (a single multi-command executable, `bohr run` / `explore` / `replay` / `reduce`) |
| C ABI, symbol prefix | `bohr_` (`bohr_assert_always`, `bohr_random_bytes`) |
| C header | `bohr_sdk.h` |
| Rust SDK | crate `bohr-sdk` |
| Python SDK | distribution `bohr-sdk`, module **`bohr_sdk`** — the top-level name `bohr` is taken on PyPI by an unrelated package, see §0.1 |
| .NET SDK | package `Bohr.Sdk`, namespace `Bohr` |
| Go SDK | module `github.com/bohr-dev/bohr-sdk-go`, package `bohr` |
| Environment | `BOHR_*` (`BOHR_SEED`, `BOHR_OUT`, `BOHR_SDK_OUTPUT`) |
| File extensions | `.tape` — raw entropy tape, `.bohr` — self-contained scenario bundle |
| Error codes | `BOHR-####`, a flat stable catalog ([`API.md`](./API.md) §8) |
| Domain | `bohr.sh` — registered at launch, before v1.0, the one public release |

The symbol prefix is `bohr_` and not `b_`: SDK symbols land in the global namespace of a customer
process next to their own symbols and dozens of third-party libraries. The name is short enough to
type and specific enough not to collide.

### 0.1 Names occupied elsewhere

Two of the surfaces the naming scheme would otherwise use belong to unrelated projects.

| Surface | State |
|---|---|
| **PyPI top-level module `bohr`** | **occupied** — an unrelated distribution `bohr` installs a top-level `bohr/` package |
| **`bohr.dev`** | **occupied** — a live personal site |
| crates.io `bohr`, `bohr-sdk`, `bohr-cli` · NuGet `Bohr`, `Bohr.Sdk` · PyPI `bohr-sdk` · GitHub organisation `bohr-dev` | available |

**The Python module is therefore `bohr_sdk`.** Two distributions installing a top-level `bohr` break
`import bohr` depending on `sys.path` order, which is the "version conflict inside a customer's
build" that §9 rule S3 exists to prevent. Naming a module after its distribution is the ordinary
Python convention, and examples read `import bohr_sdk as bohr`.

**The domain is `bohr.sh`.** `.sh` is the zone the audience associates with systems tooling, and all
project addresses (`security@`, `conduct@`) follow it.

Everything else — the project name, the binary, the crate prefix, the C symbol prefix, the .NET
namespace — is unaffected and stays `bohr`.

---

## 1. Languages and their role

| Language | Role | Why |
|---|---|---|
| **Rust** | Everything that contains logic: the supervisor binary, the determinism backends, the kernel (clock, scheduler, virtual world), the explorer, the reducer, the report generator | Three reasons at once. (1) This is a systems tool: it drives `ptrace`, writes seccomp-BPF programs, parses ELF and pokes another process's memory — a GC pause in the supervisor is a nondeterminism source in the thing whose whole purpose is determinism. (2) The audience we are selling to (databases, consensus implementations, infrastructure) already lives in Rust; a tool written in their language is a tool they will read and patch. (3) The supervisor is trusted with a customer's build; memory safety in the trusted component is not a luxury. |
| **C** | Test targets under `testdata/`, the SDK C header, the shim that captures `RDRAND` rewrites | To prove that we handle programs we did not compile we need programs we did not write in Rust. C targets are the cheapest way to reproduce a hostile libc path exactly. |
| **Python** | SDK №2, plus report-comparison scripts in CI | The second-largest population of systems that would benefit from us are Python services and test harnesses. No Python in the runtime — only in the SDK and in CI glue. |
| **C#** | SDK №3 | Deliberate. The .NET corner is weakly held (Coyote is C#-only and barely moving) and the corporate .NET audience is large and solvent. |
| **Go** | SDK №4 | The single biggest population already annotated with the Antithesis SDK. |
| **assembly (x86-64)** | Fragments only: `RDRAND` trampolines, TSC emulation stubs, the instruction-scanner test corpus | Nothing else. Assembly carries a maintenance cost, and no more of it is taken on than the trapped instructions require. |
| **TypeScript / JS** | Not used | The HTML report is generated as static markup with inline CSS and a few hundred lines of vanilla JS. No bundler, no framework, no `node_modules`. Reason in §5.13. |

**What does not exist and will not exist in v1:** any runtime dependency on a language other than
Rust; a daemon; a database; a network service; a container runtime; Kubernetes. `bohr` is one
statically-linkable binary plus optional SDK packages.

---

## 2. Three execution contexts

This is the main division of the whole system. Everything else is a detail inside one of the three.

```
┌──────────────────────────────────────────────────────────────────────────┐
│ CONTEXT 1 · SUPERVISOR  (our process, the `bohr` binary)                 │
│                                                                          │
│   explorer ──► kernel ──► backend ──► ptrace/seccomp ──► guest tasks     │
│      ▲            │                                                      │
│      └── trace ◄──┘   reducer · report                                   │
│                                                                          │
│  Holds the entropy tape, the logical clock, the scheduler and the whole  │
│  virtual world. The only process allowed to make a nondeterministic      │
│  decision — and it makes each one by drawing from the tape.              │
└───────────────────────────────┬──────────────────────────────────────────┘
                                │ ptrace + seccomp + shared memory
               ┌────────────────┴──────────────────┐
               ▼                                   ▼
┌───────────────────────────────┐  ┌───────────────────────────────────────┐
│ CONTEXT 2 · GUEST             │  │ CONTEXT 3 · SDK (optional, in-guest)  │
│ the customer's unmodified     │  │ bohr-sdk linked into the guest        │
│ program, one or more processes│  │                                       │
│                               │  │ Writes assertion/observation records  │
│ Believes it talks to Linux,   │  │ to a file descriptor handed over by   │
│ a network and a disk. It talks│  │ the supervisor. Draws randomness from │
│ to us. Never modified, never  │  │ the tape instead of the system.       │
│ recompiled.                   │  │ Outside Bohr every call is a no-op.   │
└───────────────────────────────┘  └───────────────────────────────────────┘
```

The rule everything else grows from: **the guest never learns it is being simulated except through
the SDK, and the SDK is optional.** Two consequences we hold ourselves to:

1. Every feature must be useful with **context 3 absent**. Crashes, hangs, data corruption and
   assertion failures inside the guest are found without any annotation. The SDK adds to what is
   found; it is not a precondition. (Specification §14, the "nobody will annotate their code" risk.)
2. The supervisor never trusts the guest. A guest that returns garbage over the SDK channel
   produces a diagnostic, not a supervisor crash — the guest is exactly the thing we expect to be
   broken.

### Simulation mode versus replay mode

| | `bohr run` / `bohr explore` | `bohr replay` |
|---|---|---|
| Where the tape comes from | seed → tape, then mutation | read verbatim from the bundle |
| Scheduling policy | `random`, `pct`, `adversarial` | `replay` — decisions are re-derived from the same tape |
| Fault injection | active, chosen by the explorer | replayed exactly |
| Coverage collection | on | off by default (it perturbs nothing, but it costs) |
| Success criterion | a property broke / coverage grew | **the trace digest equals the recorded one** |

Both modes are the *same* kernel and the *same* backend. What differs is where the bytes come from
and whether the digest is compared or recorded. There are **not** two engines: a replay engine that
diverged from the exploration engine would break the guarantee both modes rest on.

---

## 3. Workspace layout

```
bohr/
├── docs/
│   ├── ARCHITECTURE.md          # this file
│   ├── API.md                   # tape ABI, backend boundary, bundle format, SDK, CLI, errors
│   ├── TESTING.md
│   ├── DETERMINISM.md           # the nondeterminism source map — the project's checklist
│   ├── PLATFORMS.md             # host requirements, kernel features, syscall support matrix
│   ├── EVIDENCE.md              # what the project claims, and the command that checks it
│   └── repo/                    # ci.md, labels.md, release.md
├── crates/
│   ├── bohr-types/              # Tick, Seed, NodeId, TaskId, Fd, EventId, digests.
│   │                            # serde + zero I/O. The only crate everyone else knows.
│   ├── bohr-tape/               # THE ENTROPY TAPE: format, cursor, typed draws, draw journal,
│   │                            # deterministic extension. No knowledge of what a draw means.
│   ├── bohr-backend/            # THE BACKEND BOUNDARY: trait DeterminismBackend, Stop, Resume,
│   │                            # BackendCapabilities. Types only, no implementation.
│   ├── bohr-backend-ptrace/     # v1 backend: seccomp-BPF + ptrace + namespaces + prctl.
│   │                            # Linux x86-64 only. The ONLY crate allowed `unsafe`.
│   ├── bohr-backend-null/       # in-process backend driving synthetic guests. Lets layers 2-4
│   │                            # be developed and tested on any OS, including Windows.
│   ├── bohr-kernel/             # logical clock · scheduler + policies · the run loop.
│   │                            # Owns the tape cursor. DEFINES the ports it drives:
│   │                            # WorldModel (§5.16), Oracle (§5.17), SchedulePolicy.
│   │                            # Depends on the backend TRAIT, never on an implementation.
│   ├── bohr-world/              # THE DISTRIBUTED-SYSTEMS WORLD MODEL: virtual network, virtual
│   │                            # disk, node lifecycle. One implementation of WorldModel among
│   │                            # future others, and the Oracle that reads what it records.
│   │                            # sans-io: no sockets, no files, no system clock.
│   ├── bohr-trace/              # event log, trace digest, .bohr bundle reader/writer
│   ├── bohr-sdk-proto/          # assertion/observation record schema, Antithesis-compatible.
│   │                            # Shared by the supervisor and the Rust SDK.
│   ├── bohr-explorer/           # corpus, tape mutators, coverage feedback, fault targeting,
│   │                            # property evaluation
│   ├── bohr-reducer/            # delta debugging over the tape and over the event list
│   ├── bohr-report/             # text, JSON and single-file static HTML output
│   ├── bohr-cli/                # the `bohr` binary: wiring, argument parsing, exit codes
│   └── bohr-testkit/            # fixtures, golden harness, determinism harness, fake guests
├── sdk/
│   ├── rust/                    # crate bohr-sdk
│   ├── python/                  # distribution bohr-sdk, module bohr_sdk
│   ├── csharp/                  # package Bohr.Sdk
│   ├── go/                      # module bohr-sdk-go
│   └── c/                       # bohr_sdk.h + a static library
├── testdata/                    # guest programs under test: C and Rust, deliberately hostile
├── benches/                     # the committed performance baseline
├── examples/                    # end-to-end scenarios shipped to users
├── .github/workflows/
├── Makefile
└── Cargo.toml                   # workspace root
```

### Crate dependency graph

Acyclic and deliberately narrow. An arrow means "knows about".

```
                              bohr-types
        ┌──────────────┬───────────┼────────────────────┐
        ▼              ▼           ▼                    ▼
   bohr-tape      bohr-trace   bohr-backend       bohr-sdk-proto
        │              │        ▲          ▲            │
        └──────┬───────┘        │          │            │
               ▼                │          │            │
          bohr-kernel      bohr-backend-  bohr-backend- │
   defines the ports:         ptrace          null      │
   WorldModel · Oracle                                  │
   SchedulePolicy                                       │
      ▲     ▲      ▲     ▲          ▲                   │
      │     │      │     └──────────┼──────┐            │
 bohr-world │  bohr-reducer         │  bohr-report ◄────┘
 (a model)  │                       │
      bohr-explorer            bohr-testkit
                            (fixtures and the
                             determinism harness)

                        everything above ────► bohr-cli
```

The direction around `bohr-kernel` is the one to note: **the kernel is depended upon by the world,
not the other way round.** It defines the ports and drives them; the distributed-systems model, the
backends, the explorer and the reducer all plug in. `bohr-cli` is the single place where one of each
is chosen and wired.

What matters here:

- **`bohr-kernel` does not depend on any backend implementation.** It depends on the `bohr-backend`
  trait crate only. The interceptor is an engine, and the engine is replaceable. Wiring happens exactly once, in
  `bohr-cli`. Checked mechanically in CI (§8, rule 14).
- **`bohr-kernel` does not depend on any world model either** (§5.16). `WorldModel`, `Oracle` and
  `SchedulePolicy` are declared in the kernel and implemented outside it. `bohr-world` is *a* model —
  the one for distributed systems — and a second domain adds a sibling crate rather than editing
  this one. Same CI check as the backend rule.
- **`bohr-tape` does not know what a draw means.** It hands out bytes and records offsets. The
  semantics ("this draw picked the next task", "this draw decided to drop a packet") live in the
  kernel and are attached to the *draw journal*, which is a run artifact — see §5.3. If the tape crate
  ever gains a `fn draw_packet_loss`, the layering has been broken.
- **`bohr-explorer` and `bohr-reducer` are siblings, not a chain.** Neither executes anything: both
  are state machines that propose work and are handed the results (§5.19). The reducer must be usable
  standalone on a bundle produced a month ago by a different explorer version.
- **`bohr-backend-null` exists for a practical reason, not a purist one.** Two engineers cannot both
  be blocked on Linux syscall work. With a null backend, layers 2–4 have full-speed unit and
  property tests on any developer machine, and CI runs them in seconds instead of minutes
  ([`TESTING.md`](./TESTING.md) §4).
- `bohr-types` depends on nothing of ours and almost nothing external (`serde`, `blake3`). It is
  compiled into the SDK path as well, where every transitive dependency is a liability inside
  someone else's build.

---

## 4. Domain model

Exact signatures, wire formats and versioning rules are in [`API.md`](./API.md). This section
defines the entities and their invariants.

### 4.1 The entropy tape — the idea everything else follows from

**The entire run is driven by one string of bytes.** Every time the system must make a
nondeterministic decision it *draws* bytes from the tape:

which task runs next · drop this packet or not · how long to delay delivery · tear this write in
half on crash or not · what to return from `getrandom` · kill node 3 now or not.

Five properties fall out of this single decision, and they are the reason it is in the foundation
from day one:

| Property | Consequence |
|---|---|
| seed → tape → run | one 64-bit number fully determines a world; cheap to store, cheap to send |
| search = tape mutation | AFL++/libFuzzer mutators apply directly; a mature body of engineering we do not write |
| coverage steers the search | the classic fuzzing loop works unmodified |
| reduction = tape minimisation | delta debugging operates on an ordinary byte list |
| reproduction = the same tape | a scenario file is tape + fingerprints + config, and nothing else |

**Structure.** A tape is a finite **prefix** followed by a deterministic **extension**:

```
tape = prefix bytes (corpus material, mutated by the explorer)  ‖  ChaCha8(seed, counter) …
```

The extension exists because a run cannot know in advance how many decisions it will make. A guest
that loops longer than expected must not fail with "tape exhausted"; it must keep drawing, and the
bytes it gets must still be a pure function of the seed. The boundary between prefix and extension
is recorded in the trace but is invisible to the drawing code.

**Draws are typed.** `draw_u64`, `draw_range(n)`, `draw_bool(p)`, `draw_into(out)`,
`draw_weighted(&weights)`. The mapping from bytes to value is a documented, frozen function
([`API.md`](./API.md) §2.4) — a change to it invalidates every stored bundle, so it is versioned.

**The draw journal is a run artifact, not part of the tape.** During a run the kernel records, for
every draw, a tuple `(tape offset, length, site id, decision)`. The journal is what makes the
reducer structure-aware and the report readable — but it is *derived*, never an input. This split is
the whole reason we get byte-level mutators and structural reduction at the same time; it is stated
as §5.3.

### 4.2 Logical time

`Tick` is a `u64` of **virtual nanoseconds**. It has no relationship to wall-clock time and is never
read from the host.

- Time advances only at defined points: a syscall boundary, a scheduler switch, an instruction
  budget exhausting, an explicit timer expiry in the virtual world.
- A guest that sleeps for an hour wakes instantly, having advanced `Tick` by 3.6e12.
- **Each node has its own clock** with a configurable offset and drift rate relative to the world
  clock. Clock skew between nodes is one of the richest bug sources in distributed systems, and
  modelling it costs us one `i64` and one rational per node.
- Intra-thread progress, where a finer measure than "one syscall" is needed, is measured in retired
  conditional branches read from a hardware performance counter — the technique `rr` uses. This is a
  *measurement* of progress, not a source of time: the counter value is turned into a deterministic
  tick delta by a frozen formula, and if the counter is unavailable we fall back to a syscall count
  and say so in the run header.

### 4.3 Worlds, nodes and tasks

- **`World`** — everything the run contains: the set of nodes, the virtual network, the world clock,
  the tape cursor. Exactly one per run.
- **`Node`** — a simulated machine. Owns a virtual disk, a network identity, its own clock and a
  lifecycle (`Running`, `Frozen`, `Dead`, `Restarting`). A node maps to one or more guest OS
  processes.
- **`Task`** — a unit of execution the scheduler picks: one guest thread. `TaskId` is *virtual* and
  assigned by us in creation order; the host tid is host-specific and never enters the trace.
- **`Fd`** — a virtual file descriptor. The guest's numeric fd is translated at the boundary. A
  guest fd number is only stable because we make it stable.

Virtual identifiers exist for one reason: **any host-derived number that reaches the trace destroys
cross-machine reproducibility** (goal T4 — the same bundle must reproduce on a different machine).
Pids, tids, inode numbers, addresses and fd numbers are therefore all virtualised, and there is a
CI check that no raw host identifier is serialised into a trace.

### 4.4 Events and the trace

- **`Event`** — one recorded fact: a syscall entered or returned, a scheduling decision, a packet
  sent or delivered or dropped, a fault fired, an SDK assertion evaluated, a node died. Carries
  `EventId` (a monotonic run-local counter), `Tick`, `NodeId`, `TaskId` and a typed payload.
- **`Trace`** — the ordered event stream of a run. Append-only, streamed to disk, never held whole
  in memory: a run can produce tens of millions of events and the supervisor must not be the thing
  that runs out of RAM.
- **`TraceDigest`** — `blake3` over the *canonical* projection of the event stream. The canonical
  projection deliberately excludes everything host-specific (durations, host pids, addresses of
  host origin). **The digest is the definition of "the same run".** Goal T1 — 1000 runs of one seed
  produce a byte-identical trace — is mechanically checked as "1000 identical digests", not by
  diffing files.

**A run keeps its tape and not its trace.** A trace is a pure function of the tape, the
configuration and the binaries, so storing one is storing a cache of a deterministic computation. An
ordinary run leaves its tape, its coverage map and its counters — kilobytes. A finding leaves a
whole bundle. A run somebody wants to look at afterwards is regenerated by replaying it. The search
itself never reads a trace: it is guided by the coverage map, the finding class and a few counters.

### 4.5 Faults

A **`Fault`** is a deliberate hostility applied by the world at a point chosen from the tape.
The catalog is closed; adding a variant is a bundle-format minor version.

| Family | Variants |
|---|---|
| Network | delay, drop, duplicate, reorder, partition (including **one-directional**), grey degradation |
| Disk | tear a write, stall a sync, `ENOSPC`, `EIO`, lose unsynced data on crash |
| Node | kill (`SIGKILL` equivalent), freeze/thaw (GC pause, CPU starvation), restart clean, restart from an older disk snapshot, rewind a set of nodes to a common ancestor |

One-directional partition is called out explicitly because it is the case teams most often fail to
test and the one that breaks consensus implementations most reliably.

**Clocks are skewed from the start and faulted during the run.** Every node draws an offset and a
drift rate before anything runs (§4.2), so two nodes never agree on the time. During the run a
`clock_step` moves a node's wall clock — back as well as forward, as `settimeofday` does — and a
`clock_drift` changes how fast its clock runs from that moment on. A node has two readings: wall
time (`CLOCK_REALTIME`, `gettimeofday`, `time`), which a step moves, and monotonic time, which never
goes back. A defect that trusts wall time to measure elapsed time is reachable only through the step,
which is why both readings exist.

**A catalogue is a promise, and it is held to the code.** Each model declares the faults it can
inject, and `scripts/lint-rules.sh` compares that declaration against every fault the model actually
narrates. The two are separate lists that nothing at run time reads, so they drift; a campaign plans
its coverage from the declaration, which is what makes the drift cost something.

**Fault targeting.** Faults are not injected uniformly at random. The explorer prefers moments that
are *semantically dangerous* — immediately after a write and before its acknowledgement, during a
leadership handover, in the middle of a two-phase commit. Those moments are identified from SDK
`fault_point` calls when available, and from syscall-shape heuristics when not
([`API.md`](./API.md) §5.4).

### 4.6 Properties

Properties come from the SDK (context 3) and from the supervisor itself.

| Property | Meaning | Violated when |
|---|---|---|
| `always(cond, name)` | must hold every time it is evaluated | any evaluation is false |
| `always_or_unreachable(cond, name)` | holds whenever reached | reached and false |
| `sometimes(cond, name)` | must be true at least once **across the whole campaign** | the campaign ends with zero true evaluations |
| `reachable(name)` | must be hit at least once across the campaign | never hit |
| `unreachable(name)` | must never be hit | hit once |

`sometimes` and `reachable` are **campaign-level**, not run-level, and that difference is the reason
they are valuable. They do not find crashes; they produce a **map of what was never tested**
("across ten million runs we never once reached a leader change with a full disk"). That map is what
turns the tool from a bug finder into a coverage argument, and it is the half of the SDK teams
underestimate.

They are aggregated across a campaign by a mergeable counter in the corpus directory: per property,
`{evaluations, true_count, first_true_run_id}`, merged by adding the counts and taking the earlier
run. Addition is what makes the answer independent of the order workers finish in, which is what
lets `-j 1` and `-j 8` agree (§6.2). The counter lives in the corpus because the corpus is what a
campaign carries forward when it is resumed or moved — and because there is nothing to recompute
from: a satisfied `sometimes` produces no finding and an unsatisfied one produces nothing at all.

Supervisor-side properties need no annotation at all: guest crash, guest hang (no progress within a
tick budget), unsupported syscall, memory corruption reported by the guest's own sanitizers, and
non-zero exit.

### 4.7 The scenario bundle

A `.bohr` file is the product. It is **self-contained** and **machine-independent**:

manifest (format version, tool version, config, seed) · the tape prefix · binary fingerprints for
every executable and shared object the guest mapped · the host profile that was *presented* to the
guest (fixed CPUID leaves, kernel version string) · the event log · SDK records · the draw journal ·
the coverage map.

The fingerprints exist so that `bohr replay` on a different machine either reproduces exactly or
refuses with a precise reason ("libc.so.6 differs: recorded `blake3:ab12…`, found `blake3:cd34…`").
Silently replaying against a different binary and reporting "not reproducible" would be the single
most damaging failure mode this tool has, because it would teach users not to trust it.

---

## 5. Invariants

Twenty properties that hold everywhere in the system, and which everything else is built on top of.
Each states what is the case, what it costs the system to be that way, and — where a machine can
check it — where the check lives. They are cited from source files by number (`§5.7`).

### 5.1. The tape is the only source of nondeterminism

No component makes a nondeterministic decision by any means other than drawing from the tape. There
is no `rand::thread_rng()`, no `Instant::now()`, no `SystemTime::now()`, no `HashMap` iteration
order in any path that affects a decision, and no `std::thread` scheduling in the decision loop.

Every decision site is threaded to a cursor, which means `&mut TapeCursor` travels a long way down.
An ambient generator would remove that inconvenience and would also remove determinism without any
call site changing.

**Enforced by** `clippy.toml`, which denies the disallowed methods and types at compile time, and by
`scripts/lint-rules.sh` for the rest (§8, rule 14).

### 5.2. The backend boundary does not leak

`bohr-backend` speaks in terms of guest operations: "task T stopped at syscall entry with these
arguments", "resume T with this return value substituted", "T trapped on `CPUID`". It never mentions
ptrace, `waitpid`, seccomp, signals or `/proc`. Nothing above the boundary matches on which backend
is in use.

Some backend-specific efficiencies are therefore unreachable through the interface — a fused "peek
registers and memory in one stop", for instance. Where a measurement shows one matters, the boundary
grows a capability-guarded batch operation rather than a leak.

**Enforced by** `scripts/lint-rules.sh`: `ptrace|seccomp|waitpid|libc::` appears in no crate but
`bohr-backend-ptrace`, and no backend implementation appears in `bohr-kernel`'s dependency tree.

### 5.3. The tape is flat and the journal is structured

The tape is bytes. The journal is structure (§4.1). Two otherwise conflicting requirements are met
at once: off-the-shelf byte mutators apply to the tape, and the reducer and the report read the
journal.

A structured tape — a list of typed records — would make mutation semantically aware and would break
every stored corpus entry the moment the set of decision sites changed. A flat tape with no journal
would leave the reducer blind and the report unreadable. The split costs one append-only vector per
run.

The journal is regenerable, and that is what makes it safe for it to be derived: `bohr replay`
re-derives it rather than reading it, and a stored journal that disagrees with the re-derived one is
a hard error, because it means the tape ABI changed under a stored bundle.

### 5.4. One task runs at a time

The scheduler grants exclusive execution to exactly one task. Concurrency is simulated by
interleaving, not by parallelism, which is what makes data races reproducible.

**Limitation.** Genuine multi-core memory-model effects — store buffering, weak ordering — are not
reproduced. On x86-64 with TSO this loses less than it would elsewhere, and it appears in the
non-goals (§13) rather than in small print. A program whose defect requires true parallelism needs a
tool that models the memory model, such as `loom`.

Wall-clock throughput is therefore single-core. Multiple cores are used by running independent seeds
in parallel, which is embarrassingly parallel (§6.2).

### 5.5. Time is logical, and never the host's

`Tick` is virtual nanoseconds (§4.2), read from the world and never from the machine. Passing the
host clock through, monotonic as it is, would break goal T1 on the first run where a timeout fired
at a different point.

**Consequence for what can be tested.** Every timeout in the guest is instantaneous, so a guest that
busy-polls a wall clock expecting real elapsed time spins. That pattern — repeated clock reads with
no other syscall — is detected and reported as a diagnosable condition rather than left to hang.

### 5.6. A seccomp-BPF filter decides what is worth stopping for

Bare `ptrace` stops twice per syscall, for every syscall. A seccomp-BPF filter installed in the
guest decides in the kernel which syscalls are interesting: `SECCOMP_RET_TRACE` for those,
`SECCOMP_RET_ALLOW` for the rest. Goal T3 (≤5× native) is unreachable without it, and the technique
is what took `rr` to roughly 100× faster on microbenchmarks.

The filter is a compiled BPF program that must agree with the syscall handler table. Both are
generated from one table (`bohr-backend-ptrace/src/linux/syscalls.rs`), because a syscall that is
handled but not traced is a nondeterminism hole nothing downstream would detect.

**Enforced by** a test asserting the filter traces exactly the set the handler implements.

### 5.7. An unmodelled syscall ends the run, loudly

A syscall the world does not model is not passed through to the host and is not stubbed with
`ENOSYS`. It ends the run with `BOHR-2001`, naming the syscall number and the calling task.

What makes that survivable is scope discipline: the supported set is what the current hunting
targets need, driven by a report of "syscalls seen but unhandled". Abstract completeness is not a
goal; a ranked backlog derived from real targets is. The first run against a new program is
therefore likely to end this way, and it is made cheap — the error names the syscall and links to
the table, and `bohr survey -- ./x` reports the whole unsupported set at once instead of one entry
per run.

**A survey answers what it cannot model; a run refuses it.** They are separate commands, and not one
command with a flag, because a flag ends up switched on in somebody's continuous integration and a
run that stubbed half its syscalls reports a verdict. What keeps them apart afterwards is not the
flag but the world: a surveying world carries an identity of its own, so anything it produced is
refused by the provenance check before it can be replayed as a run.

**A modelled syscall with impossible arguments is answered, never obeyed.** A guest names its own
lengths, offsets and addresses. A file that would grow past what may be held is `EFBIG`, an address
that wraps the address space is `EFAULT`, and a transfer larger than the bound is carried out as far
as the bound and reported short. The supervisor is the one component whose failure destroys the
evidence a run exists to produce, and a guest that asks for something absurd is usually not a
hostile one but a program in the middle of going wrong — which is exactly when the evidence matters.

### 5.8. `RDRAND` is a hole, and it is documented as one

The processor offers no facility to trap `RDRAND`/`RDSEED`. Four things are done and nothing more is
claimed: the CPUID feature bit is cleared so most libraries take their fallback path; the executable
segments of an object are searched for the `0F C7 /6` and `0F C7 /7` opcodes; the addresses found are
reported as a warning; and, with `--patch-rdrand`, the prepared files are handed to the guest with
those instructions replaced.

**Searching and replacing are not the same question and do not get the same answer.** The search is
two bytes wide and finds every real instruction and some displacements besides. What is *reported*
is all of them, because a site real and unreported is a hole described as solid ground. What is
*replaced* is only what a sweep established: instructions are measured from a function symbol, and
where an object keeps no symbol covering one — a stripped shared object exports little — by several
sweeps begun at different bytes before it, all landing on the same one. Writing over bytes that
turned out to be the middle of another instruction corrupts that instruction silently, so a
candidate nothing established is reported and left alone.

The replacement is the same length as what it replaces: `rdrand r32` is three bytes and `xor r, r`
followed by `stc` is three. Nothing moves, and the guest is handed a zero and told the read
succeeded, which every caller of these instructions already handles because the real ones may fail.
A zero is weaker than a draw from the tape and is the strongest answer available without a trap.

A run is never reported as deterministic while an un-neutralised `RDRAND` sits in a mapped page. The
run header carries `determinism: full | degraded(reasons)`, and `degraded` is printed in red on
every report that has it. This is the one source that fundamentally requires a hypervisor.

**The scan happens at exec and not again.** Re-scanning would mean tracing `mmap`, the single most
frequent call a threaded guest makes — 3 063 of the 23 603 in the `redb` suite, all of them stack
and heap growth mapping no code at all. A guest that maps executable pages after it starts is
reported degraded rather than paid for at every mapping.

### 5.9. A bundle is self-contained and refuses to lie

A bundle carries fingerprints of everything the guest mapped (§4.7), and `bohr replay` verifies them
before running. A mismatch is an error naming the specific difference, not a warning.

Bundles are therefore replayable only against the exact binaries. `bohr replay --binaries <dir>`
exists so a reader can point at the right build, and enough is recorded to fetch it; the check is
not relaxed.

### 5.10. Coverage is an optimisation, and its absence degrades gracefully

Three tiers, in preference order: `-fsanitize-coverage=trace-pc-guard` through shared memory for
guests that can be rebuilt; Intel PT where the host supports it; SDK-provided markers otherwise.
With none of the three the explorer falls back to random mutation and PCT scheduling, and says so.

The "no code changes" claim is therefore exact for *finding* bugs and weaker for *efficiently
searching* for them. The tiers are stated plainly in the README rather than implied to be uniform.

### 5.11. The core performs no I/O and no syscalls

`bohr-kernel`, `bohr-world`, `bohr-explorer` and `bohr-reducer` are state machines over
`(tape, events)`. All contact with the operating system happens in `bohr-backend-ptrace`,
`bohr-trace` (file writing) and `bohr-cli`.

What that buys: the test "a one-directional partition between node 2 and node 3 at tick 40 000
causes the cluster to elect two leaders" runs in microseconds, on any operating system, with no
privileges. A world model that needed a real socket would make that a slow integration test, and the
suite would hold ten of them rather than a thousand.

### 5.12. `unsafe` lives in exactly one crate

`#![forbid(unsafe_code)]` in every crate except `bohr-backend-ptrace` and the FFI shims of the C and
C# SDKs. In that crate every `unsafe` block carries a `// SAFETY:` comment naming the invariant the
caller must uphold.

The supervisor is run over somebody's production build. "The unsafe surface is one crate, here it
is" is an auditable claim; "unsafe is scattered across fourteen crates" is not.

**Enforced by** `scripts/lint-rules.sh`.

### 5.13. The report is a single static HTML file

`bohr report` emits one `.html` with inline CSS and vanilla JavaScript. No server, no framework, no
build step, no `node_modules`, no CDN. It opens from a filesystem, attaches to an email, drops into
an issue, and can be committed to a repository.

This is structural rather than cosmetic: a report that required `npm install` to view would not be
attached to a bug report by the maintainer it is meant to convince, and a report behind a service
dies when the service does. The visualisation is hand-written, a few hundred lines of JavaScript
with no dependencies.

**The cap is four megabytes**, which is what an issue tracker accepts and what opens without
waiting. A run too long to draw within it is windowed rather than truncated: the neighbourhood of
the failure, plus every fault, promise and node change wherever it falls, with the rest written
beside the page as JSON Lines. What is not drawn is counted on the page, because a hole shown as
nothing reads as a run in which nothing happened.

### 5.14. The SDK record format is an external contract

The SDKs read and write the same local-output JSON records that the Antithesis SDKs write when run
outside their environment (`ANTITHESIS_SDK_LOCAL_OUTPUT`), so a team already annotated for
Antithesis runs under Bohr with no edits.

The schema is therefore not ours to change. `bohr-sdk-proto` treats the compatible subset as frozen
and puts every extension under a namespaced key, so a record written here stays readable by their
tooling and the reverse holds. Drift on their side is caught by a compatibility test against
captured fixtures rather than by guessing.

### 5.15. The engine is tested under itself

The kernel, the scheduler and the virtual world are run under themselves, and a determinism gate
runs on every commit ([`TESTING.md`](./TESTING.md) §7). A tool that finds defects in other people's
concurrency has no standing if its own is unverified.

**A flaky test in this repository is a P1 defect, never a retry annotation.** Marking a test
`#[ignore]` because it is "sometimes red" is writing the case against ourselves.

### 5.16. The world is a port, like the backend

`bohr-kernel` owns time, scheduling and the tape. It does not own the model of the environment.
Network, disk, node lifecycle and the fault catalog live behind a `WorldModel` trait, exactly as
interception lives behind `DeterminismBackend` (§5.2). The kernel drives a world; it does not know
which one.

```rust
/// The environment a guest believes it is running in.
///
/// The kernel supplies stops, the clock and the tape cursor; the model decides what the
/// world does about them. Implementations are domain-specific by design: the distributed
/// systems model is one, and a different domain is another.
pub trait WorldModel {
    /// Stable identity, recorded in the bundle. Replaying under a different model is refused.
    fn identity(&self) -> WorldModelId;

    /// The only place a domain interprets guest behaviour.
    fn handle(&mut self, stop: &Stop, ctx: &mut WorldCtx<'_>) -> Result<Effects, WorldError>;

    /// Faults this model can inject, and the moments it considers dangerous.
    fn fault_catalog(&self) -> &FaultCatalog;

    /// Timers the model has armed, in ticks. The kernel merges these into its own queue.
    fn next_deadline(&self) -> Option<Tick>;

    /// Fires everything due at or before the world clock's reading. Wakes and node
    /// operations come back as data; applying them is the kernel's job.
    fn on_deadline(&mut self, ctx: &mut WorldCtx<'_>) -> Result<DueEffects, WorldError>;
}
```

**Time advances by one kernel rule.** At the top of every scheduling iteration the kernel fires,
through `on_deadline`, everything the world armed at or before the current tick. When no task is
schedulable — every live one blocked, or its node frozen — but a deadline is armed, the kernel
advances the world clock straight to that deadline and fires it; only with nothing armed inside the
tick budget is the run declared deadlocked, because a deadline the budget forbids reaching does not
exist for the run. The queue lives in the world, because firing an entry takes world knowledge — a
packet's payload, the partition matrix at the moment of delivery, the buffer of the task being woken
— and the kernel knows only that something is due at tick T. Entries are ordered by
`(tick, node, seq)` and never by insertion order (§8.1).

**Format consequence.** A bundle records `WorldModelId`, and `bohr replay` refuses a bundle produced
by a different model rather than replaying it into nonsense ([`API.md`](./API.md) §4.2). Without
that, the first cross-domain bundle would silently produce a plausible, wrong run — which is §5.9's
failure mode with a new cause.

The seam costs one indirection on the hot path, in the same place and of the same size as §5.2's,
and a `WorldCtx` that exposes the clock, the tape cursor and the trace sink without handing over
ownership of any of them.

### 5.17. How a run is judged is a port as well

`always`/`sometimes` from the SDK and the built-in crash and hang checks answer "did *this* run
misbehave". That is one kind of verdict.

A second kind needs no domain knowledge and no annotation at all: run the same tape against two or
more implementations of one specification and check that they agree. Independent implementations
that must produce identical results — protocol stacks, query engines, virtual machines, consensus
clients — supply an oracle for free. A disagreement is a defect in one of them by construction, and
which behaviour was correct need not be known to report that they differed.

```rust
/// Decides whether a run, or a set of runs, counts as a finding.
pub trait Oracle {
    fn identity(&self) -> OracleId;

    /// Judge one run from its trace and SDK records.
    fn judge(&mut self, run: &RunOutcome) -> Verdict;

    /// Judge a set of runs against each other. This is where differential testing lives:
    /// N implementations, one tape, compare. Returns Verdict::Agree when they match.
    fn judge_group(&mut self, runs: &[RunOutcome]) -> Verdict { Verdict::Skipped }
}
```

That verdict cannot be expressed as an assertion inside one guest, so it has a seam of its own. With
a group oracle a "run" is a *set* of executions driven by one tape, and the corpus, the reducer and
the bundle all carry that (§5.20). Any oracle that composes others composes the whole interface: a
panel that answered `judge` and left `judge_group` to the default would quietly stop comparing.

#### 5.17.1. What is not a port

Three further seams are foreseeable and none of them exists, for one reason: they change only
internal structure and never the on-disk format. A trait that touches the bundle has to land before
bundles matter; a trait that touches only code can wait until a second implementation exists.

| Candidate | Why it is not a trait |
|---|---|
| `CoverageSource` | the three tiers are an internal enum (§5.10) with no format impact |
| `Mutator` | byte operators are one implementation; a journal-aware structural mutator would be a second |
| `FaultTargeting` | a function of the fault catalog, which §5.16 already makes pluggable |

#### 5.17.2. Oracles that ask the guest's own API

A third kind of verdict is about a program that neither asserts anything nor exits: a blockchain
client that finalised a block and later reports another at the same height is wrong, alive, and
answering. The crash oracle has nothing to read, an SDK is not something the client will adopt, and
reading its memory or its disk would be a model of its internals per client. What every client does
offer is its API, and the verdict is made from that (`API.md` §5.9).

**The asking is done by a guest, not by the world.** The probe is a static binary the world starts
as one more node; it calls the nodes' JSON-RPC over the virtual network and writes what came back as
SDK records. Two alternatives were rejected. A probe outside the run would ask at host moments,
so two runs of one seed would be asked different questions and nothing it saw would replay. A probe
inside the world — the supervisor speaking HTTP over sockets it models — would be a TCP client in
the sans-io core, a second implementation of the network's far end, and its timing would be the
world's choice rather than the run's. As a guest it costs nothing new: its sockets are the modelled
ones, its schedule is drawn from the tape, and its calls are on the timeline like any other node's.

**The judging is done by the world, from records of the probe's node only**, and stated as a code
an oracle reads — the division `watch.rs` keeps for liveness and durability. Any guest can write
an event of that name; only the probe's are evidence, or a node under test would be grading its own
answers. The probe's node is outside the fault model for the same reason an instrument is outside
the experiment: a probe the run could kill or cut off would report its own outage as the cluster's.

**Each chain oracle is a probe written out in advance.** `finality` is `never-reverts` on the
finalised hash by height, `agreement` is `agrees` on the same answers, `liveness` is `advances` on
the finalised height, and `double-sign` is `unique` on signed votes by signer and height. One
judging path, so a property tested once is tested for every oracle that uses it.

**What this leaves out.** A probe sees what it asked, when it asked: each node's finalised tip at
drawn moments, so a revert undone between two askings is not seen. And a client that exposes no
signed votes cannot be judged for double-signing — reading consensus messages off the wire is
protocol parsing per client, and none is built in.

### 5.18. Handing a run to a debugger is inspection, not resumption

`bohr replay --break-at` stops a reproduced run at a named event and releases the guests for an
ordinary debugger to attach to. What the debugger gets is a corpse it can read: memory, registers,
the stack, every thread. What it cannot do is continue.

Three mechanisms survive the detach and see to that. The seccomp filter's default action is
`Trace(0)`, and a debugger never sets `PTRACE_O_TRACESECCOMP`, so a syscall the guest attempts under
gdb is stopped by a filter with nobody listening and returns `ENOSYS`. `PR_SET_TSC` is set to fault,
so reading the clock raises `SIGSEGV`. And the run's determinism is the supervisor answering those
calls from the tape — a guest that continued would be answered by the host instead, which is the one
thing this design exists to prevent.

**The limits are printed with the attach lines, every time.** Making a guest continuable would mean
a gdb stub answering from the tape, which is a second interception path carrying the same
correctness burden as the first. A hand-over that printed an attach line and said nothing would earn
a bug report from the first person to type `continue`. `DebuggerTarget` carries the limits beside
the command, and the backend that released the guest fills them in.

`PTRACE_O_EXITKILL` is cleared before detaching, or the kernel would kill the guest as the
supervisor exits; ending the guest is therefore the session's duty, and a `Drop` kills the group
when the session ends. `--gdb` is `--break-at 0` rather than a second path, because two ways to
freeze a run are two ways to freeze it differently.

### 5.19. Layer three decides, layer four carries out

The explorer and the reducer execute nothing. They are state machines: they are asked what to try
next, they are handed what happened, and `bohr-cli` — the crate that owns wiring — is what runs a
guest. There is no `Runner` trait, and the kernel does not export one.

**A trait used only above the kernel is not a port.** `WorldModel`, `Oracle`, `EventSink` and
`SchedulePolicy` are declared in `bohr-kernel` because `run()` consumes them (§5.16, §5.17). A trait
the kernel never calls is shared vocabulary that has been given the wrong home.

**The state-machine shape is what makes the central claim provable rather than reviewable.** A
campaign is a pure function of its master seed *because* the explorer cannot execute anything: `-j 1`
and `-j 8` are proven to agree in milliseconds against a scripted backend. A crate holding
`&mut dyn Runner` could be handed an implementation that reads a clock, and the guarantee would
become a question for a reviewer instead of a property of the structure.

It is also the shape that scales. When the decision-maker is a state machine, the thing carrying out
its decisions can be one process, eight, or a fleet, and the machine never knows. A decision-maker
that *called* a runner would need a network hidden behind the trait and would block an adaptive loop
on it. The same argument makes parallel reduction additive — `next()` widens to "propose several" —
rather than a restructuring.

Two places therefore say how to run one scenario and collect its artefacts. The answer to that
duplication is one concrete function in the crate that owns wiring, not an abstract port one layer
up.

### 5.20. One tape is one source of adversity, not one schedule

`judge_group` compares implementations of one specification driven by one tape. What makes that
sound is not the tape.

**The implementations do not get equal conditions.** They make different syscalls in a different
order, so every draw whose *position* depends on guest behaviour lands somewhere else for each of
them. What is genuinely shared is the seed and the whole byte stream; every draw made before the
first guest instruction, which is the per-node clock offsets, drawn in node order and depending only
on the node count; every scheduled fault at a fixed tick that both executions reach; and the
configuration. What cannot be shared is the scheduler's priorities, the network's per-packet
decisions, and anything drawn after the guests start diverging.

One tape gives them one source of adversity — the same weather, not the same day.

**The value they are compared by must therefore be one the specification requires to be equal under
any legal schedule.** A state root after N blocks is such a value. An event digest is not; a log
ordering is not; anything timing-sensitive is not. Comparing one of those would report a
disagreement on nearly every seed, and a mode that cries wolf on every seed is worse than no mode.

Two consequences follow, and both are enforced rather than documented:

- **Drawn faults are refused alongside a comparison.** `maybe_kill` draws at every syscall —
  deliberately, so that byte consumption cannot depend on disk state — and two implementations make
  a different number of syscalls, so a drawn schedule cannot line up across them under any
  construction. Scheduled faults at fixed ticks can, and do.
- **A comparison that did not happen is not agreement.** A projection one implementation never
  produced leaves the question unanswered, and a verdict of `Skipped` folded into a clean run would
  let a campaign of a million differential runs report a million clean runs having checked nothing.
  It exits non-zero under `BOHR-5004`.

**What a differential run is not.** Heterogeneous clusters — one implementation on some nodes and
another on the rest — are outside the model, and so is a projection read off a request on the
virtual network, which is protocol parsing. A differential run is N homogeneous clusters, never one
mixed one.

---

## 6. Concurrency model

### 6.1 Inside the supervisor: one decision thread

The kernel run loop is single-threaded by construction. It owns the tape cursor, the clock, the
scheduler and the world, and it is the only thing that mutates them.

```
loop {
    let stop      = backend.next_stop()?;            // exactly one task is stopped
    let effects   = world.handle(stop, &mut cursor)?; // pure: (state, stop, draws) -> effects
    trace.record(&effects);
    let next      = scheduler.pick(&runnable, &mut cursor);
    backend.resume(next, effects.resume_action)?;
}
```

There is deliberately no `Arc<Mutex<World>>` anywhere. Concurrency in the supervisor would introduce
a nondeterminism source into the component whose entire purpose is removing them, and the profile
does not justify it: the loop is dominated by context switches into the guest, not by our own
computation.

**What is allowed to be concurrent:** work with no influence on decisions — trace compression and
writing (a bounded channel to one writer thread), report rendering, and the campaign runner.

### 6.2 Across runs: campaign parallelism

`bohr explore -j N` runs N **independent** worlds in N processes, each with its own seed, tape and
supervisor. They share exactly two things, both append-only and both crash-safe: the corpus
directory and the findings directory.

Process isolation rather than threads because a backend failure — an unexpected ptrace state, a
guest that wedges the kernel's ptrace bookkeeping — must kill one worker, not the campaign. The
supervisor is a systems component talking to a hostile guest; assuming it never dies is not
engineering.

**Determinism invariant across parallelism:** a finding produced by worker 7 out of 16 must replay
identically under `-j 1`. Worker identity, worker count and scheduling between workers may never
enter a draw. Enforced by a test that runs the same campaign at `-j 1` and `-j 8` with a fixed
master seed and compares the set of finding digests.

**How it is achieved, rather than hoped for: generations.** A coverage-guided search is a feedback
loop, so what it explores next depends on what came back — and on several workers, what came back
*first* is a fact about the machine. The campaign therefore plans work a generation at a time from
the corpus as it stood at the end of the previous one, and folds a generation's results **in the
order they were planned** rather than the order they arrived. Every fold is then a pure function of
a sorted list. Worker count and worker speed cannot reach a decision because they cannot reach the
work list.

A generation is a **fixed** size for the same reason. Sizing it by `-j` would put the machine back
into the shape of the search, and one worker and eight would explore different things from one
master seed. Workers slice a generation; they do not size it.

The price is that a generation ends when its slowest item does. It is paid by making a generation
several times the worker count, so a straggler costs a fraction of a worker's time rather than all
of it, and it is paid knowingly: a search that is a few per cent slower is worth a search whose
findings anybody can reproduce.

### 6.3 Inside the guest: total order

The guest may create as many threads and processes as it likes. The scheduler admits exactly one to
execution at a time (§5.4). Switch points:

syscall entry and exit · entry to and exit from a synchronisation primitive (`futex`, and the
libpthread paths that reach it) · a blocking operation · a `sched_yield`, which is the guest asking
for one · an explicit SDK `fault_point`.

One regime, on every host (§14.3). A run switches where the guest enters the kernel and nowhere
else, so two instructions that race with no syscall between them stay in the order the processor put
them — which is why a program under test that yields, logs or polls between conflicting steps is
where the search has purchase.

The switch-point set is a **tuning parameter of bug-finding power against speed** and is recorded in
the bundle: a run that used fewer switch points is not comparable to one that used more, and the
report says which was used.

### 6.4 Scheduling policies

`SchedulePolicy` is a trait with one hot method, `pick(runnable, &mut cursor) -> TaskId`.

| Policy | Behaviour | When |
|---|---|---|
| `random` | uniform over runnable tasks | baseline, and the fallback when nothing better applies |
| `pct` | PCT (ASPLOS 2010): `d−1` random priority-change points, otherwise fixed priorities. For `n` tasks and `k` steps, a depth-`d` bug is found with probability ≥ `1/(n·k^(d−1))` per run | default for exploration; most real bugs have depth 1–3 |
| `fair` | uniform, with starvation bounded: a task passed over 64 picks in a row runs next without a draw, so with `n` runnable tasks none waits more than `63 + n` picks | liveness checks, where PCT's long stretches would read as a hang the program does not have |
| `replay` | decisions re-derived from the recorded tape | `bohr replay`, and every reduction step |
| `adversarial` | biases switches toward suspicious sites (around locks, around network operations, right after a write) | after the explorer has localised a suspect region |

PCT is the default rather than `random` because it carries a probabilistic guarantee that `random`
does not. The guarantee is checked against the bound from the paper — a change point falls anywhere
in the run with roughly equal probability — rather than against another implementation of it: a
second implementation would itself have to be trusted, and the bound does not. AWS Shuttle remains
the reference to read when the behaviour is in question.

---

## 7. Data paths

Full traces with budgets. The numbers are targets, and every one of them is measured rather than
asserted ([`TESTING.md`](./TESTING.md) §11).

### 7.1 A guest syscall, from instruction to recorded event

```
 1. Guest executes  clock_gettime(CLOCK_MONOTONIC, &ts)
        │  no VDSO: AT_SYSINFO_EHDR was zeroed at exec, so this is a real syscall
 2. Kernel evaluates the seccomp-BPF filter → SECCOMP_RET_TRACE
        ▼  bohr-backend-ptrace                          budget: ≤ 12 µs per traced syscall
 3. Supervisor wakes on the stop, reads registers, builds Stop::SyscallEntry { nr, args }
        ▼  bohr-kernel
 4. Dispatch on the syscall number to a handler
 5. Handler asks the world: "what is the time on this node?"  → node clock + drift
 6. Handler writes the timespec into guest memory via backend.write_mem
 7. Emit Event::Syscall { nr, args, ret, tick } into the trace
 8. Advance the clock by the syscall's tick cost
        ▼  bohr-kernel::scheduler                       budget: ≤ 1 µs
 9. Pick the next task: policy.pick(runnable, &mut cursor)  ← a draw from the tape
10. Journal the draw: (offset, len, site = SchedPick, chosen task)
        ▼  bohr-backend-ptrace
11. Resume::SkipSyscall { ret: 0 } — the syscall never reaches the host kernel
12. The guest continues, believing it read the clock
```

Steps 5–7 are pure functions of `(world state, draws)`, which is what makes them testable at the
`bohr-world` level with no guest at all (§5.11).

### 7.2 A network packet, from `sendto` to delivery

```
 1. Node A: sendto(fd, buf, …)               → Event::PacketSent
 2. Virtual network: look up the link A→B
 3. Draw: delivered / dropped / duplicated                        ← tape
 4. Draw: delay, from the link's configured distribution          ← tape
 5. Check the partition matrix — including one-directional cuts
 6. Enqueue at tick = now + delay in the delivery queue (a min-heap keyed by tick, then by a
    deterministic tie-break, never by insertion order in a hash map)
 …
 7. The world clock reaches the delivery tick             → Event::PacketDelivered
 8. Node B's task is blocked in recvfrom → becomes runnable
 9. The scheduler may or may not pick it now              ← another draw
10. Data is written into B's guest memory; recvfrom returns
```

The tie-break in step 6 is called out because it is the classic place where determinism dies: two
packets scheduled for the same tick must have a total order that does not depend on allocator
addresses or hash iteration.

### 7.3 A campaign, from seed to a minimal reproducer

```
 1. bohr explore --target ./app -j 8 --time 30m
 2. Worker i takes a tape from the corpus (or seed_i on a cold start)
 3. Mutate: byte flips, splices, havoc — AFL++-style operators over the prefix
 4. Run (path 7.1 × millions)
 5. Collect: coverage bitmap, SDK records, exit status, supervisor properties
 6. New coverage?           → save the tape to the corpus
    Property violated?      → write finding-<digest>.bohr, continue
 7. On campaign end: for every distinct finding
 8.   bohr reduce: delta debugging over the tape and the event list
 9.     each candidate is verified by a full replay: same violation ⇒ keep the reduction
10.   converge → the minimal tape
11. bohr report: timeline, the surviving events, the draws that mattered, one HTML file
```

Step 9 is the expensive part and the reason reduction is a separate command with its own budget: a
reduction from 40 000 events to under 20 (goal T5) costs on the order of a few hundred replays, and
that is a minute-scale operation we must not hide inside `explore`.

### 7.4 The SDK record path

```
 1. Guest calls  bohr::always(invariant_holds, "log is monotonic")
 2. SDK checks BOHR_SDK_OUTPUT (or ANTITHESIS_SDK_LOCAL_OUTPUT); if unset → return, zero cost
 3. Serialise one JSON record, one line, write to the fd
 4. The write is a syscall → we are already stopped on it (path 7.1)
 5. The supervisor parses the line, matches the schema, folds it into the property state
 6. Event::Assertion { name, kind, hit, condition } enters the trace
 7. always violated → the run is a finding, stop now
    sometimes → recorded; only evaluated at campaign end
```

Step 4 is where the cost is avoided: the SDK needs no transport of its own. It writes to a file
descriptor, writes are already intercepted, and the SDK therefore carries no Bohr-specific IPC code
at all. That is what keeps five language SDKs small.

---

## 8. Code rules — Rust

Rules, not preferences. A violation is a blocking review comment; part of it is checked
mechanically in CI (rule 14).

**1. Encapsulation by default.** Struct fields are private. Public means constructors that
**validate** and accessors. A public field says "no invariant exists here"; where an invariant does
exist (a tick is monotonic, a probability is in `[0,1]`, a tape offset is within the prefix) it must
be impossible to break from outside. The exception is `bohr-types` and `bohr-sdk-proto`, which are
DTOs by nature: fields are public there and validation happens at the deserialisation boundary, not
"somewhere later".

**2. Invariants in types, not in comments.** A newtype for every identifier — `NodeId`, `TaskId`,
`EventId`, `Fd`, `TapeOffset`, `Tick`. Two `u64` parameters that can be swapped will eventually be
swapped, and in this codebase a swapped `TaskId`/`NodeId` produces a wrong-but-plausible trace,
which is worse than a crash. Units go in the name (`delay_ticks`, `budget_branches`, `size_bytes`),
never bare `delay`/`budget`. Where zero is invalid, `NonZeroU64`.

**3. Composition, not inheritance emulation.** Shared behaviour goes into a type that is
**embedded** (`struct PctPolicy { rng_sites: Vec<TapeOffset>, … }`), not into default trait methods
that every implementor overrides differently.

**4. Traits at boundaries, generics inside.** `Box<dyn DeterminismBackend>` is right: the backend is
chosen at runtime from configuration and the virtual call happens once per guest stop, where it is
noise next to a context switch. `Box<dyn Fn>` inside the packet-delivery loop is not right. Rule:
dynamic dispatch where the implementation is selected at runtime; monomorphisation everywhere else.

**5. No tuples at boundaries.** Any type crossing a module or crate boundary is named.
`(u64, u64, bool)` in a signature is a future confusion, guaranteed.

**6. Fail fast at the boundary, degrade inside.** An invalid config, a malformed bundle, a truncated
tape header, an SDK record that does not match the schema — `Result::Err` with a precise message at
parse time. No `unwrap_or_default()` turning an error into a silent zero. But: a single malformed
SDK line kills *that record* (with a counter and a sample in the log), not the run; an unsupported
syscall kills *the run* (§5.7), not the campaign.

**7. One source of truth for derived values.** If a tick cost, a delay or a draw-to-value mapping is
computed in two places they will diverge, and the divergence will present as "replay does not
reproduce", which is the hardest symptom we can give ourselves. One `#[inline] fn`, tested alone.

**8. Determinism where there is stochasticity — see §5.1.** Every stochastic choice takes an explicit
`&mut TapeCursor`. Contract: identical input plus identical tape ⇒ bit-identical output. Verified by
golden tests over the trace digest.

**9. Time through the clock, never from the host.** Not one `Instant::now()` or `SystemTime::now()`
in `bohr-kernel`, `bohr-world`, `bohr-explorer`, `bohr-reducer`. Wall-clock time exists in exactly
two places, both of which are excluded from the digest: performance measurement and campaign
budgeting.

**10. Ordered collections in decision paths.** `HashMap`/`HashSet` iteration order is not stable
across runs. Anywhere iteration order can influence a decision or the trace, the type is `BTreeMap`,
`BTreeSet` or `IndexMap`. `HashMap` is permitted only for pure lookup with no iteration, and the
review question is always "does anything iterate this?".

**11. `unsafe` only in `bohr-backend-ptrace`, and there with `// SAFETY:` — see §5.12.**

**12. Errors are typed per crate.** `thiserror` in libraries, `anyhow` permitted only at the top of
`bohr-cli`. Every user-visible error carries a stable machine-readable code from the catalog
([`API.md`](./API.md) §8), what happened, and what to do about it.

**13. Naming.** Rust conventions for casing. Modules are nouns (`tape`, `scheduler`, `world`), never
`utils`/`helpers`/`common` — those accumulate unrelated code. Fallible functions are named so that it
shows (`try_advance`, not `advance`, when it returns `Option`).

**14. Rules a machine checks.** In CI (`make lint`), not by eye at review:
- `cargo clippy --workspace --all-features -- -D warnings`, `cargo fmt --check`, `cargo deny check`,
  `typos`
- `ptrace|seccomp|waitpid|libc::` absent outside `bohr-backend-ptrace` (§5.2)
- `unsafe` absent outside `bohr-backend-ptrace` (§5.12)
- `Instant::now|SystemTime::now|thread_rng|rand::random` absent in the sans-io crates (§5.1, rule 9)
- `HashMap|HashSet` absent in `bohr-kernel`/`bohr-world` decision modules (rule 10)
- those two rules alone skip `chaos.rs`, which is where a crate keeps the deliberate defects the
  mutation harness enables one at a time ([`TESTING.md`](./TESTING.md) §7.4); every other rule,
  `unsafe` included, applies to them unchanged
- no backend implementation crate appears in `bohr-kernel`'s dependency tree (§5.2)
- no world-model or oracle implementation crate appears in `bohr-kernel`'s dependency tree (§5.16, §5.17)
- the seccomp filter's traced-syscall set equals the handler table's key set (§5.6)
- the `.bohr` format version in code equals the one documented in `API.md` §4
- no project meta in code: milestone names, roadmap references, "for now", "added later" (§8.1)

**15. Public API is documented in place.** `#![warn(missing_docs)]` in `bohr-types`,
`bohr-backend`, `bohr-tape`, `bohr-sdk-proto`, `bohr-kernel` and every SDK crate. Those are the
crates an outsider reads or implements against — the kernel's ports are what a second domain
implements (§5.16) — and the first impression is formed in an IDE, not on a website. The lint
belongs on these crates and no others: elsewhere it demands a line on every accessor, which §8.1
forbids.

**16. Comments — English, to the point, per block.** Detailed in §8.1: it is the rule broken most
often and it earns its own section.

### 8.1 Comments

**Language: English only. All code, all comments, all doc comments, all error messages in code, all
identifiers — English. No exceptions.**

The reason is not fashion. `bohr_sdk.h` goes to a third-party developer; the doc comments of
`bohr-types` are published on docs.rs; a report is attached to a bug filed against a maintainer in
another country; a contributor will open these files. A non-English comment in a public tool is an
instant "this is not for outsiders", and no README undoes it.

The split is simple and hard:

| What | Language |
|---|---|
| Code, comments, doc comments, identifiers, error text, log messages | **English** |
| `docs/*.md` | **English**, the only version |
| Commits, PR descriptions, issues, discussions | English |
| Gherkin `.feature` files and their step definitions | **English** — their audience is every user of the tool ([`TESTING.md`](./TESTING.md) §5) |

The scenarios describe the *product contract* ("a run with the same seed produces the same digest")
and are quoted in documentation and issues, so they are English rather than in the team's own
language.

**Exactly as many comments as it takes for the code to be understandable. No more.**

A comment on every line is not thoroughness, it is harm: it doubles the reading volume, diverges
from the code at the first edit, and lies from then on. A lying comment is worse than a missing one,
because it is believed.

Density rule: **a comment explains a block, an algorithm or a contract — not a line.**

We comment **why**, not **what**. The "what" is already said by the code; if the "what" is unclear
the problem is in names and structure, and it is fixed with code, not with prose.

**Register: declarative, not persuasive.** State the constraint and its consequence, then stop. No
rhetoric, no war stories, no arguing with a hypothetical reviewer. Where behaviour follows a
specification, quote it and link it rather than paraphrasing. The reasoning that led to a decision
belongs in this document, where it can be discussed and revised; source files carry the decision and
the one sentence a reader needs to not break it.

```rust
// Right — constraint, consequence, stop.
// Reads spanning the prefix boundary are stitched here rather than by callers: an off-by-one
// in the split changes how the tape decodes at exactly one length.

// Wrong — the same fact, wearing a suit.
// Reads that straddle the prefix boundary are stitched here rather than by callers, because a
// caller that got the split wrong would produce a tape that decodes differently at one specific
// length — the kind of bug that shows up once in ten thousand runs and costs a week.
```

**The shape of a doc comment.** The first line is the contract: what the item is or does, in the
terms a caller checks. Anything after it is one of three things — the edge cases (a bound, a panic,
what zero or empty does), the reference the code follows with the clause that says why that one, or
the determinism constraint a cleanup would otherwise remove. **Three lines of prose is the limit.**
A doc comment that needs more is a design argument: it moves to `docs/`, and the comment keeps the
decision and a § link.

```rust
/// Uniform in `0..n`.
///
/// Modulo at a fixed width, never a rejection loop: a retry makes consumption depend on the
/// drawn value, which the draw ABI forbids (docs/API.md §2.4).
pub fn draw_range(&mut self, n: u64, site: DrawSite) -> Result<u64, TapeError>
```

**The shape of a module comment.** `//!` names what the module owns, in one line. Where a reader
would assume more, a second line names what it does not do.

```rust
//! Logical time: the world clock and each node's reading of it.
//! No host clock is read here; time moves only when the run loop advances it.
```

**A public field** outside the crates of rule 15 gets a `///` when its name and type leave something
open — the unit, the bound, what `None` means. `max_events: u64` earns "event count after which the
run is abandoned"; `node: NodeId` does not earn "the node". Inside those crates the lint asks for a
line on every item, and an obvious item gets exactly one.

**History is not a comment.** A comment states what holds now. The bug that prompted a line, the
version before it, the test it replaced, the target where it was first seen — "used to", "an
earlier draft", "on the first try" — belong to the commit message and the name of the regression
test, where they stay true. The comment keeps the constraint the bug taught: not "the fallback used
to file these exits under node 0", but "a task whose node is gone is filed under no node".

**Where a comment is mandatory:**

- `///` on every `pub` item in the crates listed in rule 15 — that is a contract, not decoration;
- a non-trivial algorithm: one or two lines on which approach and why that one (PCT priority
  assignment, delta-debugging split strategy, the draw-to-value mapping);
- an invariant not expressible in the type system ("caller must hold the task stopped");
- `// SAFETY:` on every `unsafe`, naming the invariant the caller must uphold;
- a workaround for a kernel or libc quirk — **with a link** to the man page, commit or issue.
  Without the link it looks like a mistake in six months and someone "fixes" it;
- a deliberately non-obvious decision: why it is *not* done the obvious way;
- anything that affects determinism. A line that looks redundant but exists to make an order
  stable must say so, or it will be deleted in a cleanup PR and we will spend a week on it.

**Where a comment is forbidden:**

- restating the code (`// increment counter` above `counter += 1`);
- commented-out code — git exists;
- a `TODO` with no owner and no issue link: an unaddressed TODO lives forever;
- decorative separator bars of `=` across the screen;
- a doc comment on an obvious accessor (`/// Returns the id.` is noise, not documentation);
- duplicating what `docs/` already says. Link instead (`// See docs/API.md §2.4 for the draw ABI`),
  because a copy will diverge from the original;
- **project meta of any kind** — milestone names, roadmap references, schedules, "for now",
  "temporary", "to be added later". A comment describes the code as it is. The moment it describes
  the plan it dates itself and starts lying, and the reader who wants the plan is not reading source
  files to find it. Where a limitation is real, state the limitation and its consequence, never the
  date it is expected to end;
- **the history of the line** — what it replaced, which bug prompted it, where it was observed;
- a doc comment past three lines of prose. The argument belongs in `docs/`, the decision here.

Right:

```rust
// Tie-break by (tick, node, seq) and never by insertion order: two packets scheduled for the
// same tick must have a total order that does not depend on allocator addresses.
// See docs/ARCHITECTURE.md §7.2.
struct DeliveryQueue(BinaryHeap<Reverse<(Tick, NodeId, u64)>>);

// SAFETY: `addr` is a guest address validated against the task's mappings above, and `buf` is
// owned by us for the whole call. process_vm_readv does not write outside `buf`.
unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
```

Wrong:

```rust
// Структура очереди доставки                ← not English
struct DeliveryQueue {
    heap: BinaryHeap<Item>,  // the heap      ← restates the code
    len: usize,              // the length    ← restates the code
}
// TODO: think about this                     ← no owner, no link, immortal
// Ties used to follow insertion order        ← history: the commit message holds it
```

A comment is right when it stays true after the bug that prompted it is forgotten.

### 8.2 Authorship in git

**Every action on the repository is performed as the developer. The only author is a human.**

- The `Co-Authored-By` trailer naming any assistant is **forbidden** in commit messages.
- Footers of the form "Generated with …" are **forbidden** in PR descriptions.
- Mentions of development tooling do not enter the git history, tags or release notes in any form.

The reason is plain: authorship in a repository's history is the repository owner's decision, and
the project's history reads as the work of the team. The tool used to type the text has nothing to
do with authorship and no business in `git log`.

The rule is duplicated in `AGENTS.md` and `CLAUDE.md` at the repository root, where automation reads
it.

### 8.3 Commits

Small, and the message is about the important part.

- **Conventional Commits** with an optional scope: `<type>(<scope>): <short description>`.
  Types: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `chore`. Scope: the crate or area
  (`tape`, `kernel`, `ptrace`, `explorer`, `cli`).
- The subject line is imperative, lower case, no trailing period, ≤ 72 characters.
- The body is optional and answers **why**, in one to three sentences. It never lists changed files —
  that is what the diff is for. No "comprehensive", "robust", "enhance", "leverage".
- One logical change per commit. Checkpoint commits are squashed before review.
- A commit that changes determinism-relevant behaviour states the effect on stored bundles
  explicitly: `BREAKING: invalidates bundles with tape ABI < 2`.

```
fix(kernel): break packet delivery ties by (tick, node, seq)

Insertion order into the delivery heap depended on hash iteration, so two
packets scheduled for the same tick could swap under a different allocation
pattern. Digests diverged roughly once in 4000 runs.
```

---

## 9. Code rules — SDKs and language bindings

The SDKs are a different world with different rules, because their readers are other people's
developers and their code lives inside other people's builds.

**S1. There is no logic in an SDK.** An SDK does three things: check whether the output channel is
configured, serialise a record, write a line. Not one decision about whether a property holds, not
one aggregation, not one retry. All evaluation happens in the supervisor. Practical criterion: any
SDK can be thrown away and rewritten in two days by someone who has only read
[`API.md`](./API.md) §6.

**S2. Zero cost when disabled.** Outside Bohr, every SDK call is a branch on one cached boolean and
a return. No allocation, no formatting, no lazy initialisation on the hot path. A team that measures
a slowdown from our SDK removes our SDK, and they are right to.

**S3. Zero third-party dependencies.** No JSON library, no logging framework, no async runtime. The
record schema is small enough to serialise by hand in every one of the five languages, and every
dependency we add is a version conflict in a customer's build and a support ticket for us.

**S4. The schema is shared, the code is not.** All five SDKs emit records validated by the same
JSON Schema, and each SDK's test suite validates its output against that schema file. We do not
share code across languages; we share a contract and a conformance test
([`TESTING.md`](./TESTING.md) §6).

**S5. Antithesis compatibility is a test, not an intention.** A fixture directory holds captured
records from the Antithesis SDKs. A test asserts we parse all of them and that our own output is
accepted by their documented schema. A move in their format turns that test red.

**S6. Idiomatic per language, identical in behaviour.** Rust gets `#[track_caller]` and a macro that
captures the expression text; Python gets a decorator and a context manager; C# gets `[Conditional]`
and `CallerMemberName`; Go gets plain functions; C gets macros. The same call in five languages
produces the same record.

**S7. Comments in the SDKs follow §8.1.** Doc comments on every public member: `///` in Rust,
docstrings in Python, `/// <summary>` in C#, doc comments in Go, `/** */` in C. All English.

---

## 10. Errors, observability, configuration

**Errors.** One error type per crate (`thiserror`); `anyhow` only at the top of `bohr-cli`. Every
user-visible error carries a stable code `BOHR-####` from the catalog in
[`API.md`](./API.md) §8. Codes are never reused for a different meaning — a code appears in a user's
CI log and in an issue, and rebinding it makes the whole history of those logs wrong.

Three error classes, with different handling:

| Class | Example | Handling |
|---|---|---|
| Configuration / usage | bad flag, missing target, bundle version mismatch | fail before starting the guest, exit code 2 |
| Determinism hazard | unsupported syscall, un-neutralised `RDRAND`, unavailable branch counter | abort the run or mark it `degraded`, never continue silently |
| Finding | property violated, guest crash, hang | **not an error**: this is a successful run that found something, exit code 1 |

Exit codes are part of the contract ([`API.md`](./API.md) §7.10) because a CI pipeline branches on
them: `0` clean, `1` finding, `2` usage, `3` internal.

**Logs.** `tracing`, human-readable by default, `--log-format json` for CI. Mandatory span fields
where applicable: `run_id`, `seed`, `node`, `task`, `tick`. **Never logged:** the guest's memory
contents, environment or command-line arguments at `info` or above — a customer's binary is run
under us with production configuration and their secrets are in its environment. Those appear at
`debug` only, behind an explicit `--unsafe-log-guest-env` flag whose name is chosen to be
uncomfortable to type.

**Metrics.** The supervisor is not a service and does not expose Prometheus. It writes a run summary
to the trace and a campaign summary to JSON:

- runs executed, runs per second, guest instructions per second, the slowdown factor versus native
- syscalls traced versus syscalls filtered (the direct measure of whether §5.6 is paying off)
- coverage: edges found, corpus size, time since the last new edge
- findings by class, distinct findings after deduplication by digest
- `sometimes`/`reachable` properties still unsatisfied — the "map of the untested" from §4.6
- degradation flags active (§5.8): the count of runs that were not fully deterministic

**Configuration.** A TOML file plus flags plus `BOHR_*` environment, in that precedence order,
validated at start, fail fast. The effective configuration after merging is **written into the
bundle**, because "it reproduces on my machine" is usually a configuration difference and we should
be able to prove it in one diff. `bohr doctor` validates the host, the config and the target without
running anything ([`API.md`](./API.md) §7.5).

---

## 11. Safety and data

| Question | Decision |
|---|---|
| Is this a security sandbox? | **No.** The README states this. Bohr provides reproducibility, not containment. A malicious guest can escape — `ptrace` supervision is not a security boundary. Same caveat Hermit carried. Run untrusted code in a VM, under us or otherwise. |
| The guest's data | Never leaves the machine. Bundles are written locally; nothing is uploaded; there is no telemetry, no phone-home, no crash reporter. This is unconditional for the open-source tool. |
| Bundles contain guest data | A bundle contains binary fingerprints, syscall arguments and memory the guest read or wrote. **That is customer data**, and `bohr report --redact` plus a documented list of what a bundle holds ([`API.md`](./API.md) §4.6) exist so that a bundle can be attached to a public issue deliberately rather than accidentally. |
| Privileges required | An unprivileged user with `ptrace` on their own processes and unprivileged user namespaces. We do not need and never ask for root. Where a kernel setting blocks us (`kernel.yama.ptrace_scope`, `perf_event_paranoid`), `bohr doctor` names the setting and the exact command to inspect it — and we never change host settings ourselves. |
| Guest network isolation | The syscall filter, whose default action is to trace: every network call either reaches the model or is refused by name, and the admitted list holds only calls that shape the guest's own address space, signals or exec. A guest under Bohr cannot reach the real network **by construction**, which is also what makes network simulation total. A network namespace was used for this and was removed: it guarded nothing the filter does not, while costing 180 KB of unreclaimable kernel memory per guest against 4 KB for every other namespace together, which put a ceiling on how many campaigns a host could run. |
| Guest process numbering | An identifier namespace, which the guest is created inside rather than moved into: a thread stores its own identifier in memory and waits on it, so host numbering reaching the guest is enough to make a run differ from itself. |
| Guest filesystem | A mount namespace with a private root. Writes go to a virtual disk in the supervisor, not to the host filesystem, unless a path is explicitly passed through. |
| Binary rewriting (§5.8) | Off by default, on with `--patch-rdrand`, and it operates on a **copy**. We never modify a file the user gave us. |
| Supply chain | `cargo deny` in CI on every commit: licences, advisories, duplicate versions, and an explicit allow-list of sources. A determinism tool that ships a compromised dependency is worse than no tool. |

---

## 12. Distribution

`bohr` is a binary. There is no service to deploy in year one.

- **Build targets:** `x86_64-unknown-linux-gnu` and `x86_64-unknown-linux-musl`. The musl build is
  static and is the one referenced in documentation, because the most common first contact is a
  minimal CI container.
- **Reproducible builds.** The tool that promises reproducibility is itself reproducible.
  Pinned toolchain in `rust-toolchain.toml`, `--locked` everywhere, `SOURCE_DATE_EPOCH`, and a CI
  job that builds twice on different runners and compares hashes.
- **Distribution:** GitHub Releases with a `.tar.gz` per target and a `SHA256SUMS` file; a
  `bohrup` install script; a container image for CI use. No package-manager submissions in year one —
  they are maintenance we cannot afford yet.
- **SDK publishing:** crates.io, PyPI, NuGet, Go modules. Each SDK version declares the minimum
  `bohr` version it needs; a mismatch is a startup diagnostic, not a mystery.
- **Versioning:** the tool is SemVer. Two contracts version **independently** and are declared in
  every bundle: the tape ABI and the bundle format ([`API.md`](./API.md) §9). A tool upgrade must
  never silently invalidate a stored finding — it either replays it or refuses with the version
  difference named.
- **Supported host matrix** and the minimum kernel version are in [`PLATFORMS.md`](./PLATFORMS.md)
  and are verified by CI, not asserted in prose.

---

## 13. Outside the scope

What Bohr does not do, and why each is a property of the design rather than an omission.

| Not done | Why |
|---|---|
| Windows and macOS hosts | `ptrace`, `seccomp`, `prctl(PR_SET_TSC)` and `arch_prctl(ARCH_SET_CPUID)` exist only on Linux |
| Architectures other than x86-64 | `RDTSC`/`CPUID` handling and the branch counter are architecture-specific |
| A graphical interface | The report is a static HTML file (§5.13); a desktop or web interface is a separate product |
| True multi-core execution | One task runs at a time, by construction (§5.4) |
| `io_uring` beyond files | A ring is observable at `io_uring_enter` once its poller is asleep, and its file operations are carried there (`[linux] io_uring = "ring"`); sockets, timeouts, registered resources and operations that complete later are not, and end the run by name under `BOHR-2004` |
| Direct device access and DMA | Not reachable from above a hypervisor |
| A security sandbox | Not the problem statement, and claiming it would be untrue (§11) |
| Replacing ordinary tests | Bohr complements them; a project with no unit tests gains little from it |
| Language-level instrumentation | Bytecode rewriting is a different tool with different trade-offs, and it breaks the "no code changes" property |
| Distributed campaign scheduling | `-j N` on one machine is the unit of parallelism (§6.2) |
