# Changelog

Written for someone deciding whether to upgrade. Format and section order in
[`docs/repo/release.md`](./docs/repo/release.md).

Four numbers version independently and are recorded in every bundle: `tool_version`,
`tape_abi`, `bundle_format`, `sdk_schema` ([`docs/API.md`](./docs/API.md) §9).

## v0.3.0 — one command

A campaign against a real client used to be a scenario file, a binary built by hand at a path on
one machine, and a script that swept seeds. Now it is one command. It is not published.

| Number | Value |
|---|---|
| `tool_version` | `0.3.0` |
| `tape_abi` | `1` |
| `bundle_format` | `2` |
| `sdk_schema` | `1` |

Every stored bundle replays to its digest. Figures are under `[v0_3.*]` and `[walls.*]` in
[`benches/baseline.toml`](./benches/baseline.toml). `make pr`, `make gate` (12 of 12) and the
mutation harness (13 of 13) are green; three gates of this version fail as written, and are listed
below with what they measured.

### A campaign from one command

- **Target packs**: `targets/<name>/target.toml` names where a client comes from, the command that
  builds it, the binary's blake3 digest, the scenario and the oracle. A binary whose digest differs
  is refused before anything runs, with both digests named. Builds write this machine's paths out
  of the binary: `redb-hunt` built in another directory is the same file, byte for byte.
- **The first packs**: `redb`, `reth` (built statically from `paradigmxyz/reth`) and `op-reth`
  (from `rust/op-reth` in `ethereum-optimism/optimism`).
- **`bohr hunt <pack>`**: checks or builds the binary, runs the pack's scenario as a campaign or
  over `--seeds a..b`, with `--no-faults` for checking an oracle on runs nothing was done to.
- **Campaigns that resume**: `--resume` writes the whole campaign down — corpus, coverage,
  findings, credit — and a campaign stopped at 50 runs and resumed to 100 is the 100-run campaign,
  byte for byte. It is written down after every whole generation, in one file renamed into place.
  A 4,096-entry state of 2.1 MB, the corpus at capacity, decodes in memory in 0.74 ms.
- **`bohr findings <dir>`**: groups bundles by the program and the violation's signature, each
  group led by its smallest run. 399 findings of 6 seeded defects form 6 groups.
- **`bohr init -- ./node`**: a first scenario from a survey of the program, with the faults its
  calls can meet and a comment per wall. A dynamic binary gets its libraries copied into a state
  pinned by digest. Five v0.1 programs: each scenario loads and repeats 100 times to one digest.
- **One duration format**: `--time 90s`, `30m`, `1h`, `2d`, or bare seconds.

### Speed

- **The supervisor runs on its guest's core.** Every intercepted call is two stops, and between
  cores each was a cross-core wake-up: 200,000 `getpid` calls took 12.5 s, and take 1.55 s. One
  intercepted call costs 7.7 µs, against 58.8 µs at v0.2.0 on the same host.
- **Each campaign worker on a core of its own**, its guests beside it. Against `redb`, 256 runs at
  one worker went from 3.04 to 12.33 runs a second, and at eight from 8.42 to 57.11; the summaries
  at one and eight workers are still byte for byte equal.
- **Not yet linear in workers.** Ten minutes of `bohr hunt redb`: 4,480 runs at one worker, 19,552
  at eight, 4.36 times as many against a bound of 6. A generation of 32 waits for its slowest run,
  and every generation starts its workers again.

### The model

- **`poll` over sockets in a cluster** is answered from the readiness the `epoll` family computes
  and blocks until a descriptor is ready or the timeout passes. It stopped reth, op-reth 2.6.1 and
  Lighthouse at their start. Surveyed for a minute of virtual time, reth and op-reth now run a
  development chain to the end of it, 335,455 and 334,944 events.

### What it does not do

- **Report a client's own numbers.** A campaign's summary counts runs, adversity and findings; the
  acknowledgements a client checks are its output, which no summary reads.
- **Ship packs for `geth`, CometBFT, Lighthouse or a Solana validator.** Their walls are in the
  table, and the model does not carry them yet.
- **Accept a binary nobody can check.** A pack builds from a pinned commit or takes a binary with
  its digest, and `--binary` replaces the pack's binary in the scenario so the checked file is the
  one that runs.
- **Build the same file on another host.** In a fresh `ubuntu:24.04` the one command installed the
  toolchain, built `redb-hunt` and refused it: another digest, because a static build links that
  host's C library and compiles the coverage runtime with its compiler. A pinned build image is
  what makes a digest portable, and there is none yet.
- **Group a real client's findings.** 620 seeds of `reth` under drawn crashes found nothing to
  group; grouping is measured on six seeded defects.
- **Scale linearly in workers**: 4.36× at eight, against 6 (Speed, above).

## v0.2.0 — the engine, honest

Not new reach but five things v0.1 said about itself and did not make true. It is not published.

| Number | Value |
|---|---|
| `tool_version` | `0.2.0` |
| `tape_abi` | `1` |
| `bundle_format` | `2` |
| `sdk_schema` | `1` |

Every stored bundle replays to its digest: nothing here breaks one. Figures are in
[`benches/baseline.toml`](./benches/baseline.toml) under `[v0_2.*]` and `[walls.*]`.

Gates: `make pr`, `make gate` (12 of 12) and the mutation harness (13 of 13 injections caught) are
green on the release tree.

### Clocks fail during a run

- **`clock_step` and `clock_drift`**, scheduled or drawn under `[faults] draw`. A step moves wall
  time (`CLOCK_REALTIME`, `gettimeofday`, `time`) back or forward and never monotonic time; a drift
  changes a node's rate within ±200 ppm from its moment on. Both kinds the trace format named are
  now emitted.
- **A defect only a step reaches**: `testdata/bugs/lease_clock`, a lease measured on the wall clock.
  Found on 25 seeds of 60 with clock faults drawn, on 0 of 60 without; 1,000 repeats, one digest.

### Files through `io_uring`

- **`[linux] io_uring = "ring"`** opens a ring and carries `READ`, `WRITE`, `OPENAT`, `CLOSE`,
  `STATX`, `FSYNC` and `UNLINKAT`, each by the route its plain call takes: a write through a ring is
  marked on the disk, lost to a crash and failed by a drawn `EIO` as `pwrite64` is. Links are
  honoured; a polled ring is opened with its poller asleep. `testdata/guests/ring_files`: 1,000
  repeats, one digest, and the same answers as a real kernel.
- The default stays `"absent"`: `ENOSYS` at setup, as before. Other operations, registered
  resources and a submission that would wait end the run under `BOHR-2004`.

### Measured as what it is

- **Startup in milliseconds**: 2.77 ms over a native start, against a bound of 5 ms. A guest that
  lives under 10 ms natively is no longer held to a ratio of two startups. The bench runs guests from
  the host's own filesystem: from a Windows drive under WSL the same start read 6.8 ms, which was the
  mount and not the supervisor.
- **Supervisor stops on every run line** (`N stops`), the number a run costs: 64 writes and a sync
  through one ring are 15 stops, against 72 for the same work in plain calls.
- **Interception got no slower**: 200,000 `getpid` calls, v0.1's build and this one alternately on
  one host, 11.77 s against 11.88 s. Against v0.1's recorded 53.2 µs a call the bound fails, 58.8
  µs, and v0.1's own binary reads the same on this evening's host.
- **The run loop tested in its own crate**: 18 tests over a scripted backend, one per `StopReason`
  and `NodeOp`. They found a killed task whose record the kernel kept for ever, fixed.
- **One table of walls**: `scripts/walls.sh` records, per target, every call the model could not
  answer and the refusal or the hang that stopped it.

### Corrected

- **Absolute wall-clock deadlines** — `pthread_cond_timedwait`, `clock_nanosleep` on
  `CLOCK_REALTIME` — are measured on the wall clock. Before clocks could step the two were equal;
  after a step of +30 s a one-second wait would have lasted 31.

- **`op-reth` did not run at v0.1.0.** Since `66f43a4` it stops at its first `poll` over a socket,
  refused by name; v0.1.0's "333,485 events of its node command" was measured before that. The
  `op-reth` it was measured on, 1.1.5, is the `ethereum-optimism/op-reth` fork, archived since
  2025-02-18; the target is now `rust/op-reth` in `ethereum-optimism/optimism`.
- **`bohr survey` ignored a scenario's horizon** and node to end with, and surveyed a longer run
  than the one it was asked about.

### What it does not do

- **Run a target it could not run before.** The walls are written down, not answered.
- **Preempt a thread that spins without a syscall.** One task runs at a time, until its next
  syscall; a thread waiting on another in a loop never yields, and the run stops making events. The
  Solana validator does this. Nothing preempts at an instruction budget.
- **Turn the ring on by default**, or carry sockets, timeouts or registered buffers through it.
- **Give wall time a date.** It begins at the epoch, so a step back further than the run has lasted
  holds it there until monotonic time catches up. A realistic epoch changes every stored digest.

## v0.1.0 — the foundation

The first named version: what the engine does, fixed in one place. Built with Rust 1.99.1 on
edition 2024; the SDK crates build on 1.85. It is not published — no
artefacts, no packages, no release tag. The first public release is v1.0.

| Number | Value |
|---|---|
| `tool_version` | `0.1.0` |
| `tape_abi` | `1` |
| `bundle_format` | `2` |
| `sdk_schema` | `1` |

Every figure below is in [`benches/baseline.toml`](./benches/baseline.toml);
[`docs/EVIDENCE.md`](./docs/EVIDENCE.md) gives the key and the command behind each.

### Runs a program deterministically

- **An unmodified Linux binary, one seed, one execution.** Time, randomness, scheduling, the network
  and the disk are drawn from the entropy tape; the same seed gives the same trace digest, and the
  digest is what "the same run" means. A five-node `raft-rs` cluster: 100 runs, 16,016 events each,
  one digest.
- **115 syscalls modelled** on x86-64. Anything else ends the run with a named error rather than
  reaching the host.
- **Threads under a drawn schedule**: uniform choice, or PCT with its probabilistic guarantee. On a
  depth-2 race PCT reaches the defect on 60 seeds of 60, uniform on 47.
- **Programs as released**: `redb` 2.6.3, `tokio` 1.53.1 and a TCP service on it, `libmdbx` through
  its Rust binding, and a 103 MB Ethereum execution client through 333,485 events of its node
  command.

### A cluster in one world

- Per-node clocks with drawn offset and drift, per-node filesystems and futex spaces.
- A virtual network that drops, duplicates, reorders, delays and partitions.
- A virtual disk with strict `fsync` semantics, torn writes, stalls, full and failing devices, and
  crash snapshots.
- Node lifecycle as a fault: kill, freeze, thaw, restart, restart from an older synced image, and a
  reorg that rewinds a cluster to a common ancestor.
- A prepared filesystem as a run input, recorded in the bundle by digest and checked before replay.

### Judges a run

- Crashes, hangs and deadlocks, with no annotation at all.
- SDKs in Rust, Python, C# and Go, held to one conformance suite; a C runtime fills the coverage map
  for any program built with sanitizer coverage. `always` is judged per run;
  `sometimes` and `reachable` per campaign, and a property nothing satisfied is reported as a result.
- A guest that says nothing about itself: a run horizon, so a server that never exits is judged; a
  cluster that healed and committed nothing; an answer given before it was durable and lost on
  restart. Against `redb` 2.6.3: 200 runs, 330 crashes, 12,594 acknowledgements verified of 12,594.
- Differential testing: one tape, several implementations of one specification, a finding where
  their output differs.

### Searches, reproduces, shrinks

- **Campaigns**: a corpus, coverage-guided mutation of the tape and drawn fault schedules. A
  campaign is a pure function of its master seed; 131,000 runs per hour against `redb`.
- **`.bohr` bundles and `bohr replay`**: a run reproduces on another machine or is refused with the
  reason named. 9 of 9 bundles written by a Windows build replayed on Linux.
- **`bohr reduce`**: delta debugging over configuration, delivered events and tape, under a replay
  budget. A fixture's 49,477 events reduced to 18 in 20 replays.
- **`bohr report`**: one static HTML file — the point of violation, a timeline per node, the faults
  that fired, the draws that mattered; `--redact` rewrites the bundle so it still replays.
- **`bohr replay --break-at` and `--gdb`**: a reproduced run stopped at a named event and handed to
  an ordinary debugger.
- `bohr trace` (text or JSON Lines), `bohr note`, `bohr survey` (every unmodelled syscall a program
  asks for, in one pass), `bohr doctor` (what this host can and cannot do).

### Found

- Two defects in widely deployed open-source projects, reproduced by bundle on every seed and held
  with their maintainers until fixed.

### What it does not do

- **Run outside Linux x86-64.** The core builds and tests on macOS and Windows; the program under
  test needs Linux, kernel 5.4 or newer.
- **Model `io_uring`, or any syscall outside the 115.** Such a program stops at the first unmodelled
  call, by design.
- **Inject every catalogued fault.** Disk write reordering, a clock step and a change of drift
  during a run are named in the fault catalogue and never fired; clocks are skewed once, before the
  run begins.
- **Place faults better than chance.** Guided targeting loses to uniform on the defect it was
  measured against, and the claim is withdrawn ([`docs/API.md`](./docs/API.md) §5.4).
- **Shrink a real client's run within a small budget.** 20 replays took 1,222,888 events to
  1,189,704.
- **Hold to ≤ 5× native everywhere.** 3.6× on `redb`'s test suite, measured outside the tree; 6.6×
  on the same database under the Windows subsystem for Linux.
- **Contain a hostile program.** Bohr is not a security sandbox. `RDRAND` is a documented hole in
  determinism ([`docs/DETERMINISM.md`](./docs/DETERMINISM.md)).

Nothing here is stable: the tape ABI, the bundle format and the command line change before v1.0,
and every change that breaks a stored bundle bumps the number that says so.
