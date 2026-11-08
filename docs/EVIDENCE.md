# Evidence

What Bohr claims about itself, the command that checks each claim, and what none of it proves.
Every number on this page lives in [`benches/baseline.toml`](../benches/baseline.toml) under the key
given beside it, with the host it was taken on. A number that is not there is not claimed.

## How a change is checked

| Check | What it shows | Command |
|---|---|---|
| Determinism gate | one seed run 1000 times gives one digest per guest; a five-node `raft-rs` cluster run 100 times gives one digest | `make gate` |
| Mutation harness | each of the 13 injections in `scripts/mutation-harness.sh` breaks determinism at one named site, and the determinism suite must go red for every one | `make test-mutation` |
| Portability | a bundle written on one machine replays on another, digest for digest | `make test-portability` |
| Product contract | the 12 Gherkin scenarios under `tests/features/` hold | `make bdd` |
| Fuzzing | the 5 readers of untrusted input (bundle, tape, event log, SDK record, syscall arguments) survive arbitrary bytes | `make fuzz T=bundle S=300` |
| SDK conformance | every language's SDK writes the same records | `make test-sdk` |
| Architecture rules | the mechanical rules of [`ARCHITECTURE.md`](./ARCHITECTURE.md) §8 rule 14 hold | `make lint-rules` |
| Search | the scheduler still reaches the seeded defects | `make test-hunting`, `make policy-comparison` |
| Speed | slowdown against the same guest run natively on the same machine | `make bench-slowdown` |

`make pr` runs the per-commit set. Which jobs block a merge and which run nightly is in
[`repo/ci.md`](./repo/ci.md); the tests behind each row are in [`TESTING.md`](./TESTING.md).

The mutation harness is the row the others depend on. A determinism test that cannot fail proves
nothing, and the harness is what shows each one can.

## What the last measurements say

| Claim | Measured | Key |
|---|---|---|
| A run repeats | 100 runs of a five-node `raft-rs` cluster through a partition, 16,016 events, one digest | `[cluster.split_brain]` |
| A run travels | 9 of 9 bundles written by a Windows build replayed by a Linux one | `[m2.gates]` |
| A finding shrinks | a fixture's 49,477 events reduced to 18 in 20 replays, reproducing 100 times of 100 | `[m4.reduction]` |
| PCT earns its default | a depth-2 race reached on 60 of 60 seeds under PCT, 47 of 60 under uniform choice | `[policies.race_init]` |
| Interception cost | 53.2 µs per intercepted syscall, against a ceiling of 120 | `[v0_1.gates]` |
| The suite can fail | 13 of 13 injections turn the determinism suite red; 12 of 12 product scenarios pass | `[v0_1.gates]` |
| Campaign throughput | 131,000 runs per hour against `redb` 2.6.3 | `[m4_1.slowdown]` |

The host for every row is named in its section: a Core i5-12600KF, natively or under the Windows
subsystem for Linux.

## Negative results

Recorded as measured, with the same weight as the rows above.

- **Guided fault targeting loses to uniform.** It was specified to find an order of magnitude more;
  on a depth-2 durability defect it finds less at every probability, because its candidate moments
  are a subset of uniform's (`[m3.targeting]`). [`API.md`](./API.md) §5.4 withdraws the claim.
- **Reduction does not reach real-client scale within a small budget.** Against a real execution
  client, 20 replays at 75 s each took 1,222,888 events to 1,189,704 (`[m5.op_reth.reduction]`). The
  fixture's ratio does not transfer.
- **A guest that lives a millisecond is measured by its startup.** Starting under the supervisor
  costs about 3 ms on the host of `[v0_1.gates]`, so `cat` runs at 4.9× native against a ceiling of
  4.0×, and `make gate` is red there on that row alone.
- **The ≤ 5× speed goal rests on one row measured outside the tree.** `redb`'s own test suite ran at
  3.6× native (`[guests.redb]`, `measured_out_of_band = true`); the same database under the Windows
  subsystem measured 6.6× (`[m4_1.slowdown]`).

## Findings in other projects

A finding is a bundle that replays to the failure on any supported machine, and a report beside it.
It goes to the project's maintainers first and is published only after a fix or with their
agreement — the practice Jepsen set for reporting findings in other people's systems.

## What this does not prove

- **That the guest is correct.** Bohr judges the properties it is given — exit status, crash, hang,
  SDK assertions, the world's oracle — and nothing else. A clean campaign is a statement about those.
- **Determinism outside the modelled surface.** An unmodelled syscall ends the run
  ([`ARCHITECTURE.md`](./ARCHITECTURE.md) §5.7), and `RDRAND` is a documented hole (§5.8). Every
  known source is in [`DETERMINISM.md`](./DETERMINISM.md).
- **Containment.** Bohr is not a security sandbox; `ptrace` supervision is not a boundary.
- **Speed on another machine.** The ratios move with the host, which is why each row names it.

## Check it yourself

```bash
make gate                                         # every gate, measured on this host
bohr run --repeat 1000 --seed 42 -- ./my-service  # one seed, one digest
bohr replay finding.bohr                          # on a second machine: the same digest
make test-mutation                                # every injection turns the suite red
```

A claim this page makes and a command above fails to reproduce is a bug in Bohr, and an issue with
the command's output is the whole report.
