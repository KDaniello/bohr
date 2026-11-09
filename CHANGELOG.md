# Changelog

Written for someone deciding whether to upgrade. Format and section order in
[`docs/repo/release.md`](./docs/repo/release.md).

Four numbers version independently and are recorded in every bundle: `tool_version`,
`tape_abi`, `bundle_format`, `sdk_schema` ([`docs/API.md`](./docs/API.md) §9).

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

- **Run a target it could not run before.** The walls are written down, not answered: that is v0.3
  for `poll` over sockets and v0.5 for the rest.
- **Preempt a thread that spins without a syscall.** One task runs at a time, until its next
  syscall; a thread waiting on another in a loop never yields, and the run stops making events. The
  Solana validator does this. Preemption at an instruction budget is v0.5.
- **Turn the ring on by default**, or carry sockets, timeouts or registered buffers through it.
- **Give wall time a date.** It begins at the epoch, so a step back further than the run has lasted
  holds it there until monotonic time catches up. A realistic epoch changes every stored digest and
  is v0.6's.

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
