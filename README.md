<div align="center">

# bohr

**Deterministic simulation testing for unmodified Linux programs.
Same seed, same bytes, every time.**

[![CI](https://github.com/KDaniello/bohr/actions/workflows/test.yml/badge.svg)](https://github.com/KDaniello/bohr/actions/workflows/test.yml)
[![Lint](https://github.com/KDaniello/bohr/actions/workflows/lint.yml/badge.svg)](https://github.com/KDaniello/bohr/actions/workflows/lint.yml)
[![Determinism](https://github.com/KDaniello/bohr/actions/workflows/determinism.yml/badge.svg)](https://github.com/KDaniello/bohr/actions/workflows/determinism.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE-APACHE)

[Overview](./docs/OVERVIEW.md) ·
[Architecture](./docs/ARCHITECTURE.md) ·
[API](./docs/API.md) ·
[Determinism map](./docs/DETERMINISM.md) ·
[Testing](./docs/TESTING.md) ·
[Evidence](./docs/EVIDENCE.md)

</div>

---

## What is Bohr

Bohr runs an unmodified Linux program in a deterministic world. Time, randomness, thread
scheduling, the network and the disk all come from Bohr, and every nondeterministic decision is
drawn from one string of bytes — the **entropy tape**. One 64-bit seed defines a whole execution.

On top of that sit network delays and partitions, process kills, torn writes, stalled disks,
reordered threads, and a search for the schedule that breaks the program. A finding is a file that
replays on another machine, reduced to the smallest scenario that still fails.

## What works today

- **A cluster in one world.** Five `raft-rs` nodes driven through a one-way partition and back:
  100 runs, 16,016 events each, one digest.
- **Real programs, as released.** `redb` 2.6.3 under a crash campaign; `tokio` 1.53.1 and a TCP
  service on it, 100 runs agreeing; a 103 MB Ethereum execution client through 333,485 events of
  its node command.
- **Runs travel.** 9 of 9 bundles written by a Windows build replay on a Linux one, digest for
  digest.
- **Findings shrink.** A fixture's 49,477 events reduce to 18 in 20 replays and reproduce 100 times
  of 100.
- **Findings in other projects.** Two, in widely deployed open-source software, are with their
  maintainers and stay private until fixed.

Every figure is in [`benches/baseline.toml`](./benches/baseline.toml) with the host it was taken on,
a Core i5-12600KF. [EVIDENCE.md](./docs/EVIDENCE.md) gives the key and the command behind each.

## Goals and where they stand

| Goal | Target | Measured |
|---|---|---|
| Total repeatability | 1000 runs of one seed, one byte-identical trace | 100 runs of a five-node cluster, one digest; `make gate` runs 1000 per guest |
| Zero changes to the code under test | an existing binary, one command | the programs above, as their projects release them |
| Usable speed | no worse than 5× native | 3.6× on `redb`'s own test suite, measured outside the tree; 6.6× on the same database under the Windows subsystem for Linux; 57 µs per intercepted syscall |
| Reproduction that travels | 100 of 100 on another machine | 9 of 9, Windows build to Linux build |
| Minimal reproducers | 40,000 events to fewer than 20 | met on a fixture, 49,477 to 18; not met on a real client within 20 replays, 1,222,888 to 1,189,704 |
| Stated limits | every hole recorded with its reason | the [determinism map](./docs/DETERMINISM.md) |

## What it does not do

- **Judge what it was not told to judge.** It finds crashes, hangs, broken SDK assertions and what a
  scenario's oracle checks. A clean campaign is a statement about those and nothing else.
- **Run what it does not model.** An unmodelled syscall ends the run with a named error rather than
  reaching the host. `RDRAND` is a documented hole.
- **Run the program under test anywhere but Linux x86-64.** The core builds and tests on macOS and
  Windows; the guest needs Linux.
- **Place faults better than uniform choice.** Guided targeting loses to uniform on the defect it
  was measured against, and the claim is withdrawn ([API.md](./docs/API.md) §5.4).
- **Contain a hostile program.** It is not a sandbox — see [Security](#security).

## Why

The most expensive bugs in server software happen once in a million runs: two threads line up, a
packet arrives out of order, a node dies between the write and the acknowledgement. Ordinary tests
miss them, and a test that is sometimes red gets a retry annotation with a real bug behind it.

The teams that need deterministic simulation testing — TigerBeetle, FoundationDB, WarpStream,
SurrealDB, RisingWave, Resonate — have each written their own, because there is nothing to take off
the shelf. The one commercial product in the category is closed-source.

Bohr is an open implementation at the syscall level: no code changes, no rebuild, no hypervisor.

## Status

**v0.4.0, blockchain oracles — not published.** What it does and does not do is fixed in the
[changelog](./CHANGELOG.md). There is one public release, v1.0, and it ships after findings in real
projects are confirmed by their maintainers. Until then the tape ABI, the bundle format and the
command line all change.

## For users

Requirements: Linux x86-64, kernel 5.4 or newer, an ordinary unprivileged user.
Full matrix in [PLATFORMS.md](./docs/PLATFORMS.md).

Install from a release: the installer checks the signature on `SHA256SUMS` and every file against
it before anything runs, puts `bohr` and the target packs in `~/.bohr`, and runs `bohr doctor`.

```bash
curl -sSf https://github.com/KDaniello/bohr/releases/latest/download/bohrup.sh | sh
bohr doctor
bohr hunt redb --time 5m
```

On Ubuntu 23.10 and later, `bohr doctor` fails on `namespaces` until AppArmor lets the installed
binary make them, and prints the one `sudo` command that does ([PLATFORMS.md](./docs/PLATFORMS.md) §7).

```bash
# Hunt a target from its pack: build or check the pinned binary, run its campaign, group findings.
bohr hunt reth --time 1h -j 8 -o hunt-reth
bohr findings hunt-reth/findings

# A chain judged by its own JSON-RPC: finality, agreement, liveness, double-signing (API.md §5.9).
make probe example-chain-toy
bohr run --seed 1 --config examples/chain-toy/scenario-equivocate.toml -- examples/chain-toy/build/chain-toy-node

# Or start from your own program: a first scenario from what a survey of it finds.
bohr init --nodes 3 -- ./my-service
bohr explore --time 30m -j 8 --config scenario.toml --resume campaign -- ./my-service

# Check that this machine can run Bohr, and that the target program is supported.
bohr doctor --target ./my-service

# Run once, deterministically.
bohr run --seed 42 -- ./my-service --config prod.toml

# Prove it: 1000 runs, one seed, identical traces.
bohr run --repeat 1000 --seed 42 -- ./my-service

# Hunt for 30 minutes across 8 workers.
bohr explore --time 30m -j 8 -- ./my-service

# Reproduce, shrink and explain a finding.
bohr replay findings/finding-9c2d.bohr
bohr reduce findings/finding-9c2d.bohr -o minimal.bohr
bohr report minimal.bohr -o report.html
```

In Docker, `ptrace` and the namespaces a guest is isolated in are blocked by the default
profiles; with no command the image runs `bohr doctor`, which names what is still missing:

```bash
docker run --cap-add SYS_PTRACE --security-opt seccomp=unconfined   --security-opt apparmor=unconfined -v "$PWD:/work" ghcr.io/kdaniello/bohr:<version> <command>
```

In CI, a pull request fails on a finding, with the bundle, a report and SARIF attached:

```yaml
- uses: KDaniello/bohr/action@v1.0.0
  with: { pack: redb, time: 5m }
```

For a coding agent, `bohr mcp` serves the commands as tools over stdio; every tool answers with
the object `--json` writes ([API.md](./docs/API.md) §7.12–7.13). Every command takes `--json`.

### The optional SDK

Crashes, hangs and data corruption are found with no annotation at all. The SDK is optional, and it
tells the tool what the program *means*:

```rust
bohr::always!(log.is_monotonic(), "log index is monotonic");
bohr::sometimes!(leader_changed && disk_full, "leader change under a full disk");
bohr::fault_point("before_commit_ack");
```

Outside Bohr every call is a no-op. The output format is **compatible with the Antithesis SDKs**, so
code already annotated for Antithesis runs here unchanged.

The two halves differ: `always` finds breakage, `sometimes` finds gaps in the testing — "in ten
million runs a leader change under a full disk was never reached".

## For developers

```bash
git clone https://github.com/KDaniello/bohr && cd bohr
make test-fast    # unit, property and integration — no Linux required, under a minute
make test         # everything this host can run
make pr           # exactly what CI runs on a pull request
```

The core is sans-io: the scheduler, the virtual world, the explorer and the reducer are state
machines with no syscalls, so most of the codebase builds and tests on macOS and Windows too. Only
the syscall backend needs Linux — and there green elsewhere is not green: the Linux half is skipped,
not passed.

[AGENTS.md](./AGENTS.md) maps each area to its files and the check that covers it;
[OVERVIEW.md](./docs/OVERVIEW.md) describes the machine end to end;
[ARCHITECTURE.md](./docs/ARCHITECTURE.md) states the rules, and its twenty invariants in §5 govern
the code. [CONTRIBUTING.md](./CONTRIBUTING.md) covers the workflow.

## Getting help

Bug reports: an issue with the **seed** or the **bundle**. A bundle is what makes a report
actionable without further exchange.
[What a bundle contains](./docs/API.md#46-what-a-bundle-contains-about-the-guest--and-the-redaction-contract)
describes what travels with it before one is attached to a public issue; `bohr report --redact`
strips guest data while keeping the run structure.

Questions: GitHub Discussions. Design arguments: an issue labelled `S-needs-decision`, referencing
the section of `ARCHITECTURE.md` the disagreement is with.

## Security

Vulnerability reports: [SECURITY.md](./SECURITY.md).

> [!WARNING]
> **Bohr is not a security sandbox.** It provides reproducibility, not containment. A malicious
> program running under Bohr can escape — `ptrace` supervision is not a security boundary. Run
> untrusted code in a virtual machine, whether or not Bohr is involved.

Bohr sends nothing anywhere. No telemetry, no crash reporting, no phone-home. Everything it produces
is written to the local disk.

## Acknowledgements

This project stands on published work and on tools we learned from: `rr` for record-and-replay
engineering and the seccomp pre-filter technique · Hermit for showing both the promise and the
failure mode of syscall-level determinism · PCT (Microsoft Research, ASPLOS 2010) and AWS Shuttle for
randomised scheduling with guarantees · loom and turmoil for the in-process side of the field ·
AFL++ for mutation engineering · Jepsen for the practice of reporting findings privately first and
publishing them carefully · Antithesis for defining the category and for an SDK format this project
is compatible with.

## License

Apache-2.0. See [LICENSE-APACHE](./LICENSE-APACHE).
