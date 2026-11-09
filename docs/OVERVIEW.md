# How Bohr works

The plain-language tour. It explains what the machine is, what sits at its centre, how it is
driven, and which parts are reusable — enough to hold the whole system in mind before opening any
other document.

For the strict version, read [`ARCHITECTURE.md`](./ARCHITECTURE.md); for the exact contracts, read
[`API.md`](./API.md). This document deliberately says less than either and links instead of
repeating, because a second copy of a specification always drifts from the first.

---

## 1. The problem

The most expensive bugs in server software happen once in a million runs: two threads line up in an
unusual order, a packet arrives out of sequence, a node dies exactly between writing data and
acknowledging it. Ordinary tests do not catch them. When one does surface, the test goes red
occasionally, somebody labels it flaky, adds a retry — and a real bug is now hidden behind an
annotation.

Those bugs are hard not because they are complicated, but because **the same execution cannot be run
twice.** A system that can run it twice turns them into ordinary bugs.

## 2. The one idea: the entropy tape

An ordinary program is nondeterministic because the outside world leaks into it — the clock, the OS
scheduler, the network, the disk, the random number generator. Bohr **takes the outside world away
from the program and substitutes its own.**

Bohr's world is driven by a single string of bytes: the **tape**. Every time a nondeterministic
decision has to be made, the system bites bytes off the tape.

```
which thread runs next        → 1 byte
drop this packet?             → 2 bytes
how long to delay delivery    → 4 bytes
what to return from getrandom → 8 bytes
kill node 3 now?              → 2 bytes
```

The tape is derived from one 64-bit **seed**: **seed → tape → run.** One number therefore defines an
entire execution.

The tape is a row of boxes, where decision №5 reads from box №5. Box sizes are fixed by the
*question being asked*, never by the answer that comes out. That is what makes mutation useful:
change the contents of box 5 and exactly decision 5 changes, while every later decision still reads
from its own box. If box sizes could vary with their contents, changing box 5 would shift boxes 6, 7,
8 and beyond, and the whole run downstream would become unrelated noise. [`API.md`](./API.md) §2.4
specifies the rules that hold this property.

Five properties follow from that one decision:

| Property | How it follows |
|---|---|
| reproduce a bug | same tape → same run, byte for byte |
| search for bugs | mutate the tape — off-the-shelf AFL++ mutators apply directly |
| shrink a finding | delta debugging over an ordinary list of bytes |
| send a finding to a colleague | one file: tape + binary fingerprints + config |
| explore a different world | change the seed |

## 3. The four layers

```
┌──────────────────────────────────────────────────────────────┐
│ LAYER 4 · Reports and debugging                              │
│ timeline, one static HTML file, replay under gdb             │
├──────────────────────────────────────────────────────────────┤
│ LAYER 3 · Explorer                                           │
│ mutates the tape, keeps a corpus, aims faults, shrinks       │
├──────────────────────────────────────────────────────────────┤
│ LAYER 2 · DETERMINISM KERNEL                                 │
│ logical clock · scheduler · virtual world                    │
│ owns the tape cursor. Not one syscall in the whole layer.    │
├══════════════════════════════════════════════════════════════┤
│ BACKEND BOUNDARY  ← a strict trait; it does not leak         │
├══════════════════════════════════════════════════════════════┤
│ LAYER 1 · Backend — "how do I catch someone else's program"  │
│ v1: seccomp + ptrace      v2: hypervisor                     │
└──────────────────────────────────────────────────────────────┘
```

**Layer 1 — the interceptor.** The only place that touches Linux. Concretely: `ptrace` catches the
program at each syscall; a **seccomp-BPF** filter decides *inside the kernel* which syscalls are
worth stopping for (without it, bare ptrace is ~50× slower — the technique comes from `rr`); Linux
namespaces cut off the network, the filesystem and process numbering; `personality(ADDR_NO_RANDOMIZE)`
disables address randomisation; `prctl(PR_SET_TSC)` and `arch_prctl(ARCH_SET_CPUID)` make the CPU
instructions `RDTSC` and `CPUID` fault so we can answer them ourselves. Crates: `nix`, `libc`,
`seccompiler`. This is the only crate in the project where `unsafe` is allowed.

**Layer 2 — the kernel.** The clock (virtual nanoseconds, unrelated to real time), the scheduler
(**exactly one** thread runs at a time — the only practical way to make races reproducible), and the
virtual world (network, disk, node life and death). There is not a single syscall here; these are
pure state machines. Which is why this entire layer is tested on Windows in milliseconds.

**Layer 3 — the explorer.** The classic fuzzing loop: take a tape from the corpus → mutate → run →
collect coverage and property results → did coverage grow or a property break? keep it : throw it
away. Plus the shrinker: a finding of 40 000 events is reduced to six steps, with every reduction
step verified by re-running it.

**Layer 4 — the report.** One static HTML file, no build step and no framework, so a timeline is a
single file that opens anywhere and needs nothing installed to read it.

## 4. The run loop

The entire thing is this:

```rust
loop {
    let stop    = backend.next_stop()?;                    // the guest froze at a syscall
    let effects = world.handle(stop, &mut cursor)?;        // what should happen? ← draws from the tape
    trace.record(&effects);
    let next    = scheduler.pick(&runnable, &mut cursor);  // who runs now?      ← draws from the tape
    backend.resume(next, effects.resume_action)?;
}
```

One thread, no locks. Concurrency inside the supervisor would introduce nondeterminism into the
component whose only job is removing it.

`&mut cursor` is threaded by hand into every place a decision is made. The inconvenience is
deliberate: an ambient generator would remove determinism without any call site changing, and
nothing in the type system would object.

## 5. The inputs

Three, and everything else is a consequence of them:

| Input | Meaning |
|---|---|
| **seed** | which universe |
| **config** | which world: how many nodes, what latencies, which faults are permitted ([`API.md`](./API.md) §5) |
| **policy** | how the scheduler chooses: `random`, `pct`, `adversarial` |

A run is therefore described in full by something small and copyable, which makes a finding a file
rather than an account of the circumstances that produced it.

**A campaign is one command.** `bohr hunt reth --time 1h -j 8` reads `targets/reth/target.toml`,
takes the binary only if its digest is the pack's (building it where it is missing), runs the
pack's scenario as a search with its state written down so `--resume` carries on, and ends by
grouping what it found by cause, each group led by its smallest run ([`API.md`](./API.md) §7.6).
For a program with no pack, `bohr init -- ./node` writes a first scenario from a survey of it.

## 6. What is reusable, and what is not

The split determines what a second application of the engine — deterministic testing for a different
kind of system — would have to supply.

**Unchanged whatever the domain, and the bulk of the code:**

| Crate | Job | Domain-aware? |
|---|---|:---:|
| `bohr-tape` | tape, draws, journal | no — pure arithmetic over bytes |
| `bohr-kernel` | clock, scheduler, run loop | no |
| `bohr-trace` | event log, digest, bundle format | almost none |
| `bohr-explorer` | mutation, corpus, coverage | no |
| `bohr-reducer` | delta debugging | no |
| `bohr-report` | timeline, HTML | no |
| `bohr-backend` | the boundary trait | no |

The scheduler does not know what a transaction is. The shrinker does not care what it is shrinking.

**Changes per domain — a thin layer above and below:**

- **`bohr-backend-*`** — how the program gets caught. Syscall interception is one answer; an
  in-process backend for a library-shaped target is another and is far faster.
- **`bohr-world`** — the model of the world. This is where domain knowledge actually lives.
- **fault targeting** — knowing which moment is dangerous. Today that is "right after a write, before
  its `fsync`" ([`API.md`](./API.md) §5.4).
- **the property vocabulary in the SDK** — `always`/`sometimes` are generic; what is asserted with
  them is not.

So a second application of Bohr is, in engineering terms: **a new world model, a new fault catalog,
new targeting heuristics and a property library** — on top of an unchanged core.

There are therefore **three** places to plug into, all declared by the kernel and implemented outside
it:

| Port | Answers | Today |
|---|---|---|
| `DeterminismBackend` | how is the program caught? | syscall interception; a hypervisor later |
| `WorldModel` | what environment does it think it is in? | distributed systems: network, disk, nodes |
| `Oracle` | how do we know something went wrong? | crashes, hangs, SDK properties |

The oracle admits a verdict that needs **no annotation and no domain knowledge**: run one tape
against two independent implementations of the same specification and check that they agree. Where
such implementations exist — protocol stacks, query engines, virtual machines, consensus clients —
a disagreement is a bug in one of them by construction.

> A run therefore carries its **provenance**: which world model, which oracle, which backend produced
> it ([`API.md`](./API.md) §4.2). Replaying a bundle under a different model is refused, not
> attempted. Two domains model the same syscall differently on purpose, and a replay that is
> plausible, self-consistent and wrong is indistinguishable from a correct one.

## 7. Where to go next

| Question | Document |
|---|---|
| why is it built this way, and what are the rules | [`ARCHITECTURE.md`](./ARCHITECTURE.md) |
| what exactly crosses each boundary | [`API.md`](./API.md) |
| how do we stop *this specific thing* leaking in | [`DETERMINISM.md`](./DETERMINISM.md) |
| what proves any of this works | [`TESTING.md`](./TESTING.md) |
| what does the host machine need | [`PLATFORMS.md`](./PLATFORMS.md) |
