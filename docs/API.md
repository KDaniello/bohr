# API and data paths

Living document. It fixes every contract that crosses a boundary: the entropy tape ABI, the
determinism backend trait, the trace and bundle formats, the world configuration, the SDK record
schema, the command-line interface, the error catalog and the versioning rules.

Why this document exists separately from [`ARCHITECTURE.md`](./ARCHITECTURE.md): architecture
answers *why the components are shaped this way*; this document answers *what exactly goes across
the wire*, in enough detail that a second implementation could be written from it. Everything in
here is a promise to somebody — a stored bundle, a customer's SDK, a CI pipeline reading an exit
code.

Neighbouring documents:
[`ARCHITECTURE.md`](./ARCHITECTURE.md) ·
[`TESTING.md`](./TESTING.md) ·
[`DETERMINISM.md`](./DETERMINISM.md) ·
[`PLATFORMS.md`](./PLATFORMS.md).

---

## 1. Scope and stability

| Surface | Audience | Stability | Versioned by |
|---|---|---|---|
| §2 Entropy tape ABI | stored corpora and bundles | **frozen per major** | `tape_abi` |
| §3 Backend boundary | our own crates, future hypervisor backend | semver of `bohr-backend` | crate semver |
| §4 Trace and bundle format | stored findings, third-party readers | **frozen per major** | `bundle_format` |
| §5 World configuration | users' config files | semver, additive | tool semver |
| §6 SDK records | customer builds, Antithesis interop | **frozen** for the compatible subset | `sdk_schema` |
| §7 CLI | scripts, CI pipelines | semver, flags additive | tool semver |
| §8 Error codes | CI logs, issues | **never reused** | — |

Three rules that apply to all of them:

1. **Little-endian, everywhere.** All binary formats are little-endian regardless of host. There is
   no big-endian host in our support matrix, and defining it away costs nothing today and saves an
   ambiguity later.
2. **No floating point in any decision path or any serialised value that feeds a decision.**
   `f64::ln` and friends are libm-backed and are permitted to differ between platforms and libc
   versions; a delay computed with floating point is a cross-machine reproduction failure waiting to
   happen (goal T4). Distributions are computed in fixed-point integer arithmetic with our own
   frozen implementations. Floating point is allowed only in reports and metrics, which never feed
   back into a run.
3. **Unknown is not ignored.** A reader that meets an unknown section, record kind or field either
   has an explicit forward-compatibility rule stated here, or fails with a named error. Silent
   skipping is how a bundle becomes subtly wrong.

---

## 2. The entropy tape

The tape is the single source of nondeterminism ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.1). This
section is its complete specification.

### 2.1 Logical model

```
tape = prefix ‖ extension

prefix     — a finite byte string; corpus material; what mutation operates on
extension  — a deterministic keystream, used once the prefix is exhausted; re-keyed every
             128 GiB so that it goes as far as an offset can be counted
```

A cursor is a byte offset. Drawing advances it. Nothing ever rewinds a cursor during a run.

The extension exists so that "tape exhausted" is not a failure mode. A guest that iterates more than
expected keeps drawing, and the bytes it gets are still a pure function of the seed.

### 2.2 Container format (`.tape`)

A 64-byte header followed by the prefix bytes. Little-endian.

| Offset | Size | Field | Notes |
|---:|---:|---|---|
| 0 | 4 | `magic` | `b"BTP\x01"` |
| 4 | 2 | `format_version` | container version, currently `1` |
| 6 | 2 | `tape_abi` | draw ABI version, currently `2` (§2.3) |
| 8 | 8 | `seed` | `u64`, drives the extension |
| 16 | 8 | `prefix_len` | bytes of prefix that follow |
| 24 | 32 | `prefix_hash` | `blake3(prefix)`, verified on load |
| 56 | 8 | `reserved` | must be zero; a non-zero value is `BOHR-4003` |

The header is checked in full on load: a `format_version` this tool does not implement is
`BOHR-4001`, and a container whose length disagrees with `prefix_len` in either direction is
`BOHR-4002`.

A `.tape` file is rarely used standalone — the canonical container is a `.bohr` bundle (§4), which
embeds exactly these bytes as section `TAPE`. The standalone form exists for corpus directories,
where a fuzzer-style flat file of raw bytes is the most convenient thing to mutate.

### 2.3 The extension keystream

```
key    = blake3(b"bohr/tape/extension/v1" ‖ seed_le_bytes)      // 32 bytes
stream = ChaCha8(key, nonce = 0, counter = 0)
extension[i] = stream[i]                                        // i counted from prefix_len
```

ChaCha8 rather than a general-purpose PRNG for three reasons: it is seekable (a cursor can jump to
any offset in O(1), which the reducer needs), it is specified bit-exactly so our implementation and
any future one agree, and it has no platform-dependent behaviour.

**This construction is frozen by `tape_abi`.** Changing the key derivation string, the cipher or the
nonce changes every run of every stored seed and therefore requires a `tape_abi` bump.

### 2.4 Draw ABI

Every draw is a pure function of `(cursor position, tape bytes)`. Implementations must match exactly;
these definitions are normative.

```rust
/// Raw bytes into the caller's buffer. The primitive every other draw is built from.
fn draw_into(&mut self, out: &mut [u8]);

/// n bytes little-endian, zero-extended to u64. Panics for n > 8.
fn draw_uint(&mut self, n: usize) -> u64;

/// Uniform in 0..n.
fn draw_range(&mut self, n: u64) -> u64;

/// True with probability num/den.
fn draw_bool(&mut self, num: u32, den: u32) -> bool;

/// Index into weights, proportional to weight. Zero-weight entries are unreachable.
fn draw_weighted(&mut self, weights: &[u32]) -> usize;

/// Exponentially distributed value with the given mean, in fixed point.
fn draw_exponential(&mut self, mean: u64) -> u64;
```

**The governing rule: consumption is a function of the request alone, never of the drawn value.**

Search is tape mutation ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.1), so a flipped byte must change
one decision and leave every later draw reading from the same offset. The moment consumption depends
on what was drawn, one flip shifts the alignment of everything downstream and local search is gone.
Every definition below exists to hold that rule.

**`draw_range(n)` — normative definition.**

```
if n == 0  -> caller error, BOHR-5002 (a range draw of zero is always a bug in the caller)
if n == 1  -> return 0, consuming ZERO bytes
w = number of bytes needed to hold (n-1)             // 1..=8
return draw_uint(w) % n
```

Three decisions inside that block, each with a reason:

- **`n == 1` consumes zero bytes.** A choice with one option is not a decision. Consuming a byte for
  it would make tape layout depend on the number of degenerate choices, which changes whenever a
  cluster has one node instead of two — and that would make corpus entries far more brittle than
  they need to be.
- **Fixed width `w`, not minimal width.** The number of bytes consumed depends only on `n`. Measured
  against a varint alternative: minimal width saves 8% of tape and is worse on every other axis.
- **Plain modulo, with no rejection loop.** Rejection sampling would consume another `w` bytes on
  every retry, which makes consumption depend on the drawn value and defeats fixed width. Without
  it, a byte flip changes exactly one decision and alignment never drifts. The price is a bounded
  skew of `(2^8w mod n) / 2^8w`: the first `2^8w mod n` values come up twice as often as the rest.
  That is zero whenever `n` divides a power of 256, 1.6% at `n = 7` — and 49.6% at `n = 129`, the
  worst case for a width, where 127 values of 129 are twice as likely as the other two. What it
  costs is search coverage, never reproduction, because the mapping is a pure function of the
  bytes. Removing it means widening the draw, which re-reads every stored tape, so it belongs to a
  change of the draw ABI.

**`draw_bool(num, den)` — a fixed two bytes, independent of the probability, except at the two
ends.** `num = 0` and `num >= den` consume nothing: a certainty is not a decision, by the same
reasoning as a one-option choice. `draw_weighted` does the same where one weight is non-zero. This
is the one discontinuity in the rule that consumption does not depend on configuration — editing a
probability *to* or *from* a certainty does re-lay-out a stored corpus — and it is deliberate.

```
if num == 0    -> false, consuming ZERO bytes
if num >= den  -> true,  consuming ZERO bytes
threshold = (num << 16) / den                        // integer division, 0..=65536
return draw_uint(2) < threshold
```

**Not** `draw_range(den) < num`. That form takes its width from the denominator — 1 byte for
`"1/50"`, 2 for `"1/1000"`, 3 for `"1/100000"` — so a probability literal in a configuration file
determines tape layout, and editing one re-interprets every tape in a corpus. Scaling a fixed 16-bit
draw decouples layout from configuration entirely, for 2% more tape.

The consequence is a granularity floor: **a denominator above 65536 is rejected at configuration
load** with `BOHR-1003` rather than silently rounded to never (§5.2). A fault that fires once per
million decisions would never be explored in a campaign anyway; refusing it is more honest than
accepting it and not delivering it.

**`draw_weighted(weights)` — a fixed four bytes, independent of the weights.**

```
sum = Σ weights                                      // must be > 0, else BOHR-5002
if exactly one weight is non-zero -> that index, consuming ZERO bytes
target = (draw_uint(4) * sum) >> 32                  // u64 intermediate, no overflow
walk the slice accumulating until the running total exceeds target
```

Same hazard, same fix: `draw_range(sum)` would tie width to weight magnitudes, which are
configuration. The walk order is slice order; the caller is responsible for the slice being in a
deterministic order, which is why weight tables are `Vec`s built from `BTreeMap`s and never from
`HashMap`s.

> The numbers quoted above are reproduced by `scripts/experiments/draw-abi.py`, which implements
> every candidate rule and measures tape size, mutation locality and alignment drift on a stated
> draw mix. It runs standalone. The mix is an assumption; the ordering of the candidates does not
> depend on it.

**`draw_exponential(mean)`** uses fixed-point arithmetic only:

```
u = draw_range(1 << 32)                     // u in 0..2^32
// -mean * ln(1 - u/2^32), evaluated with a frozen 64-bit fixed-point ln table
return fixed::exp_inverse_cdf(mean, u)
```

The table and the interpolation are part of `bohr-tape` and are covered by a golden test with
vectors committed in `testdata/tape/exponential.golden`. No `f64` is involved at any point.

> Because consumption is fixed per draw kind, this draw always costs four bytes, and the journal
> records the uniform rather than the mapped value, so a stored journal stays regenerable from the
> tape alone.

### 2.5 Draw sites

Every draw names its site. Sites are a stable enumeration; the numeric value is part of the journal
format and is never reused.

| Id | Site | Drawn by |
|---:|---|---|
| 1 | `SchedPick` | scheduler: which runnable task runs next |
| 2 | `SchedPriority` | PCT: initial priorities |
| 3 | `SchedSwitchPoint` | PCT: where priority changes happen |
| 4 | `SyncWake` | which waiter a synchronisation primitive releases |
| 10 | `NetDeliver` | network: deliver / drop / duplicate |
| 11 | `NetDelay` | network: delivery delay |
| 12 | `NetReorder` | network: reordering within a link |
| 13 | `NetPartition` | network: partition events |
| 14 | `NetSocket` | network: which socket of a shared port receives |
| 20 | `DiskLatency` | disk: operation stall |
| 21 | `DiskTear` | disk: tear a write at a crash |
| 22 | `DiskReorder` | disk: pre-`fsync` write reordering |
| 23 | `DiskError` | disk: `ENOSPC` / `EIO` injection |
| 30 | `NodeFault` | node lifecycle: kill / freeze / restart |
| 31 | `ClockSkew` | node clock: its offset from the world clock, and its rate |
| 40 | `GuestRandom` | `getrandom`, `/dev/urandom`, `AT_RANDOM`, `RDRAND` trampoline |
| 41 | `SdkRandom` | SDK `random_bytes` |
| 42 | `SdkFaultPoint` | SDK `fault_point`: fire or not |
| 50 | `Reserved` | free ids for future families; never a draw |

A site is not a "random stream": all sites share the one cursor. Separate streams per site would be
easier to reason about and would make mutation useless, because a mutation would then only ever
affect one subsystem.

### 2.6 The draw journal

Produced during a run, never consumed as input ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.3).
One fixed-size record per draw:

```rust
#[repr(C)]
struct DrawRecord {
    offset: u64,     // tape offset the draw started at
    event:  u64,     // EventId current when the draw happened
    value:  u64,     // the value produced; for a raw byte draw, its first 8 bytes little-endian
    site:   u16,     // DrawSite
    _pad:   u16,     // must be zero
    len:    u32,     // bytes consumed; 0 for a degenerate choice
}
```

`len` is 32 bits because a raw draw serving `getrandom` can be a whole page, and a width field must
be able to represent the draw it describes.

32 bytes per record, stored compressed in bundle section `DRAW_JOURNAL`. `bohr replay` **regenerates**
the journal and compares it to the stored one; a mismatch is `BOHR-2010` and means the tape ABI or a
draw site changed under a stored bundle — exactly the situation we want to be loud about rather than
debug later.

---

## 3. The backend boundary

The interface between the interceptor and everything above it. Above the boundary nothing knows
how determinism is achieved.

### 3.1 The trait

```rust
/// A source of guest execution that can be stopped, inspected and resumed deterministically.
///
/// Implementations: `bohr-backend-ptrace` (Linux syscall interception),
/// `bohr-backend-null` (in-process synthetic guests), and later a hypervisor backend.
/// Nothing above this trait may depend on which one is in use.
pub trait DeterminismBackend: Send {
    fn capabilities(&self) -> BackendCapabilities;

    /// Creates the initial task of a node. Returns once the guest is stopped before its
    /// first instruction, so the supervisor can install the world before anything runs.
    fn spawn(&mut self, node: NodeId, spec: &GuestSpec) -> Result<TaskId, BackendError>;

    /// Runs until exactly one task stops, or reports that none remain. At most one task is
    /// ever executing. `None` is the normal end of a run, not a failure.
    fn next_stop(&mut self) -> Result<Option<Stop>, BackendError>;

    fn resume(&mut self, task: TaskId, action: Resume) -> Result<(), BackendError>;

    fn read_mem(&mut self, task: TaskId, addr: GuestAddr, out: &mut [u8]) -> Result<(), BackendError>;
    fn write_mem(&mut self, task: TaskId, addr: GuestAddr, data: &[u8]) -> Result<(), BackendError>;

    fn regs(&mut self, task: TaskId) -> Result<Regs, BackendError>;
    fn set_regs(&mut self, task: TaskId, regs: &Regs) -> Result<(), BackendError>;   // inverse of regs

    /// Executable and writable mappings, used by the RDRAND scan and by coverage.
    fn mappings(&mut self, task: TaskId) -> Result<Vec<Mapping>, BackendError>;

    /// Ends a task. Nothing more is reported about it afterwards, its exit included.
    fn kill(&mut self, task: TaskId) -> Result<(), BackendError>;

    /// Holds every guest as it stands, between two picks, so the run can go on from here more
    /// than once. `Ok(None)` from a backend whose capabilities say `checkpoints: false`.
    fn checkpoint(&mut self) -> Result<Option<CheckpointId>, BackendError> { Ok(None) }

    /// Puts every guest back as it stood at `id`, keeping the checkpoint. `Ok(false)` if it cannot.
    fn restore(&mut self, id: CheckpointId) -> Result<bool, BackendError> { Ok(false) }
}
```

`checkpoint` and `restore` are what branching a run from a prefix needs: a reduction or a campaign
replays a shared prefix once and goes on from it many times. Both backends in this tree answer with
the default, and `capabilities()` says so before anyone asks.

`set_regs` writes back every field `regs` hands out, arguments included: a boundary that exposes a
field and then drops it on the way in loses a change without reporting it.

Note what is **absent**: no `pid`, no `signal`, no `waitpid`, no `/proc`, no seccomp. The vocabulary
is guest operations. This is the boundary of §5.2, and the CI grep in
[`ARCHITECTURE.md`](./ARCHITECTURE.md) §8 rule 14 enforces it.

### 3.2 Stops

```rust
pub struct Stop {
    pub task: TaskId,
    pub kind: StopKind,
}

pub enum StopKind {
    /// The guest is about to execute a syscall. It has not happened yet.
    SyscallEntry { nr: u32, args: [u64; 6] },
    /// Only delivered when the supervisor let the syscall through to the host.
    SyscallExit { ret: i64 },
    /// RDTSC / RDTSCP trapped via PR_SET_TSC.
    TimestampCounter { rdtscp: bool },
    /// CPUID trapped via ARCH_SET_CPUID.
    CpuIdentification { leaf: u32, subleaf: u32 },
    /// A new task appeared (clone/fork/vfork). It is stopped before its first instruction.
    TaskCreated { parent: TaskId, shares_memory: bool },
    TaskExited { status: ExitStatus },
    /// A signal the guest would receive. The supervisor decides whether to deliver it.
    Signal { signal: GuestSignal, from: SignalOrigin },
    /// The instruction budget from `Resume::ContinueWithBudget` ran out.
    BudgetExhausted { consumed: u64 },
    /// A fault the guest did not handle. Usually a finding.
    MemoryFault { addr: GuestAddr, kind: FaultKind },
}
```

### 3.3 Resume actions

```rust
pub enum Resume {
    /// Let the guest run until the next stop.
    Continue,
    /// Run until the next stop or until `budget` progress units elapse, whichever is first.
    /// Progress units are retired conditional branches where available, syscalls otherwise;
    /// which one is in use is reported by `BackendCapabilities::progress_unit`.
    ContinueWithBudget { budget: u64 },
    /// Do not execute the syscall. Return `ret` to the guest instead.
    /// This is the normal path: almost every syscall is emulated, never executed.
    Emulate { ret: i64 },
    /// Execute a different syscall than the guest asked for.
    Rewrite { nr: u32, args: [u64; 6] },
    /// Deliver a signal to the guest as part of its resumption.
    DeliverSignal { signal: GuestSignal },
    /// End the task. The kernel records its exit rather than waiting to be told again.
    Terminate,
}
```

`Emulate` being the default path is the design: we are a virtual world, not a filter. A syscall that
reaches the host kernel is an exception that must be justified per syscall in the support table
([`PLATFORMS.md`](./PLATFORMS.md) §4).

### 3.4 Capabilities and degradation

```rust
pub struct BackendCapabilities {
    pub name: &'static str,               // "ptrace", "null", "kvm"
    pub traps_rdtsc: bool,
    pub traps_cpuid: bool,
    pub traps_rdrand: bool,               // false for ptrace; the honest hole, §5.8
    pub progress_unit: ProgressUnit,      // Branches | Syscalls
    pub supports_snapshot: bool,          // world fork/restore; false until the KVM backend
    pub max_tasks: u32,
}
```

The kernel reads capabilities once at startup and writes them into the bundle's host profile. A
`false` where the run needed `true` is a **degradation flag**, not a crash: the run proceeds and its
header records `determinism: degraded` with the specific reason, and every report derived from it
carries the flag. What we never do is report a degraded run as clean.

---

## 4. Trace and bundle formats

### 4.1 Bundle container (`.bohr`)

A section table followed by section payloads. Sections are independently compressed with zstd and
independently hashed, so a truncated or corrupted bundle reports *which part* is bad.

```
Header (32 bytes)
  0   8   magic            b"BOHRBNDL"
  8   2   bundle_format    u16, currently 2
 10   2   section_count    u16
 12   4   flags            u32, currently 0
 16   16  run_id           128-bit, identifies this bundle forever; see below

Section table, `section_count` entries of 64 bytes:
  0   4   id               u32, see the table below
  4   4   flags            u32, bit 0 = zstd compressed
  8   8   offset           u64, from the start of the file
 16   8   stored_len       u64
 24   8   original_len     u64
 32   32  hash             blake3 of the ORIGINAL (uncompressed) bytes
```

| Id | Section | Content | Required |
|---:|---|---|:---:|
| 1 | `MANIFEST` | JSON, §4.2 | yes |
| 2 | `TAPE` | a complete `.tape` file, §2.2 | yes |
| 3 | `EVENTS` | the event log, §4.3 | yes |
| 4 | `SDK_RECORDS` | JSON Lines as emitted by the guest, §6.3 | no |
| 5 | `DRAW_JOURNAL` | `DrawRecord` array, §2.6 | no |
| 6 | `COVERAGE` | edge bitmap, §5.5 | no |
| 7 | `BINARIES` | JSON: fingerprints of every mapped executable object | yes |
| 8 | `HOST_PROFILE` | JSON: what we *presented* to the guest, §4.5 | yes |
| 9 | `CONFIG` | the effective merged configuration, §5 | yes |
| 10 | `NOTES` | free-form UTF-8 attached by `bohr note` | no |

**`run_id` is derived, not random**: `blake3(b"bohr/bundle/run-id/v1" ‖ seed_le ‖ trace_digest)`
truncated to 16 bytes. A random identifier would be a nondeterminism source inside the supervisor,
which §5.1 forbids without exception, and would make two byte-identical runs produce
byte-different bundles. Derivation also makes the identity say something true: two runs that are the
same run carry the same `run_id`.

**A recorded length is a claim, not an instruction.** `original_len` is what the section says it
unpacks to, and a reader must not allocate on the strength of it: a section is unpacked
incrementally, stopped as soon as it passes the length recorded for it, and refused outright above
**1 GiB**, which is the most a reader is required to hold at once. A file written to be opened
rather than read therefore costs a refusal rather than a machine.

**Unknown section ids are preserved, not dropped.** A tool that rewrites a bundle (`bohr reduce`)
copies sections it does not understand. Dropping them would mean a newer bundle silently loses data
when passed through an older tool.

### 4.2 Manifest

```json
{
  "bundle_format": 1,
  "tape_abi": 2,
  "sdk_schema": 1,
  "tool_version": "0.1.0",
  "run_id": "0f3a…",
  "created_utc": "2026-08-09T10:00:00Z",
  "seed": 12345678901234567890,
  "outcome": {
    "kind": "property_violated",
    "detail": "always(\"log is monotonic\") failed at src/log.rs:214"
  },
  "outcome_event": 40122,
  "determinism": {
    "level": "degraded",
    "reasons": ["rdrand_found_unpatched:libcrypto.so.3+0x2f1a0"]
  },
  "trace_digest": "blake3:9c2d…",
  "counters": {
    "events": 40551, "syscalls": 30112, "draws": 8801,
    "ticks": 12000000000, "tasks": 7, "nodes": 5
  },
  "policy": { "scheduler": "pct", "pct_depth": 3, "switch_points": "syscall+futex" },
  "provenance": {
    "world_model": { "id": "distributed", "version": 1 },
    "oracle": { "id": "properties+crash", "version": 1 },
    "backend": "ptrace"
  },
  "executions": 1,
  "reduced_from": null
}
```

`outcome.kind` is a closed enumeration: `clean`, `property_violated`, `guest_crashed`, `guest_hung`,
`unsupported_syscall`, `implementations_disagreed`, `internal_error`. `reduced_from` carries the
`run_id` of the pre-reduction bundle so a minimal reproducer can always be traced back to what
produced it.

**`provenance` is mandatory and is checked before replay.** A bundle produced by one world model or
one oracle is **refused** by a tool configured with another, with `BOHR-4006` naming both — it is not
replayed "best effort". Two domains model the same syscall differently on purpose; feeding a bundle
from one into the other produces a run that is plausible, self-consistent and wrong, which is exactly
the failure mode §5.9 exists to prevent ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.16).

**`executions`** is 1 for an ordinary run and N for a differential one, where one tape drove N
implementations and the oracle compared them ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.17). With
`executions > 1`, sections `EVENTS`, `SDK_RECORDS` and `COVERAGE` each carry N streams tagged by
execution index, and `trace_digest` becomes the digest of the ordered list of per-execution digests.
A reader that does not understand `executions > 1` must refuse the bundle rather than read the first
stream and report success.

### 4.3 Event log

Length-prefixed CBOR records, streamed, framed so the log is readable even when truncated by a
crash:

```
per record:  u32 length  ‖  CBOR payload  ‖  u32 length (repeated)
```

The trailing length repeat lets a reader scan backwards from the end of a truncated file and
recover every complete record — a supervisor that dies mid-run must still leave a usable trace,
because the runs that kill the supervisor are the interesting ones.

```rust
pub struct Event {
    pub id: EventId,        // monotonic from 0, run-local
    pub tick: Tick,         // virtual nanoseconds
    pub node: NodeId,
    pub task: TaskId,
    pub kind: EventKind,
}

pub enum EventKind {
    Syscall      { nr: u32, args: [u64; 6], ret: i64, emulated: bool,
                   data_digest: Option<[u8; 8]> },
    Schedule     { from: TaskId, to: TaskId, reason: SwitchReason },
    TaskSpawned  { parent: TaskId },
    TaskExited   { status: ExitStatus },
    PacketSent   { from: Endpoint, to: Endpoint, len: u32, digest: [u8; 8] },
    PacketFate   { fate: PacketFate, delay_ticks: u64 },   // Delivered|Dropped|Duplicated|Reordered
    DiskOp       { fd: Fd, op: DiskOp, offset: u64, len: u32, ret: i64 },
    FaultFired   { fault: FaultKind, target: FaultTarget },
    NodeState    { state: NodeState },
    ClockRead    { presented: Tick },
    Assertion    { name: String, kind: AssertionKind, hit: bool, condition: bool },
    SdkRecord    { index: u32 },                            // points into SDK_RECORDS
    Diagnostic   { code: &'static str, message: String },

    /// Emitted by a world model for facts the core has no vocabulary for.
    /// `ns` is the model's registered namespace, `kind` its own event number.
    /// The core never interprets `payload`; it only orders, digests and renders it.
    Domain       { ns: WorldModelId, kind: u32, payload: Vec<u8> },
}
```

**`data_digest` is what makes delivered data part of the run.** Argument registers carry *addresses*,
not contents, so two answers writing different bytes into the same buffer are otherwise
indistinguishable in the trace, and two different runs would share a digest. The field holds a
truncated digest of whatever the world wrote into the guest while answering, and is absent where
nothing was written. The contents themselves stay out of the trace: a bundle records what happened,
not what the guest was told.

**`NodeState` enumerates the node lifecycle: `Running`, `Frozen`, `Dead`, `Restarting`.** Legal
transitions are `Running → Frozen → Running` (freeze and thaw), `Running → Dead` (a kill, or the
node's last task exiting) and `Dead → Restarting → Running` (a restart, clean or from an older
disk snapshot). The world model emits every `NodeState` event and the `FaultFired` that caused it;
the kernel executes the transition — removes or re-spawns tasks, keeps a frozen node out of
scheduling — and emits nothing, so attribution stays with the layer that made the decision. A
restarted node keeps the clock it drew at spawn: a restart re-draws nothing, so it cannot shift
the tape position of any draw after it.

**`Schedule` records execution moving between tasks, and `reason` names what ended the previous
turn**: `SyscallBoundary`, `Blocked`, `Yield`, `TaskExit`, `BudgetExhausted` or `FaultPoint`. The
first dispatch of a run moves execution from nowhere, is not a switch and produces no event; a
dispatch after the running task exited is one, recorded with the exited task as `from` and
`reason: TaskExit`.

**`Domain` is the extension point, and the core enum is closed apart from it.** A second world model
does **not** add variants here — `BlockProposed` and `JointTrajectory` have no business in a crate
that knows nothing about either ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.16). It emits
`Domain { ns, kind, payload }` instead, and supplies a renderer for the report. The payload is
included in the trace digest verbatim, so a domain event is as much a part of "the same run" as a
syscall is; what the core refuses to do is pretend it understands the bytes.

**The log as text, one event per line.** `bohr trace --format json` writes the log as JSON Lines,
and a report that showed only part of a run leaves the same bytes beside itself for the rest — a
reader who has to reconcile two shapes of one run has been given two problems. One object per
line, and every line carries the same envelope whatever the event was:

```json
{"id":40122,"tick":12000000000,"node":3,"task":7,"kind":"assertion",
 "assertion":{"name":"log is monotonic","promise":"always","hit":true,"condition":false}}
```

`id`, `tick`, `node`, `task` and `kind` come first and are always present; the variant's own fields
sit under a key named by `kind`, so a reader that meets an event it has never heard of can still
place it in the run. **Anything that is an address, a register or a digest is a hex string**, because
`JSON.parse` reads numbers as doubles and loses precision above 2^53 — a syscall argument written as
a number would be a value quietly changed by being read. Counts, identifiers and virtual time stay
numbers: they are small, and they are what a reader sorts and filters by.

`--format text` writes `<id> t<tick> n<node> task<task> <what happened>`, one event per line, for
reading and grepping rather than for parsing.

### 4.4 Trace digest — the definition of "the same run"

```
digest = blake3( for each event in order: canonical(event) )
```

`canonical(event)` includes: `id`, `tick`, `node`, `task`, the variant tag, and every field listed
above. It **excludes**, by construction, everything host-derived:

| Excluded | Why |
|---|---|
| wall-clock durations | differ per machine by definition |
| host pids, tids, inode numbers | not virtualised at the host level |
| guest memory addresses of host origin (mmap results we did not choose) | ASLR is off but the loader still varies with libc |
| `Diagnostic` message text | wording changes are not behaviour changes |
| anything under `determinism.reasons` | environment-specific |
| argument registers a syscall does not read | they hold whatever was there before the call |
| the value of an address argument, as against whether it is null | where it points is behaviour; what it equals is this run's memory |

This exclusion list is the operational definition of goal T1 and goal T4 and is therefore itself a
tested contract: `bohr-testkit` has a test that constructs two events differing only in an excluded
field and asserts the digests match, and one differing in an included field and asserts they do not.

**The last two rows are what makes this "the same run" rather than "the same memory".** A syscall is
entered with six registers whatever it takes, and what it does not take is left over — a pointer, in
a program of any size. Measured: an execution client under two coverage map sizes produced 43 608
events, in the same order, with the same returns, whose traces agree line for line, and two
different digests. What each call reads is a fact about Linux, carried per number in
`crates/bohr-trace/assets/syscalls-x86_64.tsv`; a number that table has never heard of is hashed by
all six, which is stricter than necessary and never wrong. What an address points *at* is not lost:
bytes the world wrote into the guest are carried by `data_digest`.

### 4.5 Host profile

What we **presented** to the guest, not what the host actually is. Two different machines producing
the same host profile is the precondition for cross-machine reproduction.

```json
{
  "cpuid": { "vendor": "GenuineIntel", "family": 6, "model": 85, "stepping": 4,
             "features_masked_off": ["rdrand", "rdseed", "tsc_deadline"] },
  "kernel_release_presented": "5.15.0-bohr",
  "page_size": 4096,
  "aslr": "disabled",
  "vdso": "disabled",
  "at_random": "zeroed",
  "cpu_count_presented": 1,
  "backend": { "name": "ptrace", "progress_unit": "branches" }
}
```

### 4.6 What a bundle contains about the guest — and the redaction contract

A bundle is customer data ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §11). Stated explicitly so nobody
attaches one to a public issue by accident:

| Present | Present after `--redact` |
|---|---|
| binary paths and blake3 fingerprints | fingerprints only, paths hashed |
| syscall numbers and argument registers | unchanged |
| guest memory read or written at intercepted calls (file contents, packet payloads) | **removed**, replaced by length + digest |
| environment variables and argv of the guest | **removed** |
| SDK record `details` payloads | **removed**, names kept |
| the tape, the event structure, the timeline | unchanged — reduction still replays |

`bohr replay` on a redacted bundle works and reproduces the same **structure and digest** as long as
the guest's own inputs are reproducible from its filesystem; where payload contents mattered, replay
reports `BOHR-4010` naming the first event whose payload was needed. Redaction that silently
produced a different run would be worse than no redaction.

---

## 5. World configuration

A TOML file (`bohr.toml`) plus flags plus `BOHR_*`, merged in that order of increasing precedence,
validated at startup, written into the bundle verbatim after merging.

### 5.1 Shape

```toml
[run]
seed = 0                        # 0 = derive from the campaign master seed
scheduler = "pct"               # random | pct | fair | adversarial | replay
pct_depth = 3
switch_points = ["syscall", "futex", "budget"]
budget_branches = 10_000
max_ticks = 90_000_000_000_000  # 25 virtual hours; a guest that sleeps longer is hung
timeout_wall_secs = 300         # real-world guard against a wedged supervisor

[cluster]
nodes = 5
argv = ["./target/release/app", "--id", "{node}"]
env = { RUST_LOG = "info" }

# Or, where the nodes are not alike — a service and the thing that drives it — one command line
# each instead of one shared. `argv` and `programs` are alternatives and naming both is refused;
# so is naming fewer programs than there are nodes, which would quietly hand a node its
# neighbour's command line. `{node}` is substituted either way.
[cluster]
nodes = 2
programs = [
  ["./execution-client", "--datadir", "/data"],
  ["./driver"],
]

[network]
topology = "full"               # full | ring | star | file:<path>
latency_mean_ticks = 500_000    # 0.5 ms
latency_jitter_ticks = 200_000
drop_probability = "1/1000"     # rationals, never floats (§1 rule 2)
duplicate_probability = "1/10000"
reorder_probability = "1/100"
corrupt_probability = "1/500"   # a datagram arrives with bytes altered; never a stream
corrupt_bytes = 1               # how many bytes an altered datagram has flipped

[disk]
sync_semantics = "strict"       # strict | lenient — strict loses everything unsynced on crash
stall_probability = "1/500"
stall_mean_ticks = 5_000_000
eio_probability = "1/10000"
enospc_probability = "1/10000"
snapshot_depth = 4              # synced images kept for node_restart_from_snapshot
reorder_unsynced = true         # a crash may keep some unsynced writes and lose others

[faults]
enabled = ["net_partition", "node_kill", "node_freeze", "node_thaw", "node_restart",
           "node_restart_from_snapshot", "disk_tear", "clock_skew"]
budget_per_run = 8              # hard cap on drawn faults in one run
kill_probability = "1/1000"     # odds a drawn node kill fires at a moment it may fire at
enomem_probability = "1/500"    # odds a brk, mremap or anonymous mmap is refused with ENOMEM
fault_points = ["after_ack"]    # SDK fault points a run may arm, by name
fault_point_probability = "1/2" # odds each is armed on a node, drawn once per run
restart_after_ticks = 3_000_000 # 0 leaves a killed node dead, a cut standing, a link bad
arm_after_ticks = 300_000_000   # nothing is drawn before here; 0 arms from the first instruction
draw = ["node_kill", "node_freeze", "node_isolate", "node_degrade", "recover_from_snapshot",
        "clock_step", "clock_drift"]
targeting = "guided"            # uniform | guided — guided prefers dangerous moments

[linux]
io_uring = "absent"             # absent | ring — the kernel the guest is shown

[clock]
skew_max_ticks = 50_000_000
drift_ppm_max = 200

[coverage]
mode = "auto"                   # auto | shm | pt | sdk | off
shm_size = 65536
```

**Altered datagrams.** `corrupt_probability` delivers a datagram with `corrupt_bytes` bytes flipped
at drawn offsets, each by a drawn mask that changes at least one bit, as a `net_corrupt` fault and a
`corrupted` packet fate. Never a stream: a stream's transport checks and retransmits, and what
reaches the program is what was sent. Drawn only where the scenario sets it, and then always at one
width — the decision and every flip — so a run that arms no corruption keeps the tape it had.

**Writes reordered before a crash.** A crash keeps all of what was unsynced, none of it, or — a
quarter of the time — each sector for itself, with a file's grown tail landing with its length or
not at all. That last case can keep a later write and lose an earlier one, and a crash that does
fires `disk_reorder`. `reorder_unsynced = false` takes it away: every crash then keeps all or
nothing, which is the state a store that orders its writes by issuing them in order is written for.

**Wall time has a date.** Every wall clock starts at 2025-01-01 00:00:00 UTC, the same on every run
and every host, so a clock stepped back in a run's first minute moves back rather than standing at
1970, and a program that checks the date it is shown sees a year it can believe. A file's access,
change and modification times are that date, so its age read against the clock is the run's own
duration.

**Memory that runs out.** `enomem_probability` refuses an allocation — `brk`, `mremap`, `mmap` of
anonymous memory — at drawn moments, each a `memory_exhausted` fault counted as adversity: `mmap` and
`mremap` answer `ENOMEM`, `brk` leaves the break where it was, which is how the kernel refuses one.
`brk(0)`, which asks where the break is, is never refused. With it set, allocation calls stop in the
supervisor and cost what any traced call does; without it the filter lets them through untraced, so
a scenario that does not arm it pays nothing. The probe's node is outside it, as it is outside every
fault.

**What a build accepts is smaller than the shape, and says so.** Unknown keys are refused by name
(`BOHR-1002`) rather than skipped, so a typo is never silently a default — and a section this build
does not implement is refused by the same rule. Accepted today: `[cluster]`, `[network]`, `[disk]`,
`[[partition]]`, `[[grey]]`, `[[fault]]`, under `[faults]` the six keys `kill_probability`,
`targeting`, `restart_after_ticks`, `arm_after_ticks`, `budget_per_run` and `draw`, and under
`[run]` the four in §5.6, under `[coverage]` the key `shm_size`, and under `[linux]` the key
`io_uring`. Everything else above —
`[clock]`, `coverage.mode`, `faults.enabled` — is refused where it appears; what those describe is
set by flags and by the defaults in §4.2 and §5.5. Reserved keys are named in refusals rather than
accepted and ignored, for the reason `sync_semantics = "lenient"` is: a configuration that is read
and does nothing is a configuration that lies.

**A campaign that armed adversity and landed none is refused** (`BOHR-5006`), and a single run says
what landed on it. The two come apart silently: `faults.arm_after_ticks` is a moment, and the moment
a run ends is not `run.horizon_ticks` where `run.ends_with_node` names a node — so a scenario can be
checked at load, be entirely well-formed, and fire nothing. Measured: an execution client's campaign
armed a kill three hundred milliseconds in against a run that ended with its caller at forty-four,
and twenty seeds reported twenty clean runs having never once pressed on the client. No check at
load could know; only the runs can say.

**A run in which this world showed the guest something impossible is refused, not reported**
(`BOHR-5005`). Two rules are checked as the run goes, and both are about this tool rather than about
the guest. At every crash, the image the guest comes back to must be one a machine could have left:
each file at a length it held or grew to, holding bytes some write or some sync put there, and
nothing the kernel answers out of itself — no `/proc` entry — on the disk or in its namespace. At
every answer, a descriptor this world issued and has not taken back is never answered `EBADF`.

The reason is attribution rather than correctness. A guest told something untrue about its own state
does whatever it does next because of this tool, and a run like that reported as a finding is a
finding against the wrong project — which is not hypothetical: of four things that looked like an
execution client's defects on the first day of hunting one, three were this tool's, and each was
disproved only by reading the client's source. A refusal names the file or the descriptor and stops
the run, so the next such defect is found by the tool that caused it.

**Disk semantics under `strict` — normative.** Reads always see the live file; durability is
`fsync`'s promise alone, and it is **that file's**. A completed `fsync` makes the named file's
content durable and says nothing about any other file — which is what makes a write-ahead log worth
having, and what makes a store that syncs the wrong descriptor lose data.

**A crash leaves a state the disk was never in.** For each file, everything its own last sync
promised survives. Of the byte ranges written since, one drawn value decides the regime: half of
all crashes keep none of them, a quarter keep all of them, and a quarter decide **each 512-byte
sector for itself** — a sector that landed holds what the guest wrote, and one that did not holds
what was there before. A file reached by a surviving sector past its durable length grows to it and
the gap below reads as zeroes. A crash where some sectors landed and others did not records
`FaultFired { DiskTear }` on the node.

The regime is one draw of a fixed eight bytes per crash, whatever the disk holds. A draw per sector
would make the tape position depend on how many sectors happened to be dirty, and one mutated byte
would then shift every decision after it.

**Not modelled**, and named here rather than left to be found: reordering *between* writes to the
same sector — a sector holds either its newest value or its old one, never an intermediate; and
directory operations as an axis of their own — names and numbering are taken whole at each sync.

Three draws happen on every sync whatever falls out — `EIO` (`DiskError`), a stall (`DiskLatency`),
and the stall's exponential duration — so a drawn stall cannot change what the next draw costs. A
failed sync reports `EIO` once and drops the dirty pages **of that file**, which is the bug class
fsyncgate named: a store that retries and carries on has acknowledged data it no longer has. Each
completed sync joins a ring of `snapshot_depth` whole-disk images, which is what
`node_restart_from_snapshot` reopens. Every data call is recorded as a `DiskOp` event alongside the
`Syscall` row that answered it: the syscall row says what the guest was told, the disk row what the
disk did. `sync_semantics = "lenient"` is reserved and refused until it ships.

**`[linux] io_uring` decides whether the guest has a ring.** `"absent"`, the default, is a kernel
built without one: `io_uring_setup` answers `ENOSYS`, the guest takes the fallback every such
program keeps, and entering or registering a ring ends the run under `BOHR-2004`. `"ring"` opens a
ring — a polled one with its poller asleep — and carries seven operations: `READ`, `WRITE`,
`OPENAT`, `CLOSE`, `STATX`, `FSYNC` and `UNLINKAT`, plus `NOP`. Each is carried out as the plain
call it stands for, by the route that call takes, so a write through a ring is marked on the disk,
lost to a crash and failed by a drawn `EIO` exactly as `pwrite64` is. Entries run in queue order,
links are honoured, and every completion is posted before `io_uring_enter` returns. A kernel with
the ring also answers `statx`, which `"absent"` leaves `ENOSYS`.

**Not carried**, each ending the run under `BOHR-2004` by name: any other operation, an operation
on a descriptor that is not a file (a socket, a counter), read and write flags (`RWF_*`), the setup
flags `IOPOLL`, `R_DISABLED`, `SQE128`, `CQE32`, `NO_MMAP`, `REGISTERED_FD_ONLY` and `NO_SQARRAY`,
registered files and buffers (`io_uring_register` answers `EINVAL`, so a caller takes the
unregistered path), buffer selection, and a submission that would wait. Setup hands its flags back,
honours `CQSIZE` and `CLAMP`, and answers `EINVAL` to a size of nought or past 32,768. `"ring"` together with
`disk.stall_probability` above zero is refused at load (`BOHR-1002`): a sync stalled inside a
submission has no moment to complete.

### 5.2 Probabilities are rationals

`"1/1000"`, never `0.001`. Parsed into `(num, den)` and fed to `draw_bool` (§2.4). This is rule 2 of
§1 applied where it matters most: a probability that goes through a `f64` parse round-trip on two
different libc versions is a cross-machine divergence in the most-executed code path in the system.

**`faults.draw` names what a fire may be, and only what a running node can be given.** A fault is
drawn at a syscall and a node making one is running, so a thaw or a restart drawn there would be
aimed at a state the node is not in — coming back is not a kind of its own but the second half of
the fault that took the node away, armed after `restart_after_ticks`. `recover_from_snapshot` is a
choice about that second half rather than a kind: the fault is still the kill, and what changes is
the disk the node finds when it returns.

Which kind a fire is comes from a draw over the enabled set, so a mutation moves it. **Enabling one
kind consumes exactly what enabling none did**: an index into a set of one is a choice with one
option and costs no tape, which is what keeps every bundle recorded before this replaying to the
digest it recorded.

Two of the kinds are about links rather than about a node's life, and both take the whole node:
`node_isolate` cuts every link between it and the rest of the cluster, `node_degrade` leaves those
links up and makes them bad — slow, lossy, or both, drawn over three rungs. Both recover after
`restart_after_ticks`, and both are raised through the same code a scenario reaches with
`[[partition]]` and `[[grey]]`. A scenario that wants one link of one pair writes it down; these are
the whole-node cases, which is what a search wants and what nothing before this could draw.

Two more are about a node's clock. A drawn `clock_step` moves its wall clock by a drawn amount within
a minute either way; a drawn `clock_drift` sets its rate to a drawn value within ±200 parts per
million. The size is drawn after the kind, from the `ClockSkew` site, so a scenario that does not
draw clocks lays its tape out as it did. Neither recovers: a clock that was moved stays moved.

**`budget_per_run` is a ceiling on fires, not on faults narrated.** Odds say when a fault fires and
never how many: at one part in a thousand per eligible call, a guest making millions of them is
killed hundreds of times, and what such a run reports on is the restart path rather than whatever
the campaign was asking. A spent budget draws the odds exactly as an unspent one does and returns
nought — the draw is a function of the request and never of the run's state (§2.4) — so a ceiling
moves nothing decided before it was reached. Left out, a run has no ceiling; `0` is refused, because
a policy that can never fire is one that should not have been armed.

**`den` may not exceed 65536**, the granularity of the boolean draw (§2.4). A larger denominator is
`BOHR-1003` at configuration load, naming the key and the nearest expressible value. Rounding it
silently would give a fault that is configured but never fires.

### 5.3 Topology files

```toml
# network topology, referenced as topology = "file:net.toml"
[[link]]
a = 0
b = 1
latency_mean_ticks = 20_000_000   # cross-region
drop_probability = "1/100"

[[partition]]                     # a scheduled, not drawn, partition
at_tick = 5_000_000_000
cut = { from = [0, 1], to = [2, 3, 4], directional = true }
heal_at_tick = 9_000_000_000
```

Scheduled partitions exist so a specific scenario can be described by hand and replayed as a
regression test. The classic split-brain-then-heal is written this way in `examples/split-brain/`.

**Delivery semantics — normative.** Every packet makes the same draws whatever the network's state,
in this order: dropped (`NetDeliver`, `drop_probability`), duplicated (`NetDeliver`,
`duplicate_probability`), reordered (`NetReorder`, `reorder_probability`), then the delay
(`NetDelay`), computed as `latency_mean_ticks + draw_exponential(latency_jitter_ticks)`. Only then
is the partition matrix consulted. Drawing first keeps consumption a function of the request stream
alone (§2.4): a partition healing one tick earlier changes packet fates, never the tape alignment
of anything after them. The matrix is consulted once, at send; a partition raised while a packet is
in flight does not retract it.

**Reordering** schedules the packet at `max(now + 1, t_p − 1)`, where `t_p` is the delivery tick of
the earliest packet still in flight on the same directed link; with nothing in flight the fate
degrades to `Delivered`. A packet is never delivered before it is sent, whatever the draws say.

**Topologies.** `full` links every pair; `ring` links `i ↔ (i+1) mod n`; `star` links node 0 to
every other node. A pair with no link is unreachable: a send to it still makes its four draws, and
the packet is then dropped with `PacketFate::Dropped` — topology is not a fault, and no
`FaultFired` is recorded for it.

**Addressing.** Node `n` owns the virtual address `10.66.0.(n+1)`. Binding registers
`(node, port)`; ephemeral ports are handed out from 32768 upward in binding order. A receive queue
holds 64 datagrams, and a packet arriving at a full queue is dropped with `PacketFate::Dropped`.

**Stream semantics — normative.** A `SOCK_STREAM` socket is a byte stream between two nodes, opened
by `connect` against a `listen`ing socket and taken by `accept`. Everything above is about
datagrams and none of it applies here: **a stream makes no draws at all.** Its transport
retransmits until loss, duplication and reordering are gone, so a model that injected any of the
three would hand a guest a conversation the real thing cannot produce. A stream conversation
therefore costs no entropy, and adding one to a scenario cannot shift the tape alignment of
anything else in it.

What the transport cannot hide is that the far end is unreachable, and that is the whole of what
this model does to a stream:

- **A write into a cut link waits.** It is neither refused nor lost: the bytes are held on the
  sending side, the call returns their length, and they land when the cut lifts. The caller learns
  nothing from the write, which is correct — what it learns is that no answer comes back.
- **A `connect` across a cut link answers `EINPROGRESS` and stays unfinished**, and is completed at
  the heal. Refusing outright would report in an instant a verdict the real transport takes minutes
  to reach, and would hide the very stall a partition exists to cause.
- **A stream ends when the process behind it goes.** Closing, shutting down, and a node being killed
  all leave the other end reading nought — end of stream — and a task already parked in that read is
  released by it. This is the only way a guest learns its counterpart is gone, and so the only way a
  client whose server was killed mid-request reaches the code that handles one.
- **Readiness** asks the same question of both kinds: a stream is readable when it holds bytes *or*
  has ended, a listening socket when a connection is waiting to be accepted. Everything is always
  writable, because this model has no send buffer to fill.

`send` is `sendto` with no address, which is what a connected stream uses; it is a stream write and
the null pointer is never read. `SOCK_NONBLOCK` on `socket` and `accept4` is honoured as `FIONBIO`
is.

**What a stream does not model**, stated so a finding is never read as more than it is. Bytes arrive
as they were written, so a read returns everything buffered and never a prefix of it — segmentation
is real and is not injected here, and a guest whose framing assumes a whole message will not be
caught by this model. A write never blocks and never writes short, because there is no send buffer
to fill. A `listen` backlog is read and not honoured: what a full one does is drop the handshake and
leave the caller retransmitting, and there is no retransmission here to stall. And nothing times
out on its own — a connection into a cut link waits for the heal or for the end of the run.

**Grey degradation** is a scheduled window over a link, written like a partition:

```toml
[[grey]]
link = { from = 0, to = 2 }
at_tick = 2_000_000_000
until_tick = 4_000_000_000
drop_probability = "1/5"          # replaces the link's own for the window
latency_factor = 10               # multiplies the link's mean latency
```

While the window is open the link's drop probability and mean latency are overridden as given, and
entry and exit are recorded as `FaultFired { NetDrop | NetDelay, Link }`. A drawn form — grey
windows chosen by the search rather than the scenario — arrives with the explorer.

**Scheduled node faults** follow the same pattern: a fault on the scenario clock, so the classic
kill-and-recover shapes replay as regression tests.

```toml
[[fault]]
kind = "node_kill"                # node_kill | node_freeze | node_thaw | node_restart |
                                  # node_restart_from_snapshot | chain_reorg |
                                  # clock_step | clock_drift
node = 1                          # the node it acts on; a cluster-wide fault names none
at_tick = 3_000_000_000
snapshot_back = 1                 # syncs to step back from newest, for the two that rewind
nodes = [0, 1, 2]                 # chain_reorg only, and optional: absent means every node
step_ticks = -3_000_000_000       # clock_step only: the wall clock moves by this, back if negative
drift_ppm = 150                   # clock_drift only: the new rate, within ±200
```

**A clock step moves wall time and never monotonic time.** `CLOCK_REALTIME` and its coarse, alarm
and TAI forms, `gettimeofday` and `time` read the stepped clock; every other clock id reads the
monotonic one, which keeps rising through a step back. A `clock_drift` changes both readings' rate
from its moment on and leaves earlier readings as they were. A `drift_ppm` outside ±200 is refused at
load, and either key on another kind of fault is refused too.

An absolute deadline is measured on the clock it names: `clock_nanosleep` on a wall clock and a
futex wait carrying `FUTEX_CLOCK_REALTIME` wait for wall time, every other one for monotonic time. A
step does not move a wait already armed; it moves the deadlines set after it.

**Not modelled: a realistic wall date.** Wall time begins at the epoch, as monotonic time does, so a
step back further than the run has lasted holds wall time at the epoch until monotonic time catches
up, rather than going below it.

**`chain_reorg` is the node-level rewind lifted to the cluster.** Every node in the set goes back to
a common older synced image at one moment, the history after that point is **discarded**, and what
the cluster does next is a different history rather than a replay of the same one. The discarding is
the difference from `node_restart_from_snapshot`, which leaves the newer images in place: a rewind
that does not trim has no fork point, because the future it stepped past is still there for the next
rewind to count from.

A depth deeper than a node's own history is clamped rather than refused, and both numbers reach the
timeline. A cluster has as many independent sync histories as it has nodes, so a depth that is valid
on one may be past another's — and ending the run there would make the fault unusable at exactly the
scenarios it exists for.

It is narrated **once, at the cluster**: one `FaultFired { chain_reorg, world }`, one domain event
carrying the depth asked for, the depth applied and the nodes touched, and then the ordinary
lifecycle of each node dying and coming back. A reorg told as N node restarts would be
indistinguishable from N independent rewinds, and telling those apart is what it is for.

**What reduction cannot do with it.** A reduction has two moves — drop an entry, or bring a moment
forward — and "make this number smaller" is a third. `snapshot_back = 4` reduces to *deleted* or to
*earlier*, never to `snapshot_back = 1`. Written here so it is found by reading rather than by a
reduction that stalls.

A freeze pauses the node's tasks without dropping anything — deliveries and wakes accumulate and
land when the thaw lets it run, which is what a GC pause looks like from outside. A kill loses
everything unsynced (the disk semantics above); a restart re-runs the node's program against the
disk its crash left, or against an older synced image when stepping back. Every fired fault is
narrated by `FaultFired` and the `NodeState` transition it causes.

### 5.4 Fault targeting

With `targeting = "guided"` the explorer does not draw a uniformly random moment. It maintains a set
of **candidate moments** and draws among those:

| Candidate | Detected from |
|---|---|
| after a write, before its `fsync` | syscall shape: `write` on an fd with no subsequent `fsync` yet |
| during a `fsync` | inside the emulated call |
| between a quorum ack and the reply to the client | SDK `fault_point`, or a send following ≥ N receives on distinct peers |
| immediately after a leadership-related SDK marker | SDK `fault_point("leader_change")` |
| while a node is in a restart window | node lifecycle state |

Without the SDK the heuristics are syscall-shaped and weaker; with it they are exact.

**A fault point is armed by the run.** `[faults] fault_points` lists the names a guest gives its
`fault_point` calls, and each is armed on each node at `fault_point_probability` (one in two by
default). Which ones are armed is drawn once, before the first node clock: eight bytes, made in
every run whatever the scenario lists, so a scenario that names a fault point moves no other
decision. The supervisor puts the armed names in `BOHR_ARMED_FAULT_POINTS` of every start of the
node, a restart included, and says so on the timeline with `BOHR-2008`. That draw is why the tape
ABI is 2: every tape offset after it moved, and a bundle of ABI 1 is refused rather than misread.
A campaign varies what is armed as it varies the tape; the names are the scenario's to list.

**The order-of-magnitude claim is not supported by measurement, and is withdrawn until it is.**
The specification claimed guided targeting finds ten times what uniform chaos does. Measured on
`kv_fsync` — a depth-2 defect where a crash must land between an acknowledgement and its `fsync` —
uniform wins at every probability tried:

| odds a fault fires | uniform | guided |
|---|---:|---:|
| 1/400 | 13/60 | 6/60 |
| 1/100 | 33/60 | 21/60 |
| 1/40 | 52/60 | 33/60 |
| 1/10 | 60/60 | 56/60 |

The reason is instructive rather than disappointing. The candidate set is a *subset* of the moments
uniform draws from, so at equal odds guided fires strictly fewer faults; and on this target the
dangerous window — anything written and not yet synced — is most of the run, so narrowing to it
removes faults without concentrating them anywhere new. Guided targeting can only pay where the
candidate moments are **rare**, and it must raise its odds to fire as often as uniform does.

Two things follow, and both are open work rather than settled design. The odds mean different
things in the two modes — per syscall under `uniform`, per candidate moment under `guided` — and a
comparison at one number compares two different rates. And a candidate heuristic that admits most
of the run is not a heuristic. Numbers in `benches/baseline.toml` under `[m3.targeting]`; the
claim returns to this section when a measurement supports it.
### 5.5 Coverage channel

`mode = "shm"`: an AFL-compatible 8-bit edge bitmap, 65 536 entries by default and as many as
`[coverage] shm_size` asks for, shared with the guest through **an inherited file descriptor**. The supervisor creates an anonymous memory file, places it
at a fixed descriptor number in the guest, and names that number in `BOHR_COV_FD`; the guest's
runtime maps it `MAP_SHARED` and writes to it directly. The guest must be built with
`-fsanitize-coverage=trace-pc-guard` and linked against `bohr-cov-rt` (a ~200-line C runtime shipped
in `sdk/c/`).

**A descriptor rather than a named segment, because of our own isolation.** A guest runs inside its
own IPC and mount namespaces (§5.6), so a System V identifier or a `/dev/shm` name means nothing on
the other side of them — the `__AFL_SHM_ID` convention cannot reach a guest we have isolated
properly. A descriptor crosses `execve` and every namespace, being neither a name nor an
identifier.

A guest built without the runtime leaves the map empty. That is reported as a run searched without
coverage, never as a run that covered nothing: the two are opposite facts and a search that confused
them would conclude the guest does nothing.

**The map must hold at least as many counters as the guest has edges, and the guest is the only
side that knows how many that is.** Two edges landing on one counter are one edge to everything
downstream: a map most of the way full aliases most of what it is shown, so nearly every run looks
novel, the corpus keeps all of them, and novelty stops choosing. Measured against a real execution
client, thirty-two thousand of the default sixty-five thousand counters were filled by one startup
and four blocks — and a search in that state looks exactly like one that has covered everything
there is.

So the region begins with a 32-byte header both sides write to. The supervisor puts a magic word
and the number of counters there; the guest's runtime puts its edge count there, filled by
`__sanitizer_cov_trace_pc_guard_init` as the compiler hands it each range. `bohr run` and
`bohr explore` print the two numbers together and say so when the map is the smaller. The remedy is
`[coverage] shm_size`, between 1 024 and 67 108 864, and not a longer campaign — a map is copied out
of shared memory once per run and held once per worker, which is why the size is asked for rather
than assumed generously.

**The size travels in the header and never in the guest's environment**, because the environment is
part of what the guest was started with: a size there makes one seed digest differently under two
map sizes, and a bundle recorded by one campaign would not replay under a campaign searching with
another. Coverage is what a search reads, never part of what a run *is*, and §5.1 does not exempt
the explorer. The guest reserves address space for the largest permitted map, reads the header, and
touches only the counters the header says are there.

That closes the direct dependency and not an indirect one, which is stated because it is measured:
against a real execution client, two map sizes produce **the same events in the same order with the
same returns** and still digest differently, because a syscall event records all six argument
registers whatever the call takes — `sched_yield` takes none — and in a large program the leftover
values are pointers that a different memory footprint moves. The remedy is to record the arguments a
call has rather than the registers it was entered with; until then, a bundle is replayed under the
map size it was recorded with, which is what storing the scenario in the bundle already ensures.

The map is read by the supervisor after each run and folded into the corpus feedback. It is
**never** read during a run: reading it mid-run would make the search depend on when we sampled it,
which is a nondeterminism source in the explorer, and the explorer is not exempt from §5.1. Because a
run is deterministic, so is its map — the same seed on the same binary produces the same bytes, and
that equality is a gate rather than an expectation.

---

### 5.6 Judging a guest that says nothing about itself

Every oracle before this one reads either an assertion the guest makes about itself or the status it
exited with. A blockchain client offers neither: it will not adopt an SDK, it does not exit, and
when it is wrong it stays alive and keeps answering. Two conditions are judged from the boundary
instead, each switched on by the section that configures it and silent otherwise.

**A cluster of a service and its driver has no ending of its own, and `ends_with_node` says which
one to use.** The service goes on serving, so a horizon covering whatever the driver does has to be
guessed at generously, and every tick of the guess is paid for. Naming the driver ends the run when
it is done: against an execution client that polls while it waits, that is four times cheaper for
the same work. The horizon stays as the ceiling for a run whose driver never finishes — a node that
is not in the cluster is refused (`BOHR-1002`) rather than waited for.

**A run has to have an ending before it can have a verdict.** A guest that never exits reaches its
tick budget, and a budget is a hang however the guest behaved — so `[run] horizon_ticks` is the
moment a run that was never going to end on its own is *complete*. Reaching it is judged; reaching
`max_ticks` is still a hang, and a guest that stops making progress before the horizon is still a
deadlock. The first wall a busy server hits is neither: ten million events go by long before
twenty-five virtual hours, so raise `--max-events` rather than doubting the verdict.

```toml
[run]
horizon_ticks = 60_000_000       # the run is complete when it gets here
ends_with_node = 1               # and it is complete when this node is, whichever comes first

[liveness]
after_heal_ticks = 20_000_000    # a healed cluster may commit nothing for this long, and no longer
quiet_ticks = 5_000_000          # and no node may make no call at all for this long
progress = "fsync"               # the only definition this build implements

[durability]
ack = "send_while_dirty"         # the only definition this build implements
```

**Liveness after heal** (`BOHR-2030`) asks whether a cluster that could talk to itself again went on
committing nothing. The window opens when the *last* cut lifts — a scenario with two overlapping
partitions has not healed while one remains — and closes `after_heal_ticks` later; every node that
completed no `fsync` in between is named. Progress is a completed sync and nothing else, because a
cluster that has stopped committing still gossips: counting delivered packets would read a dead
cluster as a healthy one for as long as anybody watched.

**A node that stopped** (`BOHR-2030`, `quiet_ticks`) asks whether a node has made no call at all for
a span. **An idle server is not a quiet one**: with nothing to do it still asks the time and waits on
its poller, and an execution client with no work spends a million clock reads on saying so. A guest
parked in a wait nothing will release makes no call, and that is the only thing telling the two
apart — from the run loop both are every task blocked with a timer somewhere ahead, which is why a
horizon completes both.

**It is the one verdict here that needs nothing written per target.** Every other way of noticing a
guest has died reads an assertion it made, a status it exited with, or an answer a caller asked it
for — and a caller is written for one target at a time. A node that went quiet is a finding for
anything. The span belongs to the scenario because what counts as too long belongs to the guest.

**Two silences are not the guest's and are never reported.** A node this world killed or froze makes
no calls because we stopped it, and a node whose program ended makes none because it finished. Both
would otherwise be findings against the scenario's own doing — the first at every fault whose outage
outlasts the span, which is every run of a crash campaign, and the second in every scenario driven
by a caller that completes. Watching a node resumes, timed afresh, when it comes back.

**Acknowledged and lost** (`BOHR-2031`) asks whether a node answered somebody while what it had
written was not durable, and came back without it. The world has no word for an acknowledgement and
reading the guest's protocol is out of scope, so what stands in for one is stated rather than
implied: **an outbound datagram sent while the node's disk is dirty, by a node that has written
something since it last asked a socket for work.** The first half is the moment guided fault
targeting already aims at; the second is what tells an answer apart from a remark.

That second half was measured into existence rather than reasoned into it. Without it, a store that
answers reads over the socket it answers writes on has its replies to reads counted as promises —
its first ones especially, sent while its file already holds the layout it was created with and
nothing has been committed yet. A hundred and fifty runs against such a store produced forty-one
findings and the client, which knew what had actually been promised, contradicted every one.

It still over-reports in one direction only, and the finding needs the data to be missing afterwards
as well, so a node that lost nothing is never accused however much it sent. And it now under-reports
in one: a node that writes in the background and answers later is not marked, so what it had at
stake goes unrecorded. That is the direction to err in — a mark not made costs a finding that might
have been, and a mark made wrongly costs every finding, because nobody reads an oracle that cries on
a third of its runs.

**Both are watched as the run happens rather than read off the timeline afterwards, and that is a
fact about the code.** A stalled `fsync` records its `DiskOp` at the start of the stall and nothing
when it lands, so "this node made something durable" is not a predicate over the event log whenever
`stall_probability > 0`. What the world records is a fact with a code; what the oracle decides is
that the fact is a finding.

**What `BOHR-2031` can and cannot see, because an oracle whose limits are unwritten is an oracle
that will be trusted where it is blind.** A mark is a file *length*, so the loss it detects is a
file that came back shorter than what was answered for. That is faithful for a store whose data
file grows monotonically and is never rewritten in place — an append-structured log — and it is
blind to a store that commits by writing into a region and updating a header, which is what a
database does: such a store legitimately discards a partial transaction on recovery, and its file
does not change length.

Comparing content instead would be worse rather than better. A store recovering correctly rewrites
its header and drops what it never promised, so a content comparison would report a difference on
nearly every crash, and an oracle that fires on nearly every seed is a machine for false findings —
the same argument that governs projections ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.20).

**The property is not expressible over the bytes of a file at all.** "An acknowledged write
survived" exists only through the store's own read path after the store's own recovery. A hunt
against a real store therefore places the verdict in a client, which is where every
crash-consistency tool for applications places it, and the distinction that keeps such a client
honest is that an unanswered question is not an answer: a request that went to a dead node is
indeterminate and is never used to accuse.

**What the world contributes to such a hunt is the exposure, not the verdict.** At every crash, a
scenario watching durability records `Domain { kind = 3 }` on the node that died, carrying a `u32`
count of files answered for and not yet durable and a `u64` of their marked bytes, both
little-endian. It is a measurement and never a finding: a thousand clean runs whose crashes all
fell on a node with nothing outstanding are a thousand runs that asked nothing, and no other part
of a run tells them apart from a thousand that did.

| Kind | Meaning | Payload |
|---|---|---|
| `1` | a node answered somebody with unsynced writes behind it | `u64` unsynced bytes |
| `2` | history was rewound and let run again | `u32` asked, `u32` applied, `u32` nodes |
| `3` | a crash landed, and this is what was at risk | `u32` files, `u64` marked bytes |

**Identity.** An oracle is chosen by configuration and never by a flag, because a flag would let a
replay judge with a different window than the bundle recorded — a run that is self-consistent and
wrong. The panel's name is derived from its members, so a scenario watching for both is recorded as
`properties+crash+durability+liveness` and a scenario watching for neither is recorded exactly as
`properties+crash`, which is what keeps every bundle written before this milestone replaying.

---

### 5.7 Starting from a prepared state

A node that syncs for days cannot sync again for each of a thousand runs, so the filesystem a run
begins from is an input like any other.

```toml
[state]
dir = "examples/ton-validator/state"   # read once, before anything runs
digest = "b3:8f2a…"                    # optional pin; refused at load if the directory disagrees
cwd = "/var/lib/node"                  # what getcwd answers
```

**A directory on the host, recorded in the bundle by digest — never bundle content.** A state of any
useful size inside a file maintainers are meant to attach to an issue is not a format. What the
bundle carries is `state_digest` in its manifest and a `STATE_MANIFEST` section listing paths,
lengths and content digests, so a bundle says what it needs without carrying it; a replay whose
configuration names a different state is refused with `BOHR-4004`, the same way a changed binary is.

The digest is over `(path, length, content digest)` in sorted order. Host inode numbers,
modification times and permissions are not in it, because the world does not model any of them and a
digest including them would differ between two machines holding the same state.

Applied before the first instruction, so a node's durability baseline is the prepared state: a crash
restores to *where the run began* rather than to an empty disk. Refused at load, naming the path: a
symbolic link (this world's `readlinkat` always answers `ENOENT`, so a link in an image would be a
lie), anything that is not a regular file, and a file larger than the world will hold.

**What bounds this is memory, not disk.** Every file lives in supervisor memory, and a node's whole
disk is cloned into the snapshot ring at each `fsync`, once per node, in every worker a campaign
runs. And a store that reads through a file-backed `mmap` cannot be given a prepared state at all,
at any size: `mmap` is admitted by the filter and never reaches the supervisor, so the guest reads
the host's page cache rather than this world's file. That is a fact about the interception model
rather than about effort.

---

### 5.8 Comparing implementations against each other

A second kind of verdict, and the only one that needs no domain knowledge and no annotation at all:
run implementations of one specification against the same tape and check they produce the same
thing. A disagreement is a bug in one of them by construction, and nobody has to say which behaviour
was correct to report that they differed.

```toml
[differential]
projections = [{ label = "state-root", source = "file:/out/state-root" }]

[[differential.implementation]]
name = "reference"
argv = ["./reference", "--node", "{node}"]

[[differential.implementation]]
name = "second"
argv = ["./second", "--node", "{node}"]
```

**An implementation replaces one node, and `node` says which.** A real client does nothing until
something drives it, so the thing under comparison shares a `[cluster]` with the caller that makes
it work — and the caller is part of the conditions rather than part of what is compared. `node`
defaults to zero, must name a node the cluster has, and must be the same for every implementation:
a client compared against its own caller is not a comparison of two implementations of one thing.
An implementation that names no `argv` leaves that node running whatever the scenario gave it.

**The guest does nothing.** A projection is an extraction rule, not a channel a program opts into,
which is the whole reason this works against a client that will never adopt an SDK. `file:<path>`
reads a file off the node's own filesystem after the run; `stdout` reads what it wrote out. A value
past a megabyte is marked truncated and is **not** compared: two cut-off values that match agree
about nothing.

**A source says which node holds the value**, as `file:1:/state-root` or `stdout:1`, and node zero
where it does not. The node under comparison is not usually the one that has the answer: a client is
compared by what it answered, and it is the caller that wrote that down. Reading a node the scenario
does not run is refused at load.

**What may be compared is not a matter of taste.** The implementations do not get equal conditions —
one tape gives them one source of adversity, not one schedule — so the value has to be one the
specification requires to be equal under any legal schedule. See
[`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.20, which also explains why drawn faults are refused
alongside a comparison and why a comparison that did not happen exits non-zero.

| Outcome | Exit | Meaning |
|---|---:|---|
| every implementation agreed | 0 | on every projection |
| any implementation went wrong on its own | 1 | reported as that implementation's own outcome, and named — an implementation that crashed produced nothing to compare |
| they differed | 1 | `implementations_disagreed`, naming the projections |
| nothing was compared | 3 | `BOHR-5004`; never 0, or a campaign reports clean runs having checked nothing |

### 5.9 Asking a node what it did: probes and the chain oracles

A blockchain client that is wrong does not crash and does not say so: it finalises a block and later
reports another at the same height, signs two blocks for one slot, or keeps running and stops
making progress. What it does offer is its own API, and the judgement is made from that. A *probe*
is not a hook into the client but a guest the world starts as one more node, after the last one the
scenario numbers, which calls the nodes' JSON-RPC over the virtual network on a schedule drawn
from the tape and writes what came back as SDK records (`bohr_probe` events, §6.3). The world reads
those records from the probe's node and no other, compares the answers, and raises a code when a
property breaks; an oracle reads the code. See [`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.17.

```toml
[chain]
oracles = ["finality", "agreement", "liveness", "double-sign"]
every_ticks = 100_000_000        # how often each node is asked; each round moves by up to a quarter
stall_ticks = 1_000_000_000      # liveness: how long a finalised height may stay once the cluster is whole
votes_rpc = "chain_votes"        # double-sign: the method listing signed votes as signer, height, hash
nodes = [0, 1, 2]                # optional; every node by default
port = 8545                      # optional; Ethereum's by default

[[probe]]                        # the same mechanism, written by hand
name = "head"                    # optional; what findings call it
node = 0                         # or nodes = [...]; every node by default
rpc = "eth_blockNumber"
params = []                      # an array, or a table for a method that takes named params
every_ticks = 50_000_000
value = "."                      # a field of the result, dotted for nesting; "." is the result itself
key = []                         # fields naming what a value belongs to; none for a single value
property = "monotonic"           # never-reverts | monotonic | agrees | advances | unique
# within_ticks = 1_000_000_000   # advances only, and required there: the span a value may stay
```

Each chain oracle is a probe written out in advance, so there is one judging path:

| Oracle | Asks | Key → value | Property | Code |
|---|---|---|---|---|
| `finality` | `eth_getBlockByNumber("finalized", false)` | `number` → `hash` | `never-reverts`: on one node, a value at a key never changes once seen | `BOHR-2040` |
| `agreement` | the same | `number` → `hash` | `agrees`: two nodes never answer differently for one key | `BOHR-2041` |
| `liveness` | the same | → `number` | `advances`: the value changes within `stall_ticks`, timed from when the cluster was last whole | `BOHR-2042` |
| `double-sign` | `votes_rpc` | `signer`, `height` → `hash` | `unique`: one key carries one value, on any node at any time | `BOHR-2043` |
| — | any `[[probe]]` | as written | `monotonic`: on one node, a number at a key never goes down | `BOHR-2044` |

**The probe's node is outside the fault model.** It is never killed, never cut off by a partition or
a drawn isolation, never counted as silent by `[liveness]`, and it reaches every node whatever the
topology: an instrument the run could break would report its own breakage as the cluster's. A node
isolated from its peers still answers the probe, which is how a minority's fork is seen while it
happens. `advances` judges only while nothing is down and nothing is cut, from the latest of the
value's last change, the node's first answer since it last came up and the moment the cluster last
became whole: a node that stops answering does not advance, and a node still booting is not judged.
A value the world itself took back — a `chain_reorg`, a restart from an older snapshot — is
forgotten rather than held against the node. An answer missing a key field is no observation.

**The probe is found, not written.** It is a static executable — a world holds no dynamic loader —
named by `BOHR_PROBE`, or beside `bohr`, or where `make probe` builds it,
`target/x86_64-unknown-linux-gnu/release/bohr-probe`; a dynamic one is passed over, and a scenario
with probes and no probe is refused (`BOHR-1002`). Inside the run it is always `/bohr/probe`, so a
bundle replays wherever the file is kept. Requests to one node go as one JSON-RPC batch a round; the
reply to a round is read in the next, without waiting, so a node that is slow, down or frozen costs
only its own answers. A probe holds at most 65,536 keys and says so once (`BOHR-2045`) rather than
growing.

What the probes do not do:

- **Read a client's protocol from its memory, its disk or its wire.** What a node did is asked of
  its API. A client that exposes no signed votes cannot be judged for double-signing here; reading
  consensus messages off the network is a probe configuration per client, and none is built in.
- **Compare what the probe did not see.** Each node is asked for its finalised *tip*, so two nodes
  are compared at the heights both happened to be at when asked, and a revert between two askings
  that is undone before the next is not seen. The schedule is drawn, so another seed asks at
  other moments.
- **Judge a value it could not parse.** Results are JSON with integer numbers; a reply with a float
  or a body over 1 MiB is an unanswered request, not a finding.

---

## 6. SDK

Optional. Without it the tool finds crashes, hangs and corruption; with it, it understands intent.

### 6.1 Surface, identical in all five languages

```
always(condition, name)                 // must hold on every evaluation
always_or_unreachable(condition, name)  // holds whenever reached
sometimes(condition, name)              // must be true at least once in a campaign
reachable(name)                         // must be reached at least once in a campaign
unreachable(name)                       // must never be reached

random_bytes(n) -> bytes                // drawn from the tape, not from the system
random_choice(seq) -> item
fault_point(name)                       // a place the explorer may inject a fault
setup_complete(details)                 // "initialisation is over, hunting can begin"
send_event(name, details)               // a structured observation for the report
```

Rust:

```rust
bohr::always!(index.is_monotonic(), "log index is monotonic");
bohr::sometimes!(leader_changed && disk_full, "leader change under a full disk");
let bytes = bohr::random_bytes(16);
bohr::fault_point("before_commit_ack");
```

Python:

```python
# The module is `bohr_sdk`: the top-level name `bohr` belongs to an unrelated
# PyPI package. See ARCHITECTURE.md §0.1.
import bohr_sdk as bohr

bohr.always(index.is_monotonic(), "log index is monotonic")
with bohr.fault_point("before_commit_ack"):
    ack(client)
```

C#:

```csharp
using Bohr;
Assert.Always(index.IsMonotonic, "log index is monotonic");
Assert.Sometimes(leaderChanged && diskFull, "leader change under a full disk");
```

Go — plain functions rather than macros, so where an assertion is written is found from the call
stack:

```go
import bohr "github.com/bohr-dev/bohr-sdk-go"

bohr.Always(index.IsMonotonic(), "log index is monotonic")
bohr.Sometimes(leaderChanged && diskFull, "leader change under a full disk")
if bohr.FaultPoint("before_commit_ack") {
    return errCommitLost
}
```

### 6.2 Activation and cost

| Environment | Behaviour |
|---|---|
| neither variable set | every call returns immediately; one branch on a cached boolean |
| `BOHR_SDK_OUTPUT=<path or fd>` | records are written there |
| `ANTITHESIS_SDK_LOCAL_OUTPUT=<path>` | same, honoured for compatibility (§5.14) |
| both set | `BOHR_SDK_OUTPUT` wins, and a `Diagnostic` event notes it |

`random_bytes` behaves differently: with no output channel it falls back to the platform RNG, and
the call is then a genuine nondeterminism source. The supervisor detects this — it sees a
`getrandom` that was not routed through the SDK — and raises `BOHR-2020`, because an SDK randomness
call that silently used the system RNG would produce runs that do not replay.

### 6.3 Record schema

One JSON object per line, UTF-8, newline-terminated. The **compatible subset** matches the
Antithesis local-output format; everything of ours lives under the `bohr` key so their tooling
ignores it and ours does not lose it.

```json
{"antithesis_assert": {
   "hit": true,
   "must_hit": true,
   "assert_type": "always",
   "display_type": "Always",
   "message": "log index is monotonic",
   "condition": false,
   "id": "log index is monotonic",
   "location": {"class": "raft::log", "function": "append",
                "file": "src/log.rs", "begin_line": 220, "begin_column": 9},
   "details": {"index": 41, "prev": 42}},
 "bohr": {"schema": 1, "seq": 8814, "node": 3}}
```

```json
{"antithesis_setup": {"status": "complete", "details": {"nodes": 5}},
 "bohr": {"schema": 1, "seq": 1, "node": 0}}
```

```json
{"bohr_fault_point": {"name": "before_commit_ack", "armed": true},
 "bohr": {"schema": 1, "seq": 4021, "node": 3}}
```

Rules:

- **`bohr.seq` is per-node and monotonic.** It is how the supervisor detects a record it never
  saw — meaning the guest wrote to the channel through a path we did not intercept — which is
  `BOHR-2021`.
- A malformed line increments a counter, is sampled into the log, and is dropped. It does not kill
  the run: the guest is the thing we expect to be broken
  ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §8 rule 6).
- Unknown top-level keys are preserved into `SDK_RECORDS` verbatim, so a record written by a newer
  SDK survives an older tool.
- The JSON Schema lives at `crates/bohr-sdk-proto/schema/records.schema.json`, is committed, and is
  generated from the Rust types — a CI job regenerates it and fails on a diff, so the schema and the
  code cannot drift.

### 6.4 Antithesis compatibility matrix

| Antithesis | Bohr | Note |
|---|---|---|
| `Always`, `AlwaysOrUnreachable` | same semantics | — |
| `Sometimes`, `Reachable`, `Unreachable` | same semantics, campaign-scoped | — |
| `AlwaysGreaterThan` and the numeric guards | parsed, evaluated | we accept them; we do not emit them |
| `SetupComplete` | same | marks the end of initialisation for the explorer |
| `SendEvent` | same | becomes an `Event::SdkRecord` on the timeline |
| `GetRandom` | `random_bytes` | ours draws from the tape |
| Catalog / registration pass | **not implemented** | we discover assertions at runtime; the consequence is that an assertion never reached is invisible until it is reached at least once in some run. Documented, not hidden. |

A fixture directory `testdata/sdk/antithesis/` holds captured records from their SDKs, and a
conformance test parses all of them. When their format moves, that test goes red, which is the whole
point (`ARCHITECTURE.md` §9 S5).

---

## 7. Command-line interface

One binary, subcommands. Every command accepts `--config`, `--log-format`, `--out`.

### 7.1 `bohr run` — one deterministic execution

```
bohr run [OPTIONS] -- <program> [args...]

  --seed <u64>            seed for this run [default: 0]
  --tape <file>           use this tape instead of deriving from a seed
  --repeat <n>            run n times and assert all trace digests are identical
  --save <file.bohr>      write a bundle even for a clean run
  --facts <file.json>     write what the run came to as one line of JSON
  --scheduler <policy>    interleaving policy: pct, random or fair [default: pct]
  --strict-determinism    exit 4 if the run could not be fully guaranteed
```

A policy the binary does not implement is refused by name, listing the ones it has. Accepting it
would report a run under a policy that never executed.

`--repeat` is goal T1 as a command: `bohr run --repeat 1000 -- ./demo` is that goal executed, and its
failure output is a diff of the first two diverging events rather than "digests differ".

Standard output carries what the guest itself wrote and nothing else; everything the tool says about
a run goes to standard error. `bohr run -- ./cat /dev/urandom | head -c 100` therefore pipes the
guest's bytes, and one seed puts the same bytes on the pipe every time. A repetition beyond the
first is a check rather than an execution the user asked to see, so only one run of a seed delivers
its output; what every repetition wrote is compared through the trace digest.

**What a run says is in three levels: the verdict, then the evidence, then the log.** The summary
on standard error leads with one line — `ok` and what ran, or `FINDING`, the verdict in words and
the code to search it by (`BOHR-2040`, or the outcome's kind) — then whatever else the outcome
names, the commands that show more, and last the counters a maintainer of bohr reads:
`N runs · events · syscalls · stops · draws · digest`. No outcome is printed in a program's debug
form; every view words it through one table (`bohr-report` `narrate`), so a summary, a story and a
page never say one finding two ways.

`--facts` is the run's outcome for a program to read, not the bundle and not the text on standard
error: `outcome` (the outcome's kind, `Clean`, `GuestCrashed`, `ProbeViolated`, …), `detail`, `codes`
(every `BOHR-NNNN` the outcome names), `digest`, `events`, `syscalls`, `stops`, `virtual_ns`,
`adversity`, `probe_asked` and `probe_answered` (counted from each request's first answer, since a
node asked before it serves its API is starting rather than failing), `log_bytes`, `peak_rss_kib` (the supervisor's own peak), `cpu_user_ms`,
`cpu_system_ms`, `guest_cpu_ms`, `stop`, `degraded`. It is written for a completed run only; a
run that did not complete leaves no file, and that absence is the answer.

### 7.2 `bohr explore` — the search

```
bohr explore [OPTIONS] [--guest <name>] [-- <program> [args...]]

  --runs <n>              budget in runs; the reproducible one
  --time <duration>       budget in wall-clock time: 90s, 30m, 1h, 2d, or bare seconds
  -j, --jobs <n>          parallel workers
  --corpus <dir>          corpus carried in and written back out
  --resume <dir>          write the campaign down here, and carry on with it if it already is
  --findings <dir>        where bundles are written [default: ./findings]
  --summary <file>        a machine-readable account of the campaign
  --master-seed <u64>     everything is derived from this; reproduces a whole campaign
  --guest <name>          search a guest built into the binary instead of a program
```

**A campaign is a function of its master seed and its run budget.** `bohr explore --runs N
--master-seed S` performs the same runs and reaches the same findings whatever `-j` says, and the
`--summary` file is what says so: it carries the corpus size, the coverage digest and the findings,
and nothing that varies with the machine.

That holds because the campaign is planned a **generation** at a time from the corpus alone, and a
generation's results are folded **in the order they were planned** rather than the order they came
back. Worker count and worker speed cannot reach a decision because they cannot reach the work
list. A generation is a fixed size for the same reason: sizing it by `-j` would make the shape of
the search a property of the machine.

**`--time` cannot be reproducible and does not pretend to be.** Eight workers do more runs in
thirty seconds than one does, so a time budget describes an effort rather than a campaign. It is
kept because people think in "hunt overnight", and what it writes into the summary is the run count
it reached — repeat that campaign exactly with `--runs`.

**`--minimize` is not here yet.** Minimising a finding needs `bohr reduce`, which arrives with the
reduction milestone; the flag is absent rather than accepted and ignored.

A stored corpus holds **tapes only**. What each covered is learned by running it, which costs one
generation when a campaign starts and keeps an entry to its few kilobytes rather than the
sixty-four a coverage map would add.

**`--resume` is the whole campaign, not its tapes.** The directory holds `campaign.bin` — the
corpus with its visit counts and evictions, the coverage seen, findings, credit, properties and
counters in a versioned format, and the results of a generation a run budget cut short — and
`campaign.toml`, which a person reads. It is written after every whole generation and at the end,
each file to a part file and renamed, so a campaign stopped at any moment resumes from its last
generation. A generation is folded only when it is whole, so a campaign stopped at `--runs 50` and
resumed to `--runs 100` folds the same generations as one run of `--runs 100`, and its summary is
byte for byte the same. A directory holding a campaign of another master seed, program, binary or
scenario is refused (`BOHR-1002`) rather than continued.

### 7.3 `bohr replay` — reproduce

```
bohr replay [OPTIONS] <bundle.bohr>

  --binaries <dir>        where to find the recorded binaries if paths moved
  --verify-only           check digest and exit; do not print a timeline
  --gdb                   stop before the first instruction and print a gdb attach line
  --break-at <event-id>   stop the world at an event and hand over to the debugger
```

Exit code 0 means every named bundle replayed to the digest it recorded. A run that diverged, or a
draw journal that no longer regenerates, is exit 3 — the bundle and the tool disagree, which is a
defect in one of them rather than a finding about a guest. A bundle that is refused before it runs
— a foreign world model, another draw ABI, a binary whose bytes have changed — is exit 2. Several
bundles may be named at once; each is reported and the worst outcome is the exit code, because a
glob is checked to learn whether they all still replay.

**A hand-over is inspection, not resumption, and the difference is not negotiable.** The guest is
released from the supervisor and left stopped, so a debugger can attach and read it: backtraces,
registers, memory, threads. It **cannot be continued.** The filter it was started under traces by
default, and a debugger does not tell the kernel it is listening for those traps, so a syscall
under it does not execute — it returns as though the call did not exist. Reading the clock faults,
and so does asking the processor to identify itself: both were set to trap and stay that way.
The world it was running in — its files, its sockets, its peers — was the supervisor's own
bookkeeping and is gone, so a debugger sees a process with almost no operating system state. And
determinism ends at the hand-over: nothing done from that point belongs to the run that was
reproduced, which is also why a hand-over verifies nothing and `--verify-only` is refused with it.

`--gdb` is `--break-at 0` and shares one code path. A break lands at or after the event named,
because the run stops between two guest stops and one stop can record several events; where the two
differ, both numbers are printed. Every refusal happens before replaying: naming both flags, naming
several bundles, an event past the count the bundle recorded, a bundle whose guest was built into
the tool (`BOHR-3006`), and a host whose `kernel.yama.ptrace_scope` is not `0`, where a debugger
started by hand is not an ancestor of the guest and will not be permitted to attach (`BOHR-3001`).
Each break costs a full replay: there are no snapshots to rewind to.

### 7.4 `bohr reduce` — minimise

```
bohr reduce [OPTIONS] <bundle.bohr> -o <minimal.bohr>

  --budget <n>            maximum verification replays [default: 2000]
  --keep <events|tape|both>  what to minimise [default: both]
  --binaries <dir>        where to find the recorded objects if their paths moved
```

Prints a live "events: 40551 → 18, tape: 8801 → 41 draws" line, because the number is what a reader
of a finding is being shown. Progress goes to standard error and the final line to standard output,
so a pipeline reads the result without reading the working.

**`--keep` names what is minimised, never what is touched.** Not delivering a message is written by
changing the two bytes the delivery was decided from — there is no stored list of deliveries to
strike a line out of — so `--keep events` edits the tape as well.

**The budget is a hard stop and never a target.** What is reported is always a scenario that was
*proven* to still reproduce: a reduction that runs out mid-probe answers with the last thing it
verified, never with the thing it was trying. Two replays are reserved off the top, one to
establish what is being reduced and one to confirm the answer, so a budget below three is refused.

**What it minimises, in order — the order that removes most per replay.**

1. *The configuration.* Entries the world was told to act on — a probe, a partition, a grey
   window, a scheduled fault — are removed, and what survives has its moment brought forward. It
   goes first because the configuration is read before the tape: an entry removed changes what
   every later draw decides, and a fault moved before it would be moved again.
2. *The faults the run drew.* Every odds draw journals whether it fired, so the faults that landed
   are read off the journal: a kill, a dropped packet, an I/O error. Those the finding does not need
   are removed by delta debugging. Then each node fault that is left, from the last back, is fired
   at an earlier chance of the same kind — at the very first chance after the fault before it, then
   at an eighth, a quarter and half of the way. A kill decides how long everything after it takes,
   so one kill fired sooner removes more of a run than any other move.
3. *Deliveries*, then *the remaining decisions on the tape*, in spans halving from the whole set
   to one decision; these improve the number and rarely change the answer.

What the budget holds past the two reserved replays is divided 15 / 20 / 40 per cent among the
first three, and the tape pass runs on what is left; `--keep tape` runs the tape pass alone and
`--keep events` stops before it. **No candidate is replayed twice:** one already answered is counted
as refused without a replay, since a candidate that was kept is the scenario now held and is never
proposed again.

**The same finding, not a neighbouring one.** A candidate counts only with the original's class and
exactly its signature. A smaller run that also crashes a second node is a different finding, and is
refused however small it is. The signature is `bohr findings`' grouping for a crash and an
assertion; for a hang it is the reason the run stopped making progress, where `bohr findings` reads
the words of the verdict, so two hangs one keeps together the other may file apart.

What reduction does not do: share the work of two candidates with a common beginning. That needs
a backend that can hold a guest and go on from it twice (`DeterminismBackend::checkpoint`), and the
ptrace backend cannot; every candidate runs from the first instruction. A fault moved earlier
leaves the faults after it on bytes that now belong to other decisions — the signature check
refuses the candidates that broke, so a finding that needs three kills in order shrinks less than
one that needs one. And an earlier chance is any node's: the journal does not say whose call drew
it, so a kill moved there may kill another node, and the signature check refuses that too; under
`guided` targeting most chances have odds of zero and the probe there is spent.

**A bundle that records nothing going wrong is refused** with `BOHR-5003` rather than reduced to
nothing: reduction preserves what went wrong, and there is no such thing to preserve.

Exit code 0 where a bundle was written, whether or not it shrank; 2 where the input cannot be read
or is refused before anything runs; 3 where the input did not reproduce its own finding under this
build, which is the same reading `bohr replay` gives that disagreement.

### 7.5 `bohr doctor` — check before blaming

```
bohr doctor [--target <program>] [--syscalls]
```

Checks and reports, each as pass / warn / fail with the exact remedy:
kernel version and required features · `kernel.yama.ptrace_scope` · unprivileged user namespaces ·
`perf_event_paranoid` and branch-counter availability · `PR_SET_TSC` and `ARCH_SET_CPUID` support ·
seccomp filter installation · with `--target`, the guest's ELF: dynamic loader, `RDRAND` opcode
scan, `io_uring` usage, the syscall set it references versus our support table.

`--syscalls` prints the support table instead of checking anything, which is what a target's own
syscall set is compared against.

Exit code 0 where every check passed or only warned — a warning is something lost rather than
something broken — and 4 where one failed, the same code a run uses for "determinism could not be
guaranteed", because that is what it means. A named target that cannot be read is 2: a fact about
the file rather than a verdict about the host.

`doctor` never changes a host setting. It prints the command that would.

### 7.6 Target packs and `bohr hunt` — one command

```
bohr hunt <pack> [--time <duration> | --runs <n> | --seeds <a..b>] [-j <n>] [-o <dir>]
                 [--no-faults] [--resume] [--binary <path>] [--master-seed <u64>]
```

A **target pack** is `targets/<name>/target.toml` in this repository, or in the packs a release
installed: a bare name is looked for under `targets/` here first, then under `$BOHR_HOME/packs` —
`~/.bohr/packs` where unset — which is where `bohrup` puts the packs of the release it installed.

```toml
[source]
repository = "https://github.com/paradigmxyz/reth"
commit = "3641f10830f44f71c2469fdf7e818ccd4a4b0e76"

[build]
command = ["targets/reth/build.sh"]       # run from the repository's root
binary = "targets/reth/build/reth"         # where the build leaves it
blake3 = "…"                               # the binary's digest

[run]
scenario = "targets/reth/scenario.toml"

[judge]
oracle = "crash"                           # crash | durability | liveness | property | chain

[bench]                                    # optional: a workload timed against native
argv = ["…"]
```

Unknown keys are refused (`BOHR-1002`), and so is an oracle the scenario does not arm: a
`durability` pack whose scenario has no `[durability]` would call every run clean for want of
looking. **The binary is pinned by digest, and a binary that differs is refused before anything
runs**, with both digests named — never rebuilt over, because that would hide which file is wrong.
A missing binary is built by the pack's command; the builds write this machine's paths out of the
binary (`--remap-path-prefix`) and name their compiler, so the digest names the program on a host
with that compiler and the same C library: a static build links the host's `libc.a`, and the C in
a client is compiled by the host's compiler. A host with another C library builds another file and
is told so.

`bohr hunt` resolves the pack, checks the binary and runs the pack's scenario as `bohr explore`
would, writing `findings/`, `summary.json` and `campaign/` (the state `--resume` carries on from)
under `-o`. `--seeds a..b` runs those seeds once each instead of searching. `--no-faults` runs the
scenario with every drawn and scheduled fault taken out, which is how an oracle is checked against
runs nothing was done to. `--binary <path>` replaces the pack's binary in the scenario's
`argv`, so the file whose digest is checked is the file that runs. A hunt ends by grouping what it
found (§7.7).

The packs in this repository: `redb`, `reth`, `op-reth`. `op-reth` is built from `rust/op-reth` in
`ethereum-optimism/optimism`; the `ethereum-optimism/op-reth` repository is archived.

### 7.7 `bohr findings` — grouped by cause

```
bohr findings <dir> [--json]
```

Groups the bundles in a directory by signature — the programs the run executed and the
violation's class and what tells it from others of its class: the nodes and exit statuses of a
crash, the promises a property run broke, the files a durability failure lost, the nodes a liveness
failure silenced. A hang is named by its words with the numbers taken out. Groups are printed
largest first, each led by its smallest bundle, which is the one to reduce and to read. Two defects
of one program that leave one signature are one group; that is what a reduction already treats as
the same finding.

**`bohr report <bundle> --disclosure [-o report.md]`** writes a disclosure draft in Markdown: what
a maintainer receives. In order — the target (each program by name and blake3, and the source
repository and commit where a pack under `targets/` of the current directory names that digest),
what went wrong and where,
the steps before it, the reproduction command with the trace digest it reaches, what was run with
the configuration verbatim, and what the run cannot claim. A bundle that records nothing going wrong
is refused with `BOHR-5003`. Nothing is sent: the draft is for a person to read, edit and send.

**`bohr report <bundle|dir> --sarif [-o finding.sarif]`** writes SARIF 2.1.0, one result per group
of the directory's findings, grouped as above and each led by its smallest bundle. The rule is the
violation's code; the location is the source line of the guest's own promise where its records name
one, and the bundle otherwise, because code scanning refuses a result without a location. A crash
carries no line, and none is invented.

### 7.8 `bohr init` — a first scenario

```
bohr init [--nodes <n>] [-o <file>] -- <program> [args...]
```

Surveys the program and writes a scenario: the cluster shape, a minute's horizon, the faults its own
calls can meet — the disk where it syncs, the network where it talks across nodes, crashes where it
keeps state — and a comment per wall the survey found. A dynamically linked program gets the
libraries its loader resolves copied into a prepared state beside the scenario, pinned by digest.
The probabilities are a first guess and say so; `init` does not write over an existing file.

### 7.9 `bohr check` and `bohr sweep` — subjects and their checks

```
bohr check <subject>... | --all  [--only <checks>] [--quick] [--plan] [-j N] [-o <dir>]
bohr sweep <scenario> --seeds <a..b> [--expect <code>] [--at-least N] [--no-faults] [-j N] [-- <program>]
```

A *subject* is not a test, but a declaration: a pack's `[check]` table in its `target.toml`, or a
seeded defect's `bug.toml`. It names checks from one fixed set and the bounds they are held to, and
`bohr check` carries them out by starting `bohr run --facts` as many times as each needs, `-j` at
once. A subject named bare is looked up as `targets/<name>/`, then `testdata/bugs/<name>/`, then
`examples/<name>/`.

| Check | Holds when | Parameters |
|---|---|---|
| `works` | with every fault taken out, the program ran: no finding, enough events and virtual time, and a probe answered at least `probe_answers` of what it asked from each request's first answer on | `min_events`, `min_virtual_s`, `probe_answers`, `seed` |
| `repeats` | `times` runs of one seed leave one digest | `times`, `seed` |
| `replays` | a saved run of `seed` replays to its digest | `seed` |
| `clean` | with every fault taken out, no seed finds anything | `seeds` |
| `adversity` | at least `at_least` of the seeds met a fault | `seeds`, `at_least` |
| `finds` | at least `at_least` seeds found `code`: an outcome kind or a diagnostic code | `seeds`, `code`, `at_least` |
| `never_finds` | no seed finds anything | `seeds` |
| `walls` | a survey without faults meets no wall outside `allow` | `allow`, `seed` |
| `costs` | the scenario costs at most `max_ratio` of `against` by `events` or by `wall` | `against`, `metric`, `max_ratio`, `seed` |
| `performs` | a golden workload costs at most its bounds per operation: time, slowdown against native, stops, the supervisor's peak memory | `max_us_per_op`, `max_slowdown`, `max_stops_per_op`, `max_rss_mib`, `best_of` |
| `workers_agree` | a campaign of `runs` writes one summary at `-j 1` and at `-j jobs` | `runs`, `jobs` |
| `refuses_changed` | a copy of the pack's binary one byte longer is refused by its digest | — |

Every check also takes `scenario`, found beside the file that names it; without it, a pack's checks
run the pack's scenario and a defect's run `scenario.toml` beside its `bug.toml`. The program is the
pack's verified binary, or the first word of the scenario's argv. A `bug.toml` holds `defect` (one
line saying what is wrong), `[check]`, and for a program with several defects one `[[defect]]` per
defect with a `name` and its own `check` table. Fractions and ratios are read from their text in
thousandths, never computed in floating point. A check or a parameter nobody reads is refused with
`BOHR-1002`; a misspelt bound that was skipped would hold every time.

The golden guests are a subject of their own, `testdata/golden/golden.toml`, named `golden`. Its
`[[workload]]` entries carry `name`, `workload` (the golden program's mode, the name by default),
`count`, `nodes`, `module` (the part of bohr it loads, in a line), `scenario` (tables added to the
scenario it generates, such as a failing disk) and `check`. The scenario is generated: one node, or
`nodes` of them, each running `program workload count`. `performs` belongs to a workload alone: it
runs the workload and the same with a count of 0, both traced and natively, best of `best_of`, and
divides what is left by the count; a workload of several nodes, or one with faults, has no native
form and reports none.

A run that did not complete is never counted as clean: `never_finds` over a range where runs died
does not hold. `--quick` caps `repeats` at ten and skips every check that sweeps seeds; `--plan`
prints what would run and runs nothing. Results go to `<dir>/<subject>.json`, one object per check
with what was measured and the bound. The exit code is 0 when every check carried out held, 1 when
one did not.

`bohr sweep` is one `finds` or `never_finds` as a command: each seed once, then how many found what,
and on which seeds. With `--expect` and `--at-least` it exits 0 only when the bound held.

What these do not do: choose a bound, or judge what a program means. The bound is written in the
subject before the run; what a run is judged by is still its scenario's oracle, the guest's SDK
assertions and its probes.

### 7.10 Exit codes

| Code | Meaning | CI reading |
|---:|---|---|
| 0 | clean; nothing found; digests matched | green |
| 1 | **a finding** — property violated, crash, or hang | red, and this is a success for the tool |
| 2 | usage or configuration error | red, fix the invocation |
| 3 | the tool could not carry the run: an internal error, or a program it cannot model | red, file an issue |
| 4 | determinism could not be guaranteed and `--strict-determinism` was set | red |

The separation of 1 from 2 and 3 is the contract CI pipelines depend on: "the tool found a bug" and
"the tool is broken" must never share an exit code.

### 7.11 Other commands

**`bohr selftest [--seed N] [--repeat N] [--guest NAME] [--save-dir DIR]`** runs the guests built
into the binary and requires every repetition to reproduce. It needs no program on disk, no
privileges and no particular operating system, so it answers "is this build deterministic on this
machine" wherever the binary runs — including hosts where the syscall backend cannot work at all.
Exit codes are the ordinary ones: `0` if every guest reproduced, `1` if any run diverged from its
first repetition or produced a finding.

**`bohr explain <bundle>`** tells how a stored run got to what it found: the verdict and
its code, what the run was, the steps before the violation — the faults, the node changes, the
tasks that failed, the diagnostics, nearest last, at most twelve — and the commands that show more.
For a finding a probe reported, the violation is where the probe first saw it, not where the oracle
judged the run. The steps are gathered by rule and without knowing the program; that they caused
the violation is not shown, only that they came first, and the story says so. Told from the bundle
alone, so a bundle tells one story every time.

**`bohr trace <bundle> [--format text|json] [--node N,…] [--kind K,…] [--from T] [--to T]
[--around EVENT] [--squash]`** prints the run's event log, read from the bundle rather than
replayed. The filters select by the events' own fields, never by the text of a line, so `json`
selects exactly what `text` does: `--kind` takes `syscall`, `schedule`, `task`, `packet`, `disk`,
`fault`, `node`, `wait`, `clock`, `assertion`, `record`, `diagnostic` and `domain`, one word per kind
of event, and a kind with every other kind is the whole log. `--from` and `--to` take virtual time
as written — `1.3s`, `250ms`, `40us` — read from its digits and never through floating point.
`--around` keeps twenty events before one and five after. `--squash` folds a loop of up to four
events on one task, repeated three times or more, into two lines that say how many times and over
how long; it is a view, and `json` and the bundle still hold every event. What the file carries is what it prints, so it needs neither the guest nor a
host that could run it. `text` is for reading and grepping; `json` is the JSON Lines projection
§4.3 defines, and is the same bytes a report leaves beside itself for events it did not show. A log
that ends early is printed as far as it goes and said so about on standard error, because every
event in a truncated log is true and the last one is not the end of the run.

**`bohr report <bundle> [-o report.html] [--redact]`** writes the run as one page: no framework,
nothing fetched, opens from a file in ten years' time (`ARCHITECTURE.md` §5.13). Verdict first, then
the line that reproduces it, then what a reader needs to understand either. Where the run was
probed, the page draws the chain node by node: across is virtual time, up is the key each node
answered for, the colour is the value, the stretches a node was down are shaded, and the moment the
violation was seen is a line across every lane — a revert, a fork and a stall each look different. A run too long to draw
is windowed — the neighbourhood of the failure plus every fault, every promise and every node
change wherever they fall — and the events not drawn are written beside it as `<name>.events.jsonl`,
which is the projection §4.3 defines. What is left out is counted on the page rather than dropped.
The page is meant to stay under four megabytes so it can be attached to an issue; past that it is
written anyway and says so.

`--redact` writes `<bundle>.redacted.bohr` beside the page, per the contract in §4.6. It is a
bundle rather than a filtered page because the promise is that the result still replays, and a
filter over a page would leave nothing to hand anybody. **One consequence to plan for:** the
recorded paths are hashed, so replaying a redacted bundle of a program on disk needs `--binaries`
to say where those objects are now. A guest built into the tool keeps its name, which is not a path
and discloses nothing.

**`bohr note <bundle> "text"`** appends a line to the bundle's notes, in place, and prints them
all. No timestamp and no author: two identical invocations produce identical bytes, which is the
property every stored artefact here has.

**`bohr survey [--seed N] [--config FILE] [--max-events N] -- <program>`** lists every syscall the
program asked for that this build does not model. Each is answered `ENOSYS` so the next one can be
reached, which is the whole difference from a run: `bohr run` refuses the first one and ends, so
learning a program's surface that way costs a run per call.

It is not a run and is arranged so it can never be taken for one. Its worlds are `minimal-survey`
and `distributed-survey`, so a bundle claiming one is refused by the provenance check before it
replays (§4.2); it writes no bundle at all; and it exits **4** when it found anything, never 0 and
never 1 — what it found is a fact about this build rather than a finding about the guest.

**The set is a lower bound twice over**, and the report says so beside itself. A call answered
`ENOSYS` sends the guest down a fallback path it would not otherwise have taken, so a mechanism the
survey names may be one the program does not depend on; and a call the run never reached is not a
call the program cannot make.

`bohr corpus stat|minimize <dir>` · `bohr version --verbose` (tool, tape ABI, bundle format, SDK
schema — the four numbers any bug report needs).

### 7.12 `--json` — what a command came to, for a program

**Not the text with braces round it, but a contract of its own.** Every command takes `--json`
and then writes exactly one JSON object to standard output and nothing else; the text a person
reads is not printed, progress and warnings stay on standard error, and the exit code is the one
the command exits with anyway (§7.10). A guest's own output, which `bohr run` delivers to standard
output, is not delivered under `--json`.

Every object carries `"schema": "bohr.<command>/<n>"`. **A field removed, renamed or given another
meaning is a new version; a field added is not**, so a reader skips what it does not know and
refuses a version it was not written for. `bohr version --json` lists the schema of every command
this build writes. A command that fails writes `bohr.error/1` — `code` (the `BOHR-` diagnostic the
message opens with, or null), `message`, `exit` — with the exit code it would have had.

| Schema | Fields a reader acts on |
|---|---|
| `bohr.run/1` | `found`, `headline`, `code`, `facts` (the `--facts` object), `saved`, `exit`; `repeated` false with `divergence` where a repeat diverged; `differential` for a group run |
| `bohr.replay/1` | `bundles[]`: `bundle`, `result` (`matched`, `diverged`, `refused`), `digest`, `outcome`, `events`, `detail`; `exit`, the worst |
| `bohr.explore/1` | the `--summary` account (§7.2), with `found`, `findings_dir`, `saved`, `elapsed_ms` |
| `bohr.hunt/1` | the pack and its binary, `mode`, `campaign` (the summary) or `sweep`, `findings`, `groups[]` as `bohr findings` groups them |
| `bohr.reduce/1` | `events` and `draws` before and after, `shrank`, `budget`, `replays_spent` per pass, `out`, `from_run` |
| `bohr.report/1` | `kind` (`page`, `disclosure`, `sarif`), `bundle`, `written[]`; `results` for SARIF |
| `bohr.findings/1` | `findings`, `groups[]`: `signature`, `verdict`, `code`, `count`, `smallest`, `bundles[]` |
| `bohr.explain/1` | the story: `found`, `headline`, `code`, `run`, `steps[]`, `next[]`, `bundle` |
| `bohr.trace/1` | `events[]`, each the JSON Lines record of §4.3; `truncated` |
| `bohr.survey/1` | `outcome`, `events`, `completed`, `walls[]`: `nr`, `name`, `calls` |
| `bohr.doctor/1` | `verdict`, `checks[]`: `name`, `status` (`pass`, `warn`, `fail`, `skip`), `detail`, `remedy` |
| `bohr.check/1`, `bohr.sweep/1` | each subject's account as written to its file; a sweep's per-seed `facts` |
| `bohr.init/1`, `bohr.note/1`, `bohr.selftest/1`, `bohr.version/1` | what was written, the notes, each guest's digest, the four contract numbers |

The objects of the commands that need no backend — `version`, `explain`, `trace`, `findings`,
`note`, `report`, `error` — are held to golden files in `crates/bohr-cli/tests/golden/json/`, and
the test fails on any difference under the same schema name: a change is versioned or it does not
merge. The others are built by the same code and checked by the tests that run them on Linux.
Under `--json`, `bohr trace` writes every selected event, and `--format` and `--squash` do not
apply.

What it does not do: stream. A campaign's object arrives when the campaign ends; `bohr mcp` is
where a long one is followed while it runs.

### 7.13 `bohr mcp` — the commands as tools for a coding agent

```
bohr mcp
```

Serves the Model Context Protocol over standard input and output, one JSON-RPC message a line:
the tools `run`, `survey`, `explore`, `replay`, `reduce`, `report` and `findings`, with arguments
named as the command's options are, and `status` and `cancel`.

**A tool is not a second implementation of its command but a way of starting it.** Each call runs
this same binary with `--json` and answers with the object it wrote, as structured content and as
its text, so what an agent is told and what a pipeline reads are one object by construction.
`explore` and `reduce` take minutes, so they answer at once with a `handle`; `status` with that
handle says whether it is still running, the last progress line the command wrote, and once it has
ended its exit code and its object; `cancel` ends it. A status is answered from memory and takes
well under a tenth of a second.

What it does not do: run anything the command line could not, keep a job past the server's own
life — closing its input ends every job it started — or decide for the agent what a finding means.

---

## 8. Error catalog

Codes are stable forever and never reassigned. Every error prints the code, what happened, and what
to do.

| Range | Class |
|---|---|
| `BOHR-1xxx` | usage and configuration |
| `BOHR-2xxx` | determinism hazards |
| `BOHR-3xxx` | backend and host |
| `BOHR-4xxx` | bundle, tape and format |
| `BOHR-5xxx` | internal |

| Code | Meaning |
|---|---|
| `BOHR-1001` | target program not found or not executable |
| `BOHR-1002` | invalid configuration value, with the file, key and reason |
| `BOHR-1003` | probability is not a rational literal, or its denominator exceeds 65536 (§5.2) |
| `BOHR-1004` | conflicting options (`--seed` with `--tape`) |
| `BOHR-2001` | **unsupported syscall** — the syscall number, task, and a link to the support table |
| `BOHR-2002` | unsupported syscall argument shape on a supported syscall |
| `BOHR-2003` | `RDRAND`/`RDSEED` found in a mapped executable page and not neutralised |
| `BOHR-2004` | `io_uring`: a ring opened or declined (diagnostic), or an operation, flag or call the ring does not carry (refusal) — §5.1 |
| `BOHR-2005` | branch counter unavailable; falling back to syscall-granularity progress |
| `BOHR-2006` | guest read a clock in a tight loop with no other syscall — likely a wall-clock spin |
| `BOHR-2007` | processor identification answered from the presented profile rather than the host |
| `BOHR-2008` | a node started with SDK fault points armed, named |
| `BOHR-2010` | regenerated draw journal disagrees with the stored one (§2.6) |
| `BOHR-2011` | trace digest mismatch on replay, with the first diverging event |
| `BOHR-2012` | a per-run assertion the guest makes about itself did not hold |
| `BOHR-2020` | SDK randomness fell back to the system RNG (§6.2) |
| `BOHR-2021` | gap in the SDK record sequence — records escaped interception |
| `BOHR-2030` | a healed cluster committed nothing for the span `[liveness]` named (§5.6) |
| `BOHR-2031` | a node answered with writes that were not durable and came back without them (§5.6) |
| `BOHR-2040` | a probe saw a value at a key change on one node after it was seen; `finality` (§5.9) |
| `BOHR-2041` | a probe saw two nodes answer differently for one key; `agreement` (§5.9) |
| `BOHR-2042` | a probe saw a value stay put for its span while the cluster was whole; `liveness` (§5.9) |
| `BOHR-2043` | a probe saw one key carry two values anywhere; `double-sign` (§5.9) |
| `BOHR-2044` | a probe saw a number go down on one node; `monotonic` (§5.9) |
| `BOHR-2045` | a probe holds 65,536 keys and compares no new ones (§5.9) |
| `BOHR-3001` | `ptrace` denied; `kernel.yama.ptrace_scope` |
| `BOHR-3006` | the backend cannot hand a guest to a debugger |
| `BOHR-3002` | unprivileged user namespaces disabled |
| `BOHR-3003` | seccomp filter installation failed |
| `BOHR-3004` | `PR_SET_TSC` unsupported, or CPUID faulting not exposed by this platform (PLATFORMS.md §2.1) |
| `BOHR-3005` | guest died in a way the backend cannot describe (a Bohr bug or a hostile guest) |
| `BOHR-3007` | an operation named a task that does not exist or is not stopped |
| `BOHR-3008` | a guest memory access fell outside the guest's mappings |
| `BOHR-3009` | a resumption made no sense for the stop it was applied at |
| `BOHR-3010` | the host refused something the backend needed, naming what it was doing |
| `BOHR-3011` | the guest given to a backend is malformed: a scripted program that cannot be executed |
| `BOHR-4001` | bundle header or section table is not one this tool can read: magic, version, or a section number appearing twice |
| `BOHR-4002` | section hash mismatch, naming the section |
| `BOHR-4003` | reserved field non-zero |
| `BOHR-4004` | binary fingerprint mismatch on replay, naming the object and both hashes |
| `BOHR-4005` | tape ABI of the bundle is newer than this tool supports |
| `BOHR-4006` | bundle was produced by a different world model or oracle, naming both (§4.2) |
| `BOHR-4007` | bundle holds multiple executions and this tool or command supports only one (§4.2) |
| `BOHR-4010` | replay of a redacted bundle needed a removed payload (§4.6) |
| `BOHR-5001` | internal invariant violated — please file an issue with the bundle |
| `BOHR-5002` | invalid draw request (`draw_range(0)`) |
| `BOHR-5003` | the bundle handed to a reduction or a disclosure records nothing going wrong |
| `BOHR-5004` | a differential run compared nothing: an implementation produced no projection (§5.8) |
| `BOHR-5005` | this world showed the guest a state no machine could, so the run is not evidence about it (§5.1) |
| `BOHR-5006` | a campaign armed adversity and none of it landed, so its clean runs are about a guest under no pressure |

---

## 9. Versioning and compatibility

Four independently versioned numbers, all four printed by `bohr version --verbose` and all four
recorded in every bundle:

| Number | Bumped when | Compatibility rule |
|---|---|---|
| `tool_version` | any release | SemVer |
| `tape_abi` | the draw ABI or the extension keystream changes (§2.3, §2.4) | a tool **refuses** to replay a bundle with a different `tape_abi`, with `BOHR-4005`. There is no "best effort" here: a run derived from differently-interpreted bytes is a different run |
| `bundle_format` | a section is added, removed, or its encoding changes | a tool reads any `bundle_format ≤` its own; unknown sections are preserved (§4.1) |
| `sdk_schema` | a record field is added or its meaning changes | additive only; a reader ignores unknown fields inside `bohr`, and never inside `antithesis_*` |

**The rule that protects users:** a tool upgrade may never silently change what a stored bundle
means. It either replays it identically, or refuses and names the version difference. A finding a
team saved six months ago is evidence; evidence that quietly re-interprets itself is no longer
evidence.

### 9.1 Deprecation

A CLI flag or configuration key is removed in two steps: one minor release where it works and warns
with the replacement named, then removal at the next major. An error code is never removed — it
becomes "retired" in this table, so an old CI log stays interpretable.

---

## 10. Index of data paths

The end-to-end traces live in [`ARCHITECTURE.md`](./ARCHITECTURE.md) §7 and are cross-referenced
here so this document is a complete entry point:

| Path | Where |
|---|---|
| guest syscall → recorded event | `ARCHITECTURE.md` §7.1 |
| `sendto` → delivery on another node | `ARCHITECTURE.md` §7.2 |
| seed → campaign → minimal reproducer | `ARCHITECTURE.md` §7.3 |
| SDK call → property state | `ARCHITECTURE.md` §7.4 |
| bundle → replay → digest comparison | §7.3 and §4.4 of this document |
| bundle → HTML report | §7.11 and `ARCHITECTURE.md` §5.13 |
