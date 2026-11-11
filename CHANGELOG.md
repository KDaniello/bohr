# Changelog

Written for someone deciding whether to upgrade. Format and section order in
[`docs/repo/release.md`](./docs/repo/release.md).

Four numbers version independently and are recorded in every bundle: `tool_version`,
`tape_abi`, `bundle_format`, `sdk_schema` ([`docs/API.md`](./docs/API.md) §9).

## v0.9.2 — what was acknowledged survives

v0.9 could say a node broke only when a process died. What that misses is the commoner and costlier
failure: a node that lives, answers, and no longer holds what it acknowledged. v0.9.2 judges it with
four laws of durability and recovery, written once and fed by a chain or a store alike.

| Number | Value |
|---|---|
| `tool_version` | `0.9.2` |
| `tape_abi` | `2` |
| `bundle_format` | `3` |
| `sdk_schema` | `1` |

### Findings

- **The laws, R1.1–R1.4** (`[model] kind = "kv" | "chain"`). An acknowledged put read back absent or
  superseded (`BOHR-2051`), a removed value read back (`BOHR-2057`), a batch read back in part,
  acknowledged or not (`BOHR-2058`), and a node the world restarted that never answers again
  (`BOHR-2055`). A finding is placed at the line of the acknowledgement that was lost, and a law is
  told before a crash, since a node that refuses to come back exits non-zero.
- **A store is judged by what its harness says.** `kv.begun`, `kv.acked`, `kv.read` and `kv.ready`
  records, with a helper in every SDK and `testdata/guests/bohr_kv.h` for C.
- **A chain is judged by a load.** `bohr-load`, a guest beside the probe, sends a state's pre-signed
  transactions through an `[adapter]` declaration and asks every node for every committed one after
  the fault window; `kind = "cometbft-rpc"` covers CometBFT and every Cosmos SDK chain.
- **Cells.** `bohr hunt <pack> --cells` hunts each `scenario-s*.toml` of a pack: S0 clean, S1 kills
  in the commit path, S1f kills in the first start, S3 power loss with torn and reordered writes.

### Fixed

- `rpc`, the probe's JSON-RPC client, is shared with the load; a reply is read one way.

### Determinism

Nothing moved: the laws judge records and the world's own timer, so a finding replays, reduces and
ablates as any other.

### Not done

- **The Cosmos packs are written and not built.** `cosmos-simd-env`, `cosmos-hub-env` and the
  candidates (Osmosis, Neutron, dYdX v4, Celestia) are built from source with a Go toolchain and
  modules fetched from the network, which the host this release was made on does not have. Their
  hunts, their surveys and the gate that a Cosmos chain is a pack wait for that build.
- **The redb and mdbx harnesses keep their own summary.** Their move onto `kv.*` records is not done.

## v0.9.1.1 — the ground under the findings

A review of the whole tool after v0.9.1 found 86 defects: answers the model gave unlike Linux, runs
whose digest depended on the host, verdicts that counted the wrong thing, bundles from others that
could reach past the guest, costs that grew with the run, and a design that made each of these easy.
This patch fixes all 86 before the v0.9.x series goes on. It adds no class of error.

| Number | Value |
|---|---|
| `tool_version` | `0.9.1+1` |
| `tape_abi` | `2` |
| `bundle_format` | `3` |
| `sdk_schema` | `1` |

### Breaking

- **Bundles of the old models are refused by version.** The model fixes change what a run answers:
  the `distributed` world is version 4 and `minimal` version 3, and a bundle recorded under an older
  one is refused with `BOHR-4006`, both versions named, instead of reported as diverged.
  `bohr replay --record <file>` runs such a bundle under this build and writes it anew; whether it
  holds the same finding is for its reader.
- **Every guest is executed by a 512-byte path.** The host path is made absolute and padded with
  leading slashes; a path longer than 512 bytes is refused. `AT_EXECFN`, `argv[0]` and
  `/proc/self/exe` are the name the scenario starts the program by, so `--binaries` no longer
  refuses a directory longer than the recorded one.
- **`Outcome` carries a kind and a code, not text.** Diagnostic codes are `BOHR-NNNN` numbers in
  one type, and `--json` errors carry `code` and `message`.

### Fixed

- **The model.** `fcntl` locks and status flags, `O_NONBLOCK` and `SOCK_NONBLOCK` on sockets and
  pipes, `shutdown(SHUT_WR)`, `MSG_PEEK` and `MSG_DONTWAIT`, `rmdir` and `rename` of a non-empty
  directory, `O_APPEND` positions, `ENOSPC` only where a write allocates, `dup` of sockets and
  counters, `SO_RCVTIMEO` and `SO_SNDTIMEO`, descriptor numbers under `RLIMIT_NOFILE`,
  `socketpair` types and `EFD_SEMAPHORE`, timers armed on a drifting node's own clock, a datagram
  to the node itself, overlapping cuts, a restarted node's symlinks and `/proc/self/exe`, and a
  gathering write past an empty segment answer as Linux answers them.
- **Processes.** `exit_group` by a child process ends that process, not every task of its node.
  `CLONE_UNTRACED` is cleared before the host sees it, and a task in new namespaces is refused.
- **Verdicts.** A clean bundle is not a finding; a re-run that went elsewhere is kept apart under
  its own name; ablation runs a control and checks provenance and state; scheduled faults count
  when they fire; a resumed campaign groups and stops as an uninterrupted one; a failure of the
  probe alone is not the target's; `reduce` checks its baseline reproduces, and reads the events
  of every candidate.
- **Bundles from someone else.** Replay, reduce and ablate run only programs the bundle vouches
  for; decompression, paths and node counts are bounded; bundle strings are printed without
  terminal escapes; `--redact` covers `[cluster] programs` and the state. The probe and the
  `--patch-cpuid` copy run from `bohr-<uid>` under the temporary directory, created 0700 and refused
  when anyone else could write it, each file named by its content and written `O_EXCL`. The spool
  is created `O_EXCL`. A failed attach kills the guest instead of letting it run untraced.

### Performance

- **A finding's re-run is a worker's job.** It runs in a process of its own while the pool runs
  the next generation, where before every worker waited for it.
- **Logs held only where read.** `run --save` streams its bundle; `hunt --seeds` and replay only
  digest; reduce and the reducer read a spool. The draw journal of a campaign run keeps the 256
  draws the corpus takes, where it kept about 32 MB per million system calls, and SDK records are
  folded as they arrive, where a worker held 53 MB of them.
- **Fewer copies and crossings.** A prepared state is shared between nodes and copied per block
  on write; a gathering write reads its segments in one `process_vm_readv`; a guest binary of
  95–111 MB is hashed once per invocation, not per generation; an edge-triggered poller wakes on
  its own socket's edges only; the spool compresses on a thread of its own.

### Determinism

Moved to `solved` in [`DETERMINISM.md`](./docs/DETERMINISM.md): `rseq`, which is refused, and
`sched_getcpu`, answered core 0; the auxiliary vector, laid out in one canonical form whatever the
host's kernel and CPU; the executed path; the host's stack, address-space and data limits, set to
what the world answers; the signal state the supervisor would hand down; `MADV_FREE`, carried out
as `MADV_DONTNEED`; and the order in which the threads of a process a signal ended are reported.
One guest run from directories of two lengths, on two cores and under `ulimit -s unlimited` reaches
one digest (`host_independence::the_host_does_not_reach_the_guest`).

## v0.9.1 — honest findings

v0.9's campaigns returned findings that took hours each to read, and some of them were the tool's
own: a run in a mode nobody runs in production, a host confirmation whose node never started. v0.9.1
makes every finding say what it is before anyone reads it: where the program fell, which faults it
needed, how real those faults are, and a verdict that keeps everything but a defect of the target
out of the count. A campaign stops when it has nothing new to say, and a campaign of a four-validator
network no longer runs its host out of memory.

| Number | Value |
|---|---|
| `tool_version` | `0.9.1` |
| `tape_abi` | `2` |
| `bundle_format` | `3` |
| `sdk_schema` | `1` |

Format 2 bundles still replay. A tool of format 2 refuses a format 3 bundle: format 3 adds the
guests' output, which an older `--redact` would carry through unaware.

### Where it fell

- **Guest output in the bundle.** A run whose log is kept keeps the last 64 KiB each start of each
  node wrote to standard output and error, in section `GUEST_OUTPUT` (13), outside the trace digest.
  `--redact` removes it.
- **The place, read from it.** A Go panic or runtime fatal error gives the first frame outside the
  standard library, with its module and version; a Rust panic the line it was raised at, with its
  crate and version; otherwise the last line logged at a fatal or error level, and otherwise the
  failing node's last line. `bohr explain` prints it as `Where it fell`, and `--json` as `where`.
- **Groups by place.** `bohr findings` groups by the programs, the violation and the place, so two
  panics at two frames are two groups and one defect at two heights is one.

### What a finding is

- **`bohr ablate <bundle>`** repeats a finding once per kind of fault that landed, with that kind at
  nought odds and every draw kept, and names the kinds the finding needed.
- **Grades.** Every fault kind is graded `real`, `plausible`, `model-only` or `adversarial`, each
  with its source in `docs/DETERMINISM.md` §5. A finding's grade is the weakest among the kinds it
  needed.
- **Verdicts.** `bohr findings --ablate`, and every `bohr hunt`, gives each group a verdict: a
  target defect, a target defect under a plausible or adversarial condition, a model artefact, an
  expected refusal, a bench failure (`[cluster] roles`), out of scope (a pack's
  `[scope] nonproduction`), or a limit of bohr. The summary counts the groups that are a defect of
  the target; every bundle stays and every group is printed.
- **The Ethereum packs run in `--dev`.** geth, reth and op-reth declare it non-production, so their
  findings are out of scope until they are run as a network runs them.

### Campaigns that stop

- **`--quiet-runs N`** stops a campaign N runs after its last new group. A hunt with no budget runs
  `--quiet-runs 1000 --time 15m`. Each group's first run and its seconds into the campaign are
  written to `first-seen.json` and printed by `bohr findings`.
- **`--time` stops near its budget.** Past it no run is handed out, and a generation cut short is
  folded as one cut by `--runs` is. A network whose runs take 26 s ran 28 minutes on a 15-minute
  budget; it now stops at the budget.
- **A fault window.** `faults.disarm_after_ticks` closes it for every drawn fault, node, disk,
  memory and network, at nought odds with every draw kept. A probe times a stall from the close.

### Large runs

- **A campaign's memory.** A worker sent back the whole draw journal of each run and the campaign
  held a generation of them. On four CometBFT validators the campaign grew by 250 MB a minute and
  the kernel killed it at 3.3 GB; a worker now sends the 256 records the corpus uses, and the same
  campaign stayed at 143 MB over 28 minutes.
- **Streaming bundles.** A finding's events are compressed into a file as they are recorded, and the
  bundle is written by copying it; the event log is never held whole. `EVENTS` may unpack to 64 GiB;
  every other section keeps the 1 GiB bound.

### The brand

- **A mark, a wordmark and banners**, drawn by `examples/brand` (`make brand`) into `docs/assets`.
  The README's banner draws the tape `bohr_tape::Tape::from_seed` gives for seed 1913, three bytes a
  spectral line, beside hydrogen's four Balmer lines.

### Fixed

- **`bohr replay --binaries` across machines**, in the nightly: the traced guest travels with its
  bundle and replays through `--binaries`, and the hunting job installs `protoc`.
- **A network bundle written by an older release.** Every release builds a new probe, so such a
  bundle is refused with `BOHR-4004`; the refusal now says to name the old probe in `BOHR_PROBE`.
  `make bank` keeps each finding's probe beside its binaries. Two of five banked findings had
  stopped replaying at the change of version.

### What it does not do

- **Reach a rare window in a quiet pass.** A group a v0.9 campaign first reached at 19% of its
  run, about once in 18 000 runs, is missed by the default quiet budget. A scenario whose faults
  can land only in that window reaches it; a longer campaign is not the remedy.
- **Run op-reth.** It was not built on the 4 GB host, and its clean run and its hunt are not
  measured.
- **Read a large bundle in pieces.** A reader still unpacks `EVENTS` whole; only writing streams.

## v0.9.0 — the hunt

Bohr ran campaigns on four targets and had no way to keep what it found, to check it outside its own
model, or to say which of its claims hold. v0.9 adds all three: a private bank that replays every
stored finding on every commit, `bohr confirm`, which stages a finding on a real Linux host without
bohr, and a table of the nine claims with the figure that decides each. It also adds a pack for a
four-validator CometBFT network driven by real transactions. What the campaigns found stays private
until its maintainers have it.

| Number | Value |
|---|---|
| `tool_version` | `0.9.0` |
| `tape_abi` | `2` |
| `bundle_format` | `2` |
| `sdk_schema` | `1` |

Every bundle of v0.8 replays to its digest. Figures are under `[v0_9.*]` in
[`benches/baseline.toml`](./benches/baseline.toml). `make pr` is green on Ubuntu 24.04, with 728 tests in
`test-fast` and 60 in `test-linux`. Three of the nine claims hold; the rest are listed below.

### Campaigns

- **Four targets, master seed 0, on a 4 GB host with eight cores**: geth and CometBFT for an hour
  each (3,072 and 3,232 runs), redb and reth for thirty minutes (98,784 and 24,640 runs).
- **`targets/cometbft-net`**: four `cometbft` validators at the addresses the world gives nodes
  0–3, with `examples/comet-driver`, a static client that sends transactions in rotation, waits for
  each to commit and then asks every validator for every committed key. A key that a validator lost
  ends the driver with an abort, so a lost transaction is a finding with no oracle of its own.
  `build-state.sh` rebuilds the network's state from the pack's binary.

### Confirmation on a real host

- **`bohr confirm <bundle> [-o confirm.sh] [--runs N]`** writes a script that stages a stored
  finding without bohr ([`docs/API.md`](./docs/API.md) §7.11). The script runs the recorded
  programs as plain processes, each in a root and a working directory of its own. It delivers the
  recorded kills, freezes and restarts in order and judges each repetition by the recorded exit
  status. Its last line is the rate.
- **A crash loses unsynced writes, on the node it kills.** Each node's root sits on a device-mapper
  `flakey` device of its own that drops writes from the node's kill onward; `POWER_LOSS=0` stages a
  plain kill instead.
- **A repetition that cannot be staged is void and is not counted**: a node that outlives its kill,
  a node that has already exited when its fault comes, an unmount that does not happen, a filesystem
  `e2fsck` finds damaged. Each repetition starts on fresh filesystems, and each node's address goes
  on the loopback device for the run.
- **Real time is not virtual time.** Each gap between faults is drawn up to twice the recorded one
  and at least up to `SPREAD_NS`, 2 s by default, because an early moment of a run comes later on a
  real host.

### The bank and the claims

- **`make bank`** replays every finding kept in `findings/bank/` to its digest and fails on one
  that does not. An entry keeps its bundles, the binaries it was found on and the prepared state it
  began from, and is replayed from its own directory. The bank is private and is not part of a
  published tree.
- **`make claims`** prints each claim with the figure that decides it, read from
  `benches/baseline.toml`: held, failed, or not measured. A claim without a figure is never
  reported as held.

### Fixed

- **`bohr replay --binaries` ran a file other than the one it checked.** When the scenario named
  its programs, the replay checked the file under `--binaries` and executed the path the scenario
  named. When the scenario named none, it put the new path into `argv[0]`, which moved the guest's
  stack. Now every node executes the checked file and keeps the recorded `argv`. The executed path is
  padded with slashes to the recorded length, because the kernel copies it onto the stack. A moved
  program replays to its digest; under v0.8 it could not start.
- **Every doc comment and code comment was rewritten** to state its contract directly, and several
  that had been merged into the wrong item were split.

### What it does not do

- **Hold claim 4.** Nothing is disclosed, so no maintainer has confirmed a finding (G9.4).
- **Count refusals.** A campaign does not record the runs the model refused, and the 60-seed sweep
  G9.1 asks for was not run, so claim 1 is not measured.
- **Confirm the two findings held since v0.1 (G9.12).** Their bundles record no binaries to stage.
- **Hold P9.1.** The supervisor and workers together used 102 MB on average over minutes 1–5 of the
  geth campaign and 126 MB over minutes 26–30, against a bound of 10%. The figure is a sum over a
  changing set of workers sampled once a minute: three adjacent samples read 81, 155 and 109 MB.
  P9.2 holds: 48 runs a minute over minutes 8–12 and 55.8 over minutes 26–30.
- **Hunt op-reth, Lighthouse or Solana.** They had no campaign.
- **Hunt a network for long.** Four validators produce about 400,000 events per virtual second, so
  the event budget and the 1 GiB bundle section end a run at about 20 virtual seconds, and saving a
  finding of a network needs 1–3 GB, more than a 4 GB host has left beside the campaign.
- **Stage partitions, packet faults, disk errors, torn or reordered writes, or clock faults** on a
  real host. `bohr confirm` names the ones a run had and leaves them out.

## v0.8.0 — where it runs

Bohr was built from source by someone who had this repository, and read by a person at a terminal.
v0.8 makes the artefacts v1.0 ships and checks each from an empty home: a static binary that
reproduces, a signed installer, an image, a GitHub Action, and an interface for coding agents; and
every command answers a program in JSON. Nothing is published: no tag, no image, no release page.

| Number | Value |
|---|---|
| `tool_version` | `0.8.0` |
| `tape_abi` | `2` |
| `bundle_format` | `2` |
| `sdk_schema` | `1` |

Every bundle of v0.7 replays to its digest. Figures are under `[v0_8.*]` in
[`benches/baseline.toml`](./benches/baseline.toml). `make pr`, `make gate` (13 of 13) and the
mutation harness (13 of 13) are green on Ubuntu 24.04, with 740 tests; three gates were measured
short of how they are written, and are listed below with what was measured instead.

### JSON, for a program

- **`--json` on every command**: one object on standard output and nothing else, its schema named
  in it — `"schema": "bohr.run/1"` — and moved only when a field is removed, renamed or changes
  meaning. A failure is a `bohr.error/1` object with the exit code it would have had, a command
  line clap refuses included. A guest's own output is not delivered under `--json`.
- `bohr version --json` lists the schema of every command; the objects of the commands that need no
  backend are held to golden files, and a change under the same schema name fails the build.
- `bohr findings --json` and `bohr explain --json` are the same flag as before, now global.

### For a coding agent

- **`bohr mcp`** serves `run`, `survey`, `explore`, `replay`, `reduce`, `report` and `findings` as
  Model Context Protocol tools over stdio. Each runs this binary with `--json`, so an agent is told
  exactly what a pipeline reads. A campaign and a reduction answer with a handle; `status` gives the
  last progress line and, at the end, the object, and `cancel` ends the job's whole process group.
  Each request is answered on its own thread, so a long `run` does not hold up a `status`.

### A release, built and checked here

- **A static binary that reproduces.** The `dist` profile keeps symbol names and strips line
  tables: 8,406,368 bytes against P8.1's 20 MB. Built twice, from two directories, with the
  environment `release.yml` sets, it is one SHA-256, `70f6c8a7…`.
- **`bohrup`.** Refuses a host other than Linux on x86-64 before any download; checks an
  `ssh-keygen -Y` signature over `SHA256SUMS`, then every file against it; installs the binary and
  the target packs to `~/.bohr` and links `~/.local/bin/bohr`; runs `bohr doctor`. From a local
  release into an empty home it took 112 ms; with one checksum line altered, and with the packs
  archive altered, it installed nothing and said which.
- **Packs off a checkout.** `bohr hunt <name>` looks under `targets/` here, then under
  `~/.bohr/packs`, where the release's `bohr-packs-<version>.tar.gz` goes.
- **An image**, `ghcr.io/kdaniello/bohr:<version>`, built from the release's own files on
  `debian:12.8-slim`: 127 MB. With no command it runs `bohr doctor`, which under Docker's defaults
  fails on namespaces and address layout and with the documented flags passes both.
- **A GitHub Action**, `KDaniello/bohr/action`: installs with `bohrup`, hunts a pack, and on a
  finding attaches the bundles, a page, a disclosure draft and SARIF and fails the job. Its own
  hunt step, run as written on `chain-toy` over seeds 1–20: with `--revert-finality` it exits 1
  with 15 findings in 3,983 ms; without it exits 0 with none in 3,374 ms.
- **`release.yml`** runs the whole of this on `workflow_dispatch` and keeps it as artefacts; only
  a pushed `v*` tag publishes, refuses to without the release key, and checks the tag against the
  workspace version.

### Fixed

- **The redb pack named a binary its own build no longer produced.** Two builds, from the
  repository and from the installed packs, agree on `07c27bd7…`; the pack named `b1f4569e…`, so
  `bohr hunt redb` refused its own build. It names the one that reproduces.
- **The musl build did not compile.** glibc declares a ptrace request unsigned and musl signed;
  `make build-musl` had never been run.

### Speed

Against v0.7.0's binary on one host, alternately, the mean of two rounds and of three for the
campaign: an intercepted call costs 4.25 µs against 4.15, a guest starts in 3.07 ms against 2.91,
a 64-run campaign takes 3,102 ms against 3,043 at one worker and 1,037 against 1,027 at four, and a
worker peaks at 14.4 MB against 14.0. Each is within the 10% P8.4 allows.

### What it does not do

- **Go from nothing to a campaign in three commands on a stock Ubuntu 24.04 (G8.2).** Ubuntu
  refuses user namespaces to any program no AppArmor profile names, so `bohr doctor` fails on
  `namespaces` after the install and prints the one `sudo` command that allows the installed
  binary. Without it the rest still ran from an empty home — the pack built its target from the
  installed packs to the digest it names, and `bohr hunt redb --time 5m` ran to its report in
  303 s — with every run degraded, a guest not isolated.
- **Build on two runners (G8.1).** The two builds were two directories on one host; the
  two-runner comparison is in `release.yml` and has not run, because nothing here was dispatched.
- **Fail a pull request (G8.3).** The action's step was run as written, outside GitHub; no test
  repository and no pull request were made.
- **Publish anything.** No release key exists: it is the repository secret `BOHR_RELEASE_KEY`, and
  a person makes it. There is no `bohr-action` repository; the action is `KDaniello/bohr/action`.
- **Stream.** A campaign's object arrives when it ends; an agent asks `status` for the rest.

## v0.7.0 — findings at real scale

A finding against a real client is not yet something to send: the bundle holds the whole run, and
the maintainer it goes to has nothing to read. v0.7 shrinks a real client's finding by moving its
faults as well as removing them, writes the letter a maintainer receives and the SARIF code scanning
reads, and stops a long reduction from filling the host with the guests of runs it has finished. It
is not published.

| Number | Value |
|---|---|
| `tool_version` | `0.7.0` |
| `tape_abi` | `2` |
| `bundle_format` | `2` |
| `sdk_schema` | `1` |

Every bundle of v0.6 replays to its digest: the tape is read as it was, and a journal is stored
byte for byte as before. Figures are under `[v0_7.*]` in
[`benches/baseline.toml`](./benches/baseline.toml). `make pr`, `make gate` (13 of 13) and the
mutation harness (13 of 13) are green on Ubuntu 24.04, with 715 tests; one gate of this version
fails, two are not measured and one half of a fourth was not done, and are listed below with what
was measured instead.

### Reduction

- **Faults moved as well as removed.** After the configuration, `bohr reduce` spends a fifth of
  its budget on the faults that fired: those the failure does not need are dropped, and a node
  fault it needs is fired at an earlier chance — the first after the fault before it, then an
  eighth, a quarter and half of the way — so the run before it shrinks with it. Then the events,
  then the tape. The configuration stays first: it is read before the tape, and an entry removed
  would move every fault moved before it.
- **No scenario is replayed twice.** A candidate already answered is skipped and not charged to
  the budget.
- **The same violation, not a neighbouring one.** A candidate is kept only where its class and its
  signature equal the original's, so a reduction cannot drift to another failure that is smaller.
- **Measured on two real clients**, at `--budget 200`:

| Finding | Events | Kept | Replays | Wall time | 100 replays of the result |
|---|---:|---:|---:|---:|---|
| a real client, A | 27,577 → 5,923 | 21.5% | 200 | 471 s | 100 of 100 |
| a real client, B | 1,278 → 1,267 | 99.1% | 200 | 688 s | 100 of 100 |

  Which client and seed each is stays out of the tree until its maintainers have it. In A the
  configuration pass removed what the failure does not need in three replays, and every fault left
  is one it does; what is left is what the node does between them, thousands of events no edit of
  the tape removes. B is short to begin with, and there is little before its failure to take away.
  v0.6.0's reducer reaches 5,847 events on A in 25 replays, 76 fewer, with the same violation:
  on this finding v0.7 is not smaller, and its faults pass has nothing left to move.

### Reports a maintainer receives

- **`bohr report finding.bohr --disclosure -o report.md`**: a Markdown draft ordered by what a
  maintainer asks first — is this my program, can I make it happen, how far can I believe it. The
  program and its blake3, and the source commit where a target pack names that digest; the
  violation and the steps before it; the reproduction command with the digest it must reach; and
  what a run under a model cannot claim. Nothing is sent.
- **`bohr report findings/ --sarif -o findings.sarif`**: SARIF 2.1.0, one result per finding group
  and never one per seed, led by the group's smallest bundle. The rule is the violation's code, the
  location the guest's own source line where its records name one and the bundle where they do not,
  and a fingerprint stable across campaigns keeps one alert open for one defect.

### Fixed

- **A long command kept the guests of every run it finished.** A guest alive at the end of a run
  stayed stopped until the process exited, so a reduction of a real client held 61 stopped nodes of 59 MB
  each and ran the 4 GB host out of memory in 47 minutes. Every task the backend knows of, the
  threads and children found under them, and the nodes killed whose deaths were not yet collected,
  are now killed and reaped as the run is dropped, without a blocking wait: an isolated guest is
  the init of its own pid namespace, and an init that exits waits for its traced threads to be
  reaped by the very process that would be waiting for it. What is still there after a second is
  left and said.
- **The plainer-draws pass fired faults.** It wrote zeros over a disk latency's or a memory
  refusal's odds, and zero is the value that fires; every odds draw is now written as the one that
  does not.
- **The reduction test reduced nothing.** Its campaign never found anything, so it passed on an
  empty input; it now reduces a real finding and replays the result.

### Speed

Against v0.6.0's binary on one host, alternately, the mean of four rounds and of six for the
campaign: an intercepted call costs 4.1 µs against 4.0, a guest starts in 2.78 ms against 2.63, a
64-run campaign takes 2,926 ms against 2,887 at one worker and 893 against 851 at four, and a
worker peaks at 14.2 MB against 14.1. Each is within the 10% P7.3 allows.

### What it does not do

- **Shrink a real finding tenfold (G7.1).** A keeps 21.5% and B 99.1%; the bound was 10%. The
  reducer moves a finding's faults, not what a real node does between them, and a short finding
  has little else in it. Neither is a target
  pinned before a publicly fixed defect, as G7.1 asked: both are findings the packs produce today. The `op-reth` finding is not measured: at
  75 s a replay, 200 replays are four hours, past P7.2's hour before the first result.
- **Share a prefix between replays (P7.1).** The ptrace backend has no checkpoint, so every
  candidate runs from the first instruction; replay from the nearest checkpoint is not built.
- **Replay across vendors (G7.6).** There is no AMD runner; every bundle here was made and replayed
  on one vendor's processors.
- **Upload SARIF to code scanning (G7.5, second half).** The log validates against the 2.1.0
  schema; no upload to a repository was made.
- **Symbolise a crash.** A SARIF location is a source line only where the guest's own records name
  one; a signal or an exit status carries none.
- **Move a fault onto a chance that belongs to it alone.** The earlier chances a node fault is moved
  to are the node-fault draws before it, which may be another node's, and a candidate kept with no
  fault at that chance is counted as refused.

## v0.6.0 — every fault real

**The tape ABI is 2.** Every bundle recorded by an earlier release is refused by name — `BOHR-4005:
bundle tape ABI 1 is not this build's (2)` — and never replayed. A run now draws which SDK fault
points to arm before anything else, and every later decision reads its bytes from a different
place; a bundle of ABI 1 replayed under ABI 2 would be a different run that looked like the
recorded one. Record findings you need to keep again under v0.6. It is not published.

| Number | Value |
|---|---|
| `tool_version` | `0.6.0` |
| `tape_abi` | `2` |
| `bundle_format` | `2` |
| `sdk_schema` | `1` |

Figures are under `[v0_6.*]` in [`benches/baseline.toml`](./benches/baseline.toml). `make pr`,
`make gate` (13 of 13) and the mutation harness (13 of 13) are green on Ubuntu 24.04, with 693
tests; one gate of this version fails as written, and is listed below with what it measured.

### Faults that were named and never fired

- **One list of faults.** Duplication, reordering and stalls fired only as a packet's or a disk's
  fate; each now fires its own fault as well, counted once. The trace format's fault kinds, the
  world's catalogue and what it narrates are one list, and `make pr` fails on a kind in one and not
  the others.
- **Altered datagrams**: `[network] corrupt_probability`, `corrupt_bytes`. A datagram arrives with
  drawn bytes flipped, as a `net_corrupt` fault and a `corrupted` fate; never a stream, whose
  transport checks. A protocol with no checksum (`testdata/bugs/no_checksum`) applies an altered
  update on 58 of 60 seeds with corruption and on none of 60 without.
- **Writes reordered before a crash**: a crash that keeps a later unsynced write and loses an
  earlier one fires `disk_reorder`, and `[disk] reorder_unsynced = false` makes every crash keep
  all or nothing. A store that rewrites its header to point at an appended record and syncs once
  after both (`testdata/bugs/header_without_record`) is found on 11 of 60 seeds with reordering and
  on none of 60 without.
- **SDK fault points are armed**: `[faults] fault_points = [...]` and `fault_point_probability`.
  Which are armed is drawn per node once per run, put in `BOHR_ARMED_FAULT_POINTS` at every start
  of the node, a restart included, and said on the timeline as `BOHR-2008`.
- **Wall time has a date**: every wall clock starts at 2025-01-01 00:00:00 UTC, so a clock stepped
  back in a run's first minute moves back instead of standing at 1970, and a file's times are that
  date, so its age read against the clock is the run's own duration.

### Where faults land

`guided` targeting places a drawn node fault only where a crash would lose something — a node
holding writes not yet synced. Measured on every seeded defect whose scenario draws node faults,
the same 60 seeds each way:

| Defect scenario | uniform | guided |
|---|---:|---:|
| `header_without_record` | 11 | 15 |
| `header_without_record`, in order | 0 | 0 |
| `lease_clock`, drawn clock faults | 23 | 0 |

Guided wins where the defect is a crash during a write and loses everything where the fault is a
clock step on a node that writes nothing, because it never fires there. It stays opt-in.

### Speed

Against v0.5.0's binary on one host, alternately: an intercepted call costs 4.0 µs against 4.3, a
guest starts in 2.41 ms against 2.63, a 64-run campaign at one worker takes 2,771 ms against 3,003,
and a worker peaks at 14.1 MB against 14.5. Every fault armed at odds of zero costs nothing a run
can measure — 850 ms against 852, the median of fifteen — and a crash of a node holding a gibibyte
across sixteen files, with a thousand pages unsynced, builds its image within the noise of a 37 ms
run.

### Fixed

- **A test of streams measured a parked call.** A blocking read is recorded with what it answered
  when it parked, and the test read that as the length; it now reads once the answer is there.
- **Three tests of the run loop held only on one schedule.** A node acted on by another could end
  first on a different tape; each is now alive whatever the order.

### What it does not do

- **Reach `header_without_record` as often as G6.3 asked** (20 of 60): a crash decides sector by
  sector only a quarter of the time, and only then can the header outlive the record. The defect is
  reached by reordering and by nothing else, which is the claim; the rate is not.
- **Tell two adjacent writes apart once they are merged.** The disk keeps unsynced writes as ranges
  and joins those that touch, so a later write kept and an earlier lost inside one joined range is a
  tear on the timeline, not a reorder.
- **Alter a stream's bytes**, or forge what a protocol checks: an altered message is the fault a
  model of the wire can draw, and a valid forged signature is not one.
- **Learn fault points by itself.** The names a run may arm are the scenario's; a campaign varies
  which are armed, not which exist.
- **Make guided targeting the default.** It loses on a defect the measurement covers.

## v0.5.0 — the model the targets need, and checks that say so

A target that bohr cannot model is not tested: it stops at the first call nobody answers, or runs
on a model that answers it wrongly, and either way the run that comes back proves nothing. v0.5
answers what geth and CometBFT ask, makes every claim about a target or a seeded defect a check
anyone can run, says what a run found in words, and runs about twice as fast. It is not published.

| Number | Value |
|---|---|
| `tool_version` | `0.5.0` |
| `tape_abi` | `1` |
| `bundle_format` | `2` |
| `sdk_schema` | `1` |

Every stored bundle replays to its digest. Figures are under `[v0_5.*]` in
[`benches/baseline.toml`](./benches/baseline.toml). `make pr`, `make gate` (13 of 13) and the
mutation harness (13 of 13) are green on Ubuntu 24.04; two gates of this version are not measured
and one is not built, and are listed below.

### The model

- **Answered now**: `statx`, `prctl`, `chmod`, `fchmod`, `fchmodat`, `fallocate` (and a crash
  treats the bytes it reserved as written), `sync_file_range`, `sched_setparam` and
  `sched_setscheduler` as an unprivileged process is answered. `inotify_init1` is answered `ENOSYS`
  on purpose: geth falls back to polling its keystore.
- **Waits that end**: futex waits with a timeout in a world without a cluster, an endless `poll`
  that blocks until something is ready, and a timer that belongs to the wait that armed it — one
  woken early can no longer end a later wait with `ETIMEDOUT`.
- **Signals to another thread**: `tgkill` queues the signal for the named task, ends its wait with
  `EINTR`, and delivers it at its next stop.
- **Readiness as Linux reports it**: `EPOLLET` and `EPOLLONESHOT`, and a write to an `eventfd`
  wakes whoever polls it. reth stopped spinning on a descriptor reported ready forever: 352,301
  events in 5.4 s for fourteen virtual seconds, against 6.67 million in 62 s.
- **Pipes**, in a world with a `[cluster]`, as a stream pair: a read blocks on an empty pipe, a
  write wakes it, a closed write end reads as the end.
- **Memory that runs out**: `[faults] enomem_probability = "1/500"` refuses `brk`, `mremap` and
  anonymous `mmap` at drawn moments, as a fault. A store that writes through a refused mapping is
  found on 43 of 60 seeds with refusals and on none of 60 without.
- **Packs for geth and CometBFT**: each reaches thirty virtual seconds, serves its RPC to the probe
  (59 of 59 and 58 of 58 answered), repeats 100 times to one digest, and stays clean on ten seeds
  with no faults.

### A claim is a check

- **`bohr check <subject>`** holds a pack's `[check]` table or a seeded defect's `bug.toml` to
  eleven checks that are the same code for every subject: `works`, `repeats`, `replays`, `clean`,
  `adversity`, `finds`, `never_finds`, `walls`, `costs`, `workers_agree`, `refuses_changed`. A
  bound is written in the file before the run; a check or a parameter nobody reads is refused.
- **`bohr sweep <scenario> --seeds a..b --expect CODE`** is one `finds` as a command.
- **`bohr run --facts FILE`** writes what a run came to as one line of JSON for a program to read.
- **The golden guests** (`make golden`): one program with a mode per part of bohr — the stop path,
  the clock, files, syncs, mappings, pipes, futexes, threads, readiness, sockets, the network
  between nodes, the filter, a guest that only computes — each a known number of operations, held
  to `works`, `repeats`, `replays` and `performs`: microseconds per operation with startup taken out,
  the same natively, stops and events per operation, and the supervisor's peak memory. Five more put
  a workload under its own subsystem's faults. 71 checks, about seventy seconds on four cores.
- **What the checks found on their first run**: a kernel defect that made a process failing from a
  thread other than its first lose its status on some runs (fixed in v0.4.0); a seeded defect whose
  documented 60 of 60 had become 44 of 60 when the crash model began to land every unsynced write a
  quarter of the time — bisected to that change and rewritten as a bound from the model; pipes,
  never modelled; and a `msync` that copied a whole mapped file on every call.

### Reading a run

- **A run says its verdict first**: `FINDING`, what broke in words, and the code to search it by;
  then what else the outcome names, the commands that show more, and last the counters. No outcome
  is printed in a program's debug form any more.
- **`bohr explain <bundle>`** tells how a run got to what it found: the faults, the node changes,
  the failed tasks and the diagnostics before the violation, at the moment a probe saw it rather
  than when the run was judged. `--json` carries the same fields.
- **`bohr trace` selects**: `--node`, `--kind`, `--from`/`--to` in virtual time as written,
  `--around`, and `--squash`, which folds a repeating loop into two lines.
- **The report draws the chain**: one lane per node, what it answered the probe over virtual time,
  coloured by value, with the stretches it was down shaded and the moment of the violation marked.
- **`bohr findings`** words each group's verdict and names the command that explains its smallest
  run.

### Speed

Against v0.4.0's binary on one host, alternately:

| | v0.4 | v0.5 |
|---|---:|---:|
| one intercepted call | 8.7–8.8 µs | 4.0–4.3 µs |
| a 64-run `redb` campaign, one worker | 5,487 ms | 2,922 ms |
| the same, four workers | 1,683 ms | 815 ms |
| a campaign worker's peak memory | 50.1 MB | 14.6 MB |

geth's dev chain over thirty virtual seconds holds the supervisor at 92 MB, against 107 MB for geth
itself over thirty seconds run natively (P5.3, bound twice).

- **An emulated call is one stop, not two**: answered at the seccomp stop and released.
- **Nothing reads a run's log back**: the counts are kept while it is written, a run nobody will
  read is digested and not encoded, and a binary is hashed once per process.
- **A file is held in 64 KiB blocks**, each shared with the disk images taken at every sync, so a
  write after a sync copies a block and not the file; a file grown with zeros shares one block of
  them; a mapping written back copies only the pages that changed.

### Fixed

- **A futex timer belongs to the wait that armed it.** In a cluster, a timed wait woken early left
  its timer armed, and the same task's next wait on that word ended with `ETIMEDOUT` at the first
  one's deadline; v0.4.0 ends `rewaits_after_a_timed_wait` with status 1, this release with 0.
- **A thread can signal a node's first thread on every node.** `tgkill` answered `ESRCH` for the
  first thread of any node but node 0, and of node 0 after a restart.
- **A write made while a signal waits is a write.** A call answered with a signal to deliver after
  it was read as answering 0: the disk never marked the write as unsynced, so no crash could lose
  it, and the record channel was not recognised.
- **`poll` sees a node's `eventfd` and `epoll` descriptors** on a node with no socket, where it called
  them invalid.
- **A probe runs under memory refusals**: its allocations were handed to a world that models none,
  and every probed run with `enomem_probability` was refused at the probe's start.

### What it does not do

- **Preempt a thread that spins.** The instruction budget needs a branch counter, and neither host
  this was built on exposes one; G5.7 is not built, and the Solana validator, which spins, has no
  pack.
- **Run Lighthouse.** A consensus client needs an execution client beside it, and no pack pairs
  them yet.
- **Measure a pack against native** (G5.6, P5.1): no pack declares a `[bench]` workload.
- **Run a probe cheaply.** The probe costs 6.5% more events and 47% more wall time on `chain-toy`;
  each round's answers are parsed by the world, and a file appended to after a sync copies its last
  block.
- **Tell a host's out-of-memory kill from a guest's crash.** A guest the host kills reads as
  `GuestCrashed: killed by signal 9`.
- **Keep a pipe one-way.** A write to a pipe's read end is delivered where the kernel answers
  `EBADF`, and a world without a `[cluster]` still refuses pipes.
- **Rebuild `redb-hunt` to its pinned digest on another host**: the pack refuses the binary built on
  the test VM, as it must.
- **Hold G5.1 as written.** An hour of virtual time on 60 seeds is seventeen hours of reth; it was
  restated, before its runs, to twenty seeds of each pack at its own horizon. None of the sixty
  seeds ended in a model refusal.

## v0.4.0 — blockchain oracles

A blockchain client that is wrong does not crash: it finalises a block and later reports another at
the same height, signs two blocks for one slot, or keeps running and stops making progress. v0.4
judges those from the client's own JSON-RPC, without changing a line of it. It is not published.

| Number | Value |
|---|---|
| `tool_version` | `0.4.0` |
| `tape_abi` | `1` |
| `bundle_format` | `2` |
| `sdk_schema` | `1` |

Every stored bundle replays to its digest. Figures are under `[v0_4.*]` in
[`benches/baseline.toml`](./benches/baseline.toml). `make pr`, `make gate` (13 of 13) and the
mutation harness (13 of 13) are green on Ubuntu 24.04; the four performance gates of this version
fail as written, and are listed below with what they measured.

### Asking the node

- **The probe** is a guest, not a hook: a static binary (`make probe`) the world starts as one more
  node, which calls the nodes' JSON-RPC over the virtual network on a schedule drawn from the tape
  and writes the answers as SDK records. One JSON-RPC batch per node a round; the reply is read the
  next round, so a node that is slow, down or frozen costs only its own answers. Inside a run it is
  `/bohr/probe`, its build is fingerprinted in the bundle, and a replay finds it on its own host.
- **`[chain] oracles = [...]`**: `finality` (a finalised hash at a height that changes,
  `BOHR-2040`), `agreement` (two nodes with two hashes at one height, `BOHR-2041`), `liveness` (a
  finalised height that stays put once the cluster is whole, `BOHR-2042`), `double-sign` (two
  signed votes from one key at one height, `BOHR-2043`). Each is a probe written out in advance.
- **`[[probe]]`** for anything else a node answers: `never-reverts`, `monotonic` (`BOHR-2044`),
  `agrees`, `advances`, `unique`, over any method, key and value.
- **The probe's node is outside the fault model**: never killed, never cut off, never counted as
  silent, and it reaches every node whatever the topology; a value the world itself took back — a
  reorg, a restart from an older snapshot — is not held against the node.
- **`bohr trace` shows what each guest record said**, not only that one was written.

### A chain to break

- **`examples/chain-toy`**: three nodes, a block final on two votes of three, correct without a
  flag. Each flag adds one defect that the drawn faults bring out, and each oracle catches its own:
  `--revert-finality` on 35 of seeds 1–60, `--equivocate` on 60, `--fork-on-partition` on 40,
  `--stall-after-heal` on 54, against a bound of 30. Without a flag, none of the four reports
  anything on seeds 1–200. 1,000 repeats with every probe, one digest.
- **The probe costs 7.7% more events** on the same 20 seeds, against a bound of 10%.

### reth, run for real

- **The `reth` pack never ran past its start in v0.3.** reth's database grew its file past the
  world's 64 MiB a file at the tenth virtual second, was answered `EFBIG`, and shut down with status
  zero — a run the crash oracle called clean. So v0.3's "620 seeds, nothing found" measured reth
  starting. The pack now holds the database under the bound, answers `sched_setscheduler` and
  `sched_setparam` as an unprivileged process is answered, serves JSON-RPC, and is judged by
  `finality` and `agreement` as well as by crashes.
- **On reth, clean is clean**: `bohr hunt reth --no-faults` over seeds 1–30 found nothing from
  `finality` or `agreement`. The gate named 100 seeds; a seed takes about a minute on the test
  machine, and the gate was restated to 30 before the runs past the eleventh.

### Speed

- **Campaign workers live for the whole campaign** and each takes the next run as it finishes the
  last, instead of a fixed share of each generation in a process started for it. Against `redb` from
  the host's own filesystem, v0.3's binary and this one alternately: 416 against 544 runs a minute
  at one worker, 1,760 against 2,304 at eight — 31% more at both.
- **Workers take one hardware thread of every physical core before a second of any.** Linux
  numbers a core's two threads next to each other, so position by number had put eight workers on
  four of six cores.
- **Eight workers do 4.56 times the runs of one** against `redb`, where this host computes at most
  4.89 times as much on eight threads as on one: 93% of what the host can give.
- **SDK records are one write each**, terminator included, rather than two.

### A run is judged the same on every run

- **A process that fails from a thread other than its first is a finding on every run.** Each
  thread of a group that exits reports its own end, in an order the host decides, and the run
  ended on the first report to arrive while the failing thread was still in flight: its status
  was lost on some runs and kept on others. One seed of geth under drawn kills left ten digests in
  twenty runs, and a restart that exited 1 read as clean. The run now waits for a task the host is
  still running before it picks, moves time or ends; `exits_from_a_thread` repeats 100 times to one
  digest and is found every time; v0.3's binary diverges on that guest at its third repeat.
- **`bohr hunt --no-faults` runs a durability pack.** The scenario it writes kept `[durability]`
  with every kill taken out, and a durability oracle with nothing killed is refused at load.

### What it does not do

- **Read a client's protocol from its memory, its disk or its wire.** A client that exposes no
  signed votes cannot be judged for double-signing.
- **See what it did not ask.** Each node is asked for its finalised tip at drawn moments; a revert
  undone between two askings is not seen.
- **Cost little in time.** Every probe on `chain-toy` costs 26–33% more wall time against a bound
  of 10% (P4.1 fails): a probe round is a guest's work, and its calls cost what any guest's do.
- **Reach six times the runs at eight workers** (P4.0 fails as written): the host itself reaches
  4.89 times, so the bound measured the machine. It is restated for v0.5 as a share of the host's
  own ceiling.
- **Judge reth for long.** reth's RPC answers from its tenth virtual second of the pack's fourteen:
  the probe is answered 14 times of 55, so `finality` and `agreement` see the last few seconds.
- **Keep the probe's memory flat** (P4.2 fails): over 10,657 blocks every probe adds 140 MB to a
  `bohr run`, which holds its whole log, and 53 MB to a campaign worker, which holds none — about
  5 KB a block, growing with the run.
- **Keep every earlier number** (P4.3 fails): against v0.3's binary on one host, alternately, a
  campaign worker's peak memory is 50.1 MB against 44.9 MB (+12%) and `make gate` 117–119 s against
  105 s, a gate that now also repeats the probed chain 100 times. Per call (8.9 µs against 8.8 µs),
  startup (2.60 ms against 2.59 ms), a 64-run campaign at one worker (+3.4% time) and `make pr`
  (49–51 s against 50–53 s) hold.
- **Run reth quickly.** Past its start reth polls without pause, about 1.7 million calls a virtual
  second, because an `epoll` answer keeps calling a descriptor ready; a run of fourteen virtual
  seconds takes about a minute.

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
