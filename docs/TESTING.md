# Testing

Living document. It describes what we test, at which level, with which technique, and which gates
must be green before a change lands or a release ships.

A tool that finds bugs in other people's concurrency has no standing if its own is unverified. That
is not a slogan — it is the reason several rules in here are stricter than they would be in an
ordinary project, most of all §9 on flakiness.

Neighbouring documents:
[`ARCHITECTURE.md`](./ARCHITECTURE.md) — components and rules ·
[`API.md`](./API.md) — the contracts under test ·
[`DETERMINISM.md`](./DETERMINISM.md) — the source map each determinism test maps to ·
[`PLATFORMS.md`](./PLATFORMS.md) — what the CI matrix must cover.

---

## 1. Principles

**P1. Determinism is a test target, not a property we hope for.** Every level of the pyramid has at
least one test whose assertion is "the same input produced the same trace digest". A change that
breaks determinism must fail CI within seconds, not be discovered in a customer campaign.

**P2. The core is testable without Linux, without privileges and without a guest.** `bohr-kernel`,
`bohr-world`, `bohr-explorer` and `bohr-reducer` are sans-io state machines
([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.11). Nine tenths of our logic is therefore covered by
tests that run in milliseconds on a developer laptop of any operating system. This is what lets two
engineers work in parallel without one of them blocked on ptrace.

**P3. Every goal the tool claims is an executable test.** T1 through T6 are not prose — each has a
named test and a CI job, listed in §10.

**P4. We test with hostile guests.** `testdata/` contains programs written specifically to be
difficult: a program that reads the clock in a tight loop, one that spawns 200 threads, one that
uses `RDRAND` directly, one that races on purpose, one that writes and never syncs. Testing only
against well-behaved programs proves nothing about a tool whose users are debugging misbehaviour.

**P5. A test that needs a comment explaining what it asserts is misnamed.** Test names are sentences:
`replay_of_a_reduced_bundle_keeps_the_original_violation`, not `test_reduce_3`.

**P6. Coverage is a diagnostic, not a target.** We do not chase a percentage. We do track uncovered
*branches in determinism-critical paths* — the draw ABI, the digest projection, the scheduler
policies — where uncovered means untested means potentially divergent.

---

## 2. The pyramid

| Level | What | Where | Runtime | Needs Linux | Runs on |
|---|---|---|---:|:---:|---|
| **L0** unit | pure functions, state machines, parsers | `#[cfg(test)]` in each crate | < 1 s | no | every commit |
| **L1** property | invariants over generated inputs | `crates/*/tests/prop_*.rs` | < 30 s | no | every commit |
| **L2** integration | layers 2–4 against `bohr-backend-null` | `crates/*/tests/` | < 60 s | no | every commit |
| **L3** system | the real ptrace backend against real guests | `tests/system/` | < 10 min | **yes** | every commit |
| **L4** determinism | repeat-and-compare gates | `tests/determinism/` | < 15 min | **yes** | every commit |
| **L5** cross-machine | a bundle from runner A replays on runner B | `tests/portability/` | < 20 min | **yes** | nightly + release |
| **L6** effectiveness & performance | can we still find the seeded bugs, and how fast | `benches/`, `tests/hunting/` | ~ 1 h | **yes** | nightly + release |

The shape is deliberately bottom-heavy. L3 and above need privileges, a specific kernel and real
time; if they carried the bulk of our confidence, our feedback loop would be twenty minutes long and
we would stop running it.

---

## 3. How we work — TDD in practice

We write the test first where the test is cheap to write and expresses a contract. We do not
ritualise it where it does not help.

**Test first, always:**

- anything in [`API.md`](./API.md): the draw ABI, the bundle format, the record schema, exit codes.
  These are promises, and a promise written as a test before the implementation is a promise that
  cannot drift;
- every bug fix. The regression test goes in the same commit as the fix, and the commit message
  states the seed or bundle that found it;
- every state machine in `bohr-world`: a network link, a disk, a node lifecycle. These are the parts
  where "what should happen" is genuinely arguable, and writing the test first is where the argument
  gets settled;
- every determinism guarantee.

**Implementation first is fine:**

- exploratory work against a real kernel — no meaningful test can be written for a `ptrace` stop
  sequence that has not been observed yet. The rule is that the test is written *before the pull
  request*,
  not before the code;
- report rendering, CLI plumbing, log formatting.

**The loop:**

```
1. write the failing test, run it, watch it fail for the RIGHT reason
2. make it pass in the smallest way
3. make it clean — see ARCHITECTURE.md §8
4. make lint && make test-fast
5. commit (small, conventional, see ARCHITECTURE.md §8.3)
```

Step 1's second half is not ceremony: a determinism test that passes because it compared two empty
traces is the most dangerous test we can have. Every determinism assertion is verified to fail when
determinism is deliberately broken — §7.4 describes the mutation harness that automates exactly that
check.

---

## 4. L0–L2: unit, property and integration

### 4.1 Unit

Colocated `#[cfg(test)] mod tests`, arranged arrange–act–assert. Nothing that touches the filesystem,
the network, the clock or a subprocess.

Priority areas, in the order we would rewrite them if they were lost:

| Area | Crate | What must be exhaustive |
|---|---|---|
| the draw ABI | `bohr-tape` | `draw_range` for every `n` in `0..=257` and near the byte-width boundaries; zero-byte cases; the modulo bias bound; that consumption never depends on the drawn value |
| the digest projection | `bohr-trace` | every excluded field excluded, every included field included ([`API.md`](./API.md) §4.4) |
| bundle read/write | `bohr-trace` | round trip; truncation at every byte offset; unknown sections preserved; a provenance mismatch is refused, not replayed ([`API.md`](./API.md) §4.2) |
| scheduler policies | `bohr-kernel` | PCT priority assignment against hand-computed vectors |
| virtual disk | `bohr-world` | `fsync` semantics, tear boundaries, reordering windows |
| virtual network | `bohr-world` | one-directional partitions, delivery tie-breaks |
| syscall argument decoding | `bohr-backend-ptrace` | every supported syscall's argument shape |

### 4.2 Property tests

`proptest`, with the shrinker configured and failing cases persisted to
`crates/<crate>/proptest-regressions/`. Those files are committed: a shrunk counterexample is a test
case we paid for.

The properties that matter most:

```
tape:      draw_range(n) always returns a value in 0..n, for all n and all tape bytes
tape:      the byte count consumed by a draw depends only on n, never on the drawn value
tape:      a cursor advanced by k separate draws lands where one combined draw would
bundle:    write(read(b)) == b for any well-formed bundle
bundle:    any single-byte corruption is detected by section hashes
digest:    permuting two events with different EventIds changes the digest
world:     a packet is never delivered before it is sent, under any fault schedule
world:     nothing survives a crash that was not fsynced, under strict semantics
world:     a one-directional partition A→B does not affect delivery B→A
kernel:    the same tape and the same world produce the same event sequence
reducer:   a reduction result still reproduces the original violation
reducer:   reduction is monotonic — the result is never larger than the input
oracle:    a group verdict does not depend on the order the executions are given in
bundle:    a bundle with executions > 1 is refused by a single-execution reader, never
           silently read as its first stream
```

The last two are the reducer's entire contract. A reducer that produces a smaller scenario that no
longer reproduces the bug is worse than no reducer, because it wastes a maintainer's afternoon.

### 4.3 Integration against the null backend

`bohr-backend-null` drives a synthetic guest — a small scripted program of "syscalls" defined in
Rust — through the real kernel, world, scheduler and trace writer. This gives full-speed coverage of
layers 2–4 with no privileges and on any OS.

What lives here:

- a five-node cluster splits, heals, and the trace shows exactly the expected fault events;
- a task blocked in `recvfrom` becomes runnable when a packet is delivered, and not before;
- `always` violated in a synthetic guest produces a bundle whose manifest names it;
- a campaign at `-j 1` and `-j 8` with the same master seed produces the same set of finding digests
  ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §6.2);
- reduction of a synthetic 40 000-event scenario converges to under 20 events (goal T5), measured
  rather than asserted;
- **two world models are swappable**: the same tape drives a second, trivial `WorldModel` and the
  resulting bundles refuse each other on provenance ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.16).
  A seam that is never exercised through a second implementation is decorative;
- **a differential oracle finds a planted disagreement**: two synthetic guests, identical except for
  one branch, are run from one tape and the group verdict reports which event first differed (§5.17).

**The run loop is tested in its own crate, not only from outside.** `crates/bohr-kernel/tests/
run_loop.rs` drives `run()` with a scripted backend and a world whose answers are named by call
number, so every `StopReason` and every `NodeOp` has a test that reaches it directly — 18 tests,
none of which needs the real world or the real backend. The scripted backend reports nothing about
a task it killed, exactly as the real ones do: a fake that reports the exit hides a kernel that
keeps a killed task's record for ever.

---

## 5. BDD — behaviour specified in Gherkin

Gherkin is used where a scenario is a **product contract that gets quoted outside the team**: in
documentation, in an issue, in a blog post. It is not used as a second syntax for unit tests.

Scenarios live in `tests/features/*.feature`, steps in `crates/bohr-cli/tests/bdd.rs`, run by
`cucumber` against the built binary. A scenario that needs a program on disk is tagged `@linux` and
is skipped, by name, where no program can be intercepted.
**Both are in English** — the audience is every user of the tool, not the two of us
([`ARCHITECTURE.md`](./ARCHITECTURE.md) §8.1).

```gherkin
Feature: Reproducibility

  Scenario: The same seed produces the same run
    Given the built-in guest "clock-and-random"
    When I run it 100 times with seed 42
    Then the run succeeds
    And the report says every run agreed

  Scenario: A different seed produces a different run
    Given the built-in guest "clock-and-random"
    When I run it once with seed 1
    And I run it again with seed 2
    Then the two runs report different digests
```

The other files are `bundle.feature`, `doctor.feature`, `findings.feature` and `refusals.feature`,
which is where a program this build cannot carry deterministically is refused under the code that
says which wall it hit. What is quoted here is quoted **from** those files; a scenario that appears
only in this document is a scenario nothing runs.

Rules for feature files:

- one feature per user-visible capability, and the file is named after the capability, not the crate;
- steps are reusable and phrased in the product's vocabulary (run, seed, bundle, digest, finding) —
  never in implementation vocabulary (cursor, ptrace stop, section id);
- a scenario that cannot be written without mentioning internals belongs in L0–L2, not here;
- every goal in §10 of this document has a scenario, mapped one to one.

---

## 6. SDK conformance

Five SDKs, one contract ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §9 S4). Four of them exist —
Rust, Python, C# and Go — and the suite requires all four on every change, including the ones a
change did not touch: the point of a shared contract is that it is checked as one thing. C is
skipped by name until it arrives, because a language that cannot be checked must not look like one
that passed.

Go's driver is what checks that an assertion reports the guest's own line: the walk out of the SDK
leaves by directory, and the SDK's own tests sit in that directory, so only a program in another
package can hold it to account.

**The conformance suite** is a directory of cases, each `case.json` describing calls to make and
`expected.jsonl` holding the records that must result. Every SDK has a small driver that reads the
case, makes the calls, and writes its output; a shared checker compares modulo field order and the
fields documented as environment-dependent (`location.file`, `seq`).

```
sdk/conformance/
├── cases/
│   ├── always_pass/            ├── always_fail/
│   ├── sometimes_never_true/   ├── reachable/
│   ├── unreachable_hit/        ├── random_bytes_from_tape/
│   ├── fault_point/            └── setup_complete/
├── drivers/ {rust,python,csharp,go,c}/
└── check.py
```

Additional SDK-level gates:

| Gate | Assertion |
|---|---|
| schema validity | every produced record validates against `records.schema.json` |
| schema freshness | the committed schema equals the one regenerated from the Rust types |
| Antithesis interop | every fixture in `testdata/sdk/antithesis/` parses; our output satisfies their documented shape |
| zero cost when disabled | a benchmark asserts a disabled `always` call is within noise of an empty function |
| zero dependencies | a manifest check: the Rust SDK has no dependencies outside `std` plus `bohr-sdk-proto`; the Python, Go and C# packages declare none |
| formatting | `gofmt` reports nothing in the Go SDK or its driver, and `go vet` is silent |

---

## 7. Determinism gates — the tests that define the product

### 7.1 Repeat-and-compare (goal T1)

```
bohr run --repeat 1000 --seed <s> -- <guest>   ⇒  1000 identical trace digests
```

Run on every commit for the small guest set, nightly for the large one. Failure output is the first
diverging event on both sides, not "digests differ" — the diff is the debugging session.

Guests covered on every commit: `clock_and_random`, `threads_racing`, `spawn_and_wait`,
`file_write_sync`, `socketpair_pingpong`. Nightly adds the real third-party targets from §8.

The cluster gate itself — `examples/split-brain`, five `raft-rs` processes through a
one-directional cut and back — is run by `scripts/gate.sh` rather than by the suite: it needs the
example built, and a check that cannot run must not look like a check that passed.

### 7.2 Replay fidelity (goal T4)

For every bundle in `testdata/bundles/` — a growing collection, one per historical bug —
`bohr replay --verify-only` must exit 0. These are our golden files, and they are the reason a
refactor of the scheduler cannot silently change what a stored finding means.

**Adding to the collection is mandatory** whenever we fix a determinism bug: the bundle that
exhibited it becomes a permanent regression test.

### 7.3 Cross-machine reproduction (goal T4, second half)

Nightly, and blocking for a release. Runner A (Intel) produces bundles; runner B (AMD) replays them,
and vice versa. Any divergence is a P1: it means something host-specific reached a decision, which
is exactly the class of bug that makes the tool untrustworthy.

The CI matrix for this is in [`PLATFORMS.md`](./PLATFORMS.md) §6.

### 7.4 Negative determinism — the mutation harness

A determinism test that cannot fail proves nothing. Each crate keeps its deliberate defects in its own
`chaos.rs`, behind a feature that names one of them; `bohr-testkit` forwards those features so the
harness can enable exactly one at a time:

| Feature | Injection | Should be caught by |
|---|---|---|
| `chaos-ambient-rng` | tape bytes replaced with a process-seeded generator | §7.1 repeat-and-compare |
| `chaos-hash-iteration` | the runnable set reordered by hash iteration | §7.1, within ~100 repeats |
| `chaos-delivery-tie` | the scheduling choice resolved by arrival time, not by a draw | §7.1 and the world property tests |
| `chaos-wall-clock-digest` | host-measured time folded into the digest | §7.1 immediately |
| `chaos-syscall-passthru` | one clock answer taken from the host | §7.1 and §7.3 |
| `chaos-futex-order` | the longest-waiting task released, not the drawn one | the futex wake-order test |
| `chaos-node-clock-shared` | every node answers the time from the world clock | the node-clock divergence test |
| `chaos-deadline-tie` | due timers fire in an ambient order, not `(tick, node, seq)` | the same-tick timer test |
| `chaos-fault-moment-drift` | a scheduled fault armed a few ambient ticks late | the torn-write and split-brain repeats |
| `chaos-coverage-ambient` | a process counter mixed into the coverage map | the same-seed-same-edges test |
| `chaos-unknown-descriptor` | a vectored write to a socket answered `EBADF` | the descriptor audit, `own_evidence` |
| `chaos-proc-on-the-disk` | what the kernel answers about a process left on the disk | the crash-image audit, `own_evidence` |
| `chaos-worker-order` | a generation folded in the order the workers finished | the campaign order-independence test |

A CI job builds with each injection in turn and asserts that the determinism suite **fails**. If an
injection passes, the corresponding test has a hole and the job is red. It is the job that tests
the tests.

### 7.5 Self-hosting

`bohr-explorer` and `bohr-reducer` are themselves run under Bohr with the null backend, as
guests. Running the scheduler under itself is the cheapest way to find the concurrency bugs in a
component whose own job is finding concurrency bugs.

---

## 8. Effectiveness: can we still find the bugs

Correctness gates say we did not break anything. These say we are still *good*.

**Seeded-bug corpus.** `testdata/bugs/` holds programs with a known, documented defect and a
recorded difficulty:

| Program | Bug | Depth | Budget |
|---|---|---:|---|
| `race_counter` | classic lost update between two threads | 1 | < 100 runs |
| `race_init` | double initialisation under a specific interleaving | 2 | < 1 000 runs |
| `raft_toy` | acknowledged write lost when the leader dies before it replicates | 3 | 256 runs |
| `kv_ack_quiet` | the same as `kv_fsync`, with the self-check removed: the store loses an acknowledged write and exits 0 | 2 | 44 of 60 seeds: a crash lands every unsynced write a quarter of the time, and then nothing is lost |
| `kv_fsync` | acknowledged write lost after crash, no `fsync` | 2 | < 2 min |
| `partition_split_brain` | two leaders under a one-directional partition | 3 | < 10 min |
| `no_checksum` | a datagram protocol with no checksum applies an altered update | 1 | ≥ 20 of 60 seeds with `corrupt_probability`, 0 without |
| `header_without_record` | a header rewritten to point at an appended record, one `fsync` after both | 2 | ≥ 20 of 60 seeds with reordering, 0 with `reorder_unsynced = false` |

**Some of these catch themselves, and one deliberately does not.** `kv_fsync` compares what it
acknowledged against what survived and exits 1, so the crash oracle finds it — which makes it a
good test of the *search* and no test at all of an oracle that judges from the boundary. Its quiet
twin `kv_ack_quiet` is the same defect with that check removed, and it comes with a *correct* twin
as well: the gate is the pair, **found on the defective one and never on the correct one**, because
an oracle that fired on every run would satisfy the first half alone. Numbers under
`[m4_1.durability]`.

Two claims that must not be conflated, and `bohr-testkit`'s fixtures separate them: the scripted
pairs in `tests/liveness.rs` and `tests/durability.rs` prove **the oracle is correct**, in
milliseconds and on any machine; the corpus entries above prove **the search reaches the defect**,
which needs a real interceptor and a budget.

**Depth is the claim; the budget is only the bound of the experiment.** What this tool promises is
that it reaches what ordinary testing cannot — a defect of depth 3 needs three things to fall
together, and a search that reaches one is qualitatively different from a search that does not. A
search that reaches it in four minutes rather than nine is the same search.

So the gate is stated as reach: a defect is **found on a majority of master seeds**, which is the
line between finding it and being lucky. The budget bounds the experiment, because without one the
claim is unfalsifiable — infinite time finds everything. It is written in runs wherever the suite
fixes one, because `--runs` is the budget that repeats exactly and a budget in minutes is a budget
that depends on the machine.

**One fault probability across the whole corpus, never tuned per target.** This is the rule that
keeps the number honest, and it is not hypothetical: measured on `kv_fsync`, raising the odds a
fault fires from 1/400 to 1/10 took uniform targeting from 13 seeds in 60 to 60 in 60
([`API.md`](./API.md) §5.4). If the gate were "found quickly", the winning move would be to turn
that knob — which is more chaos rather than a better search, and would make every number here
measure the knob.

Time is recorded and published as measured. A search that has become twice as slow is worth
noticing, and that is what the number is for; it is not what the milestone claims.

**The chain oracles have a corpus of their own.** `examples/chain-toy` is a three-node chain that
finalises a block on two votes of three, correct without a flag; each flag adds one defect, and each
needs the faults its scenario draws — a kill, an isolation — to show:

| Flag | Defect | Oracle | Code |
|---|---|---|---|
| `--revert-finality` | the finalised log is written without a sync; after a crash the node rebuilds the heights it lost from blocks of its own | `finality` | `BOHR-2040` |
| `--equivocate` | a proposer resending before its block is final builds a new one and signs it too | `double-sign` | `BOHR-2043` |
| `--fork-on-partition` | a node that hears no peer finalises blocks of its own, alone | `agreement` | `BOHR-2041` |
| `--stall-after-heal` | a node that was cut off stops proposing once it hears its peers again | `liveness` | `BOHR-2042` |

The gate is the same pair as for the durability twins: each oracle **reports its code on at least
30 of seeds 1–60** with its flag, and **no oracle reports anything on seeds 1–200** without one.
The scenarios differ from `scenario.toml` by the flag and the oracle and by nothing else, so a
number here measures the oracle and not a scenario tuned to show it. The fixture was shaped on
seeds 1001 and up and judged once on 1–60; figures under `[v0_4.gates]`. The oracles' own
correctness is proved without a guest, by the observer's unit tests in `crates/bohr-world/src/probe.rs`.

**Policy comparison** (`scripts/policy-comparison.sh`). The same corpus is hunted with `random` and
with `pct`, and what is recorded is the share of seeds that expose each defect rather than the first
seed that does: once a defect is reachable from most orderings both policies answer "the first one",
and the share still separates a policy that reaches it from every angle from one that stumbles into
it. If PCT ever falls behind random, either the implementation broke or the reasoning behind it did,
and both turn the build red.

**Fault targeting comparison** ([`API.md`](./API.md) §5.4). `targeting = "uniform"` versus
`"guided"` on the same corpus, same budget. The specification claimed guided targeting is worth an
order of magnitude, and this job is what answered it: measured on `kv_fsync` guided loses at every
probability, its candidate moments being a subset of uniform's and on that target most of the run.
§5.4 withdraws the claim rather than softening it. The job stays, because the mechanism is sound and
the heuristic choosing its moments is what needs the work — this is where that will show.

**Reduction on a real client.** A fixture's ratio is not a reducer's ratio: `PingPong` shrinks from
49,477 events to 18 because nothing in it costs events but the faults, and a real client spends
thousands on starting. So the reducer is judged on findings the target packs produce, each reduced
with `--budget 200`, by three numbers: the share of events kept, the wall time, and how many of 100
replays of the result reproduce the same violation signature. The last is the contract; the first
two are measured and published, and which client and seed each finding is stays out of the tree
until its maintainers have it. Figures under `[v0_7.reduction]`.

---

## 9. Flaky tests

**There is no retry annotation in this repository, and there never will be.**

A test that fails intermittently has found either a real defect in Bohr or a real nondeterminism in
the test. Both are exactly what this tool exists to find. Marking it `#[ignore]` or wrapping it in a
retry writes the case against ourselves, in our own repository, where anyone can read the diff.

The procedure when a test flakes:

1. Stop. It is a P1 until triaged, and it blocks the branch.
2. Capture the seed. Every test that involves a run prints its seed on failure, which is a
   requirement on the test harness.
3. Reproduce with `bohr replay`. If it does not reproduce, the nondeterminism is in *our* process,
   and that is a more serious finding than the original test failure.
4. Reduce, fix, and add the bundle to `testdata/bundles/` (§7.2).
5. If the root cause was a missing determinism guarantee, add the corresponding injection to the
   mutation harness (§7.4) so the class cannot come back.

Tests that legitimately depend on the environment — timing-sensitive performance gates, hardware
counter availability — are not flaky tests. They are **capability-gated**: they check the capability
first and skip with a printed reason, and the CI job that is *supposed* to have the capability fails
if the skip happens.

---

## 10. Acceptance criteria as tests

Every goal the tool claims, mapped to the thing that proves it.

| ID | Criterion | Test | Level | Gate |
|---|---|---|---|---|
| T1 | 1000 runs of one seed, byte-identical trace | `determinism::repeat_1000` | L4 | every commit |
| T2 | no code changes in the target | `system::runs_prebuilt_binary_unmodified` | L3 | every commit |
| T3 | ≤ 5× native slowdown | `scripts/bench-slowdown.sh` | L6 | nightly, release-blocking |
| T4 | a bundle reproduces 100/100, on another machine too | `determinism::replay_100`, `portability::cross_cpu` | L4, L5 | commit / nightly |
| T5 | reduction by more than 100× in events | `hunting::reduce_ratio` | L6 | nightly |
| T6 | 5 nodes with a network in one deterministic world | `integration::five_node_cluster` | L2 | every commit |
| C1 | `bohr run -- ./cat /dev/urandom \| head -c 100` identical 1000× | `determinism::urandom_stable` | L4 | every commit |
| C2 | a 24-hour test finishes in seconds with the same result | `determinism::a_day_of_sleep_costs_no_real_time` | L4 | every commit |
| C3 | a known race is caught in fewer than 1000 runs | `determinism::a_known_race_is_found_by_searching_seeds` | L4 | every commit |
| C4 | split-brain and heal reproduced | `integration::split_brain_then_heal` | L2 | every commit |
| C6 | a depth-3 consensus defect reached on a majority of master seeds | `hunting::raft_toy` | L6 | nightly |
| C7 | 40 000 events reduced to fewer than 20 | `hunting::reduce_raft_toy` | L6 | nightly |

A criterion with no test in this table is not a criterion, it is an aspiration. Adding a goal to the
specification means adding a row here in the same change.

---

## 11. Performance

Performance is a correctness property here: a tool at 50× native does not get run at all, and goal
T3 is the line the project draws.

**One producer, one file.** Every number here comes from `scripts/bench-slowdown.sh` and lands in
`benches/baseline.toml`, which is also where the guests it measures are listed — the script
enumerates the file rather than carrying a list of its own, for the same reason the syscall table
generates both the filter and the dispatch: two lists that can disagree eventually do.

There are no micro-benchmarks. An earlier version of this section described `criterion` targets in
`benches/`, and there were none: `make bench` ran `cargo bench` over a workspace with no benchmark
targets and passed by doing nothing, which is worse than an absent job because it reads as a
passing one.

**Macro** (`scripts/bench-slowdown.sh`): the metric is the slowdown factor versus running the same
guest natively, on the same machine, in the same job. An absolute number is meaningless across
runners; a ratio measured side by side is not. The baseline is `benches/baseline.toml`, which
carries the last measured ratio per guest and the ceiling the job refuses to go past. The gap
between them is deliberate: a threshold tight enough to catch machine noise catches nothing else.

The ratio alone is a blunt instrument, because the native side of a short guest is mostly process
startup and swings by a quarter between runs on one machine. `syscall_storm` therefore also reports
**the cost of one intercepted syscall**, with startup measured separately and subtracted. That
number moves with the interception path and not with the machine, and it is the one a regression
shows up in first.

**Startup is judged in milliseconds, not as a ratio.** A guest that lives under 10 ms natively has a
ratio of two startups: `cat` lives about a millisecond and stood at 4.7× against a ceiling of 4.0×,
red on two toolchains with no change to interception. Such a guest prints its ratio and is not held
to a ceiling. What judges it is the startup gate:
`testdata/guests/startup` traced minus native, best of 20 attempts, against `[startup] ceiling_ms`
in `benches/baseline.toml`. The gate fails when the startup guest is not built, because a gate that
passes on a missing measurement is not a gate.

**The guests run from the host's own filesystem.** The bench copies them to a temporary directory
first. From a Windows drive mounted under WSL, starting the same static guest cost 5.38 ms natively
against 1.09 ms from `/tmp`, and 11.63 ms traced against 4.16 ms, because the supervisor reads the
binary as well: the startup figure measured the mount, not the supervisor. A timed attempt that
exits nonzero fails the gate rather than counting as a fast one.

Per-guest thresholds live in `benches/baseline.toml` beside the measurement, and nowhere else. They
were once written here as well, with different numbers — `syscall_storm` at 8× here against 1000× in
the file — and a threshold with two values has none.

**The seccomp filter ratio** is tracked as its own metric: traced syscalls divided by total syscalls.
It is the direct measure of whether §5.6 is still paying off, and a jump in it explains a
slowdown faster than any profile.

---

## 12. Fuzzing

`cargo-fuzz` targets, run in CI for a short budget on every commit and continuously in a nightly
job. Every target parses **untrusted input**, which for us means input that could be a corrupted
bundle or a hostile guest's output:

| Target | Input | Must never |
|---|---|---|
| `fuzz_bundle` | arbitrary bytes as a `.bohr` | panic, allocate unboundedly, loop forever |
| `fuzz_tape` | arbitrary bytes as a `.tape` | panic |
| `fuzz_sdk_record` | arbitrary bytes as a JSON Lines record | panic |
| `fuzz_event_log` | arbitrary bytes as an event log, including truncation | panic |
| `fuzz_syscall_args` | arbitrary syscalls, with arbitrary registers, into the world | panic, allocate unboundedly |

Seeds live in `fuzz/seeds/` and are committed; the corpus a fuzzer grows from them lives in
`fuzz/corpus/` and is not. Seeds are written by the same writers the tool itself uses
(`make fuzz-seed`), so they cannot drift from the formats they are meant to reach past, and an
input that once brought a reader down is kept among them by name.

A panic in the bundle reader is not a
theoretical concern: the bundles we are handed will include ones truncated by a supervisor that
died, and refusing to read them cleanly is what turns a bad run into a lost investigation.

---

## 13. Test data

```
testdata/
├── guests/          # hostile-by-design programs, C and Rust, with a Makefile and pinned flags
│                    # clock_and_random · cat · rdtsc_and_clock · syscall_storm ·
│                    # startup · two_threads · long_sleep · file_roundtrip ·
│                    # unmodelled_syscall
├── bugs/            # seeded-bug corpus (§8), each with README.md describing the defect
├── bundles/         # golden bundles, one per historical determinism bug (§7.2)
├── tape/            # golden draw vectors, the exponential table vectors
├── sdk/antithesis/  # captured records from the Antithesis SDKs (§6)
└── traces/          # reference event logs and their expected digests
```

Guest binaries are **built in CI from committed source with pinned compiler flags**, never committed
as binaries. A committed binary would make our own golden tests depend on someone's toolchain from
two years ago, and we would not notice until it stopped reproducing.

---

## 14. Running tests

```bash
make test-fast     # L0-L2: unit, property, integration. No Linux needed. Under a minute.
make test          # everything runnable on this host, including L3-L4 on Linux
make test-det      # the determinism gates only (§7)
make test-sdk      # the SDK conformance suite (§6)
make bdd           # the cucumber features (§5)
make bench-slowdown # slowdown versus native against benches/baseline.toml (§11)
make policy-comparison # which scheduler reaches the seeded defects (§8)
make gate          # every milestone gate, measured on this host
make fuzz T=bundle # one fuzz target for 60 seconds, from its committed seeds
make fuzz-seed     # rewrite those seeds from the current formats (§12)
make lint          # fmt, clippy, deny, typos, ruff+mypy, and the mechanical rules (ARCHITECTURE.md §8 r.14)
make pr            # what CI runs on a pull request: lint + test-fast + test + bdd
```

`cargo nextest` is the runner; configuration in `.config/nextest.toml`. Nextest is not cosmetic here:
per-test process isolation matters when a test drives `ptrace`, and a leaked traced process must not
poison the tests that follow.

**Before every pull request: `make pr`.** It is the same set of jobs CI runs, so a green local run
means a green CI run, and a red CI run after a green local run is itself a bug we fix.

---

## 15. CI jobs

| Job | Trigger | Blocking | Contents |
|---|---|:---:|---|
| `lint` | every push | yes | fmt, clippy `-D warnings`, deny, typos, the mechanical rules |
| `test-fast` | every push | yes | L0–L2 on Linux, macOS and Windows — proving P2 holds |
| `test-linux` | every push | yes | L3–L4 on the supported kernel matrix |
| `determinism` | every push | yes | §7.1, §7.2 on the small guest set |
| `mutation` | every push | yes | §7.4 — the injections must break the determinism suite |
| `sdk` | touching `sdk/` | yes | §6 conformance across every language that has an SDK; the SDK crates build on their `rust-version` (`make msrv`) |
| `bdd` | every push | yes | §5 |
| `docs` | touching `docs/` | yes | every relative link resolves |
| `fuzz-short` | every push | yes | 60 s per target |
| `portability` | nightly | no | §7.3 cross-CPU replay |
| `hunting` | nightly | no | §8 — does the search still reach the seeded defects |
| `perf` | nightly, release | release only | §11 |
| `fuzz-long` | nightly | no | 4 h per target |

A trigger in the second column is a path, never an event. `sdk` and `docs` were written to
react to pull requests alone, and a repository pushed to directly opened none that touched
those paths, so two blocking gates had never run once — which is indistinguishable from two
gates that always passed. A gate that does not fire on push is not a gate, and
`scripts/check-workflows.py` now refuses one.

## 16. Subjects and checks

**A gate is not a script, but a line in a subject's file.** Until v0.5 every gate on a real program
was a shell script of its own, each repeating one of five moves — run seeds and count a code, repeat
a seed, run without faults, survey the walls, compare with and without — and a pack got checks only
where somebody typed them. The v0.3 reth pack exited on `EFBIG` at ten virtual seconds, reported
itself clean, and lived a whole version that way: nothing asked whether the program had run at all.

A *subject* is a pack's `[check]` table or a seeded defect's `bug.toml`; a *check* is one of eleven
properties of runs, the same code for every subject ([`API.md`](./API.md) §7.9). `works` comes first
and is the one v0.3 lacked: enough events, enough virtual time, a probe that was answered.

| Gate | Was | Is |
|---|---|---|
| G4.1 | `g41all.sh` | four `finds` in `examples/chain-toy/bug.toml` |
| G4.2 | `g41.sh scenario.toml 1 200` | `never_finds` in the same file |
| G4.3 | `bohr run --repeat 1000` by hand | `repeats` |
| G4.5 | `g45.sh` | `costs` with `metric = "events"` |
| G5.1 | not automated | `works` and `adversity` in each pack |
| G5.2 | `walls5.sh` | `walls` in each pack |
| G5.4 | a script | `finds` and `never_finds` in `testdata/bugs/alloc_unchecked/bug.toml` |
| G3.6 | a test in `hunt.rs` | `refuses_changed` |

`crates/bohr-cli/tests/subjects.rs` reads every subject rather than listing them: the declarations
must parse, every seeded defect with a scenario must have a `bug.toml`, and the quick checks hold on
every built defect in `make test`; every check in full runs in `make test-hunting`. Packs are checked
by `bohr check <pack>` in the Linux gate, because their binaries are built by the pack and are not
in a checkout.

What this does not do: write a bound. `at_least = 30` is the subject author's, written before the
run; a check that fails is reported with the bound it missed, never moved to meet the measurement.

A new target or defect is therefore files and not code: a subject declares, and eleven checks that
already exist hold it to what it declared.

## 17. Golden guests

**A golden guest is not a test of a program, but a known load on one part of bohr.** One program,
`testdata/golden/golden.c`, has a mode per subsystem and does exactly the number of operations it
is given; `testdata/golden/golden.toml` lists the workloads and holds each to the same checks. What
the packs measure is bohr and a client together, and a slowdown there names neither the subsystem
nor the cause; a golden workload names both.

| Workload | Part of bohr it loads |
|---|---|
| `syscall` | the stop path: seccomp, ptrace, the kernel loop, the trace writer |
| `clock` | the clock model |
| `write`, `read` | the file model |
| `fsync`, `bigfile` | sync: disk images and the snapshots that share their blocks |
| `mmap` | mappings and the write-back of mapped pages |
| `pipe` | pipes, which a world with a `[cluster]` models as a stream pair |
| `futex`, `threads` | futex waits and wakes, scheduling, the task books |
| `epoll` | readiness: epoll and eventfd |
| `socketpair`, `udp` | sockets in a node, the network between nodes |
| `alloc` | the filter: what passes without a stop |
| `compute` | the floor: a guest that asks nothing |

Every workload is held to `works`, `repeats` and `replays`, and the calm ones to `performs`:
microseconds per operation with startup taken out, the same natively, stops, events and bytes of
log per operation, the supervisor's peak memory and its processor time. Five more entries put a
workload under the faults of its own subsystem — a disk answering `EIO`, kills, refused memory,
stepped clocks, dropped datagrams — and ask that the faults land and the runs still repeat.

`make golden` runs the lot in about seventy seconds on four cores. The bounds are ceilings at about
twice the first measurement on that machine, written in the file beside the workload.

What this does not do: stand in for a client. A golden workload is one load at a time, and a client
mixes them; the packs remain where a release is measured, and the golden set is where a cost is
explained.

The golden set is therefore the place a change to any part of bohr is measured first: a regression
in a subsystem shows as its own workload's line, before any client is built.

