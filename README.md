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
[Русская документация](./docs/ru/)

</div>

---

## What is Bohr

Bohr runs an unmodified Linux program inside a deterministic world. Time, randomness, thread
scheduling, the network and the disk all come from us, and every nondeterministic decision is drawn
from a single string of bytes — the **entropy tape**. One 64-bit seed therefore defines an entire
execution.

On top of that sits controlled hostility — network delays and partitions, process kills, torn
writes, stalled disks, reordered threads — and a search over the schedules that break the program
under test. A finding is a file that reproduces on another machine with a different CPU, and it is
reduced to the smallest scenario that still fails.

## Why

The most expensive bugs in server software happen once in a million runs: two threads line up, a
packet arrives out of order, a node dies exactly between the write and the acknowledgement. Ordinary
tests do not catch them. A test that is sometimes red gets marked flaky, and a real bug is now
behind a retry annotation.

Deterministic simulation testing addresses this class, and the teams that need it — TigerBeetle,
FoundationDB, WarpStream, SurrealDB, RisingWave, Resonate — have each written their own, because
there is nothing to take off the shelf. The one commercial product in the category is closed-source.

Bohr is an open implementation at the syscall level: no code changes, no rebuild, no hypervisor.

## Goals

1. **Total repeatability.** 1000 runs of one seed produce a byte-identical event trace.
2. **Zero changes to the code under test.** An existing binary, one command, no rebuild.
3. **Usable speed.** No worse than 5× native on a typical workload.
4. **Reproduction that travels.** A scenario file replays on a different machine with a different
   CPU, 100 out of 100.
5. **Minimal reproducers.** 40 000 events reduced automatically to fewer than 20.
6. **Stated limits.** Where determinism cannot be guaranteed, the run records it and names the
   reason. See the [determinism map](./docs/DETERMINISM.md).

## Status

**Pre-alpha. Not yet released.** A five-node cluster runs in one deterministic world, through a
network that drops and reorders and a disk that tears, and a run saved on one machine replays on
another. There is exactly one public release — v1.0 — and it ships only after the hunting phase has
produced findings confirmed by the maintainers of real projects. Nothing here is stable yet: the
tape ABI, the bundle format and the command line will all move before v1.0.

Progress is measured by gates rather than by commit count. Every number this repository states about
itself is measured, and the measurements live in [`benches/baseline.toml`](./benches/baseline.toml).

## For users

Requirements: Linux x86-64, kernel 5.4 or newer, an ordinary unprivileged user.
Full matrix in [PLATFORMS.md](./docs/PLATFORMS.md).

```bash
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

In Docker, `ptrace` is blocked by the default seccomp profile:

```bash
docker run --cap-add=SYS_PTRACE --security-opt seccomp=unconfined ghcr.io/kdaniello/bohr ...
```

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
the syscall backend needs Linux.

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
