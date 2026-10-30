# Changelog

Written for someone deciding whether to upgrade. Format and section order in
[`docs/repo/release.md`](./docs/repo/release.md).

Four numbers version independently and are recorded in every bundle: `tool_version`,
`tape_abi`, `bundle_format`, `sdk_schema` ([`docs/API.md`](./docs/API.md) §9).

## Unreleased

Pre-alpha. Nothing is released yet. The first public version is v1.0, and it follows the
hunting milestone: the release carries confirmed findings, not only a tool.

### Added

- Deterministic execution of an unmodified Linux program: the entropy tape, logical time, the
  `ptrace` backend and a scheduler whose every choice is drawn rather than observed. The same seed
  produces the same trace digest, and the digest is what "the same run" means.
- Deterministic multithreading, with PCT scheduling and the probabilistic guarantee it carries.
- A cluster in one world: per-node clocks, filesystems and futex spaces, a virtual network that
  drops, duplicates, reorders, delays and partitions, and a virtual disk with strict `fsync`
  semantics, torn writes and crash snapshots.
- Node lifecycle as a fault: kill, freeze, thaw, restart, and restart from an older synced image.
- The `.bohr` bundle and `bohr replay`: a run leaves the machine that found it and reproduces
  elsewhere, or is refused with the reason named.
- SDKs for Rust and Python, checked against one conformance suite, and the record channel the
  supervisor reads them on.
- `bohr doctor`: what this host can do, what it cannot, and what to type about it.
- Campaign search: a corpus, coverage-guided mutation of the tape and fault schedules drawn per
  run. A campaign is a pure function of its master seed, and one degree of parallelism finds what
  another does.
- Property evaluation across a campaign: `always` judged per run, `sometimes` and `reachable` per
  campaign, and the properties nothing satisfied reported as a result rather than as silence.
- `bohr reduce`: delta debugging over the configuration, the events delivered and the tape, under a
  replay budget, leaving a bundle that still reproduces the same violation.
- The HTML report and `bohr report`: one static file naming the point of violation, a timeline per
  node, the faults that fired and the draws that mattered. `--redact` rewrites the bundle rather
  than the report, so a redacted bundle still replays.
- `bohr trace`, the event log as text or JSON Lines, and `bohr note`, a line of context kept with
  the bundle.
- `bohr replay --break-at` and `--gdb`: a reproduced run stopped at a named event and released to an
  ordinary debugger, with what that debugger will not be able to do printed beside where to attach.
- SDKs for C# and Go, checked by the same conformance suite as the others.
- Judging a guest that never says anything about itself: a run horizon, so a server that does not
  exit can be judged at all; a cluster that healed and went on committing nothing; and an answer a
  node gave before it was durable and came back without.
- `bohr survey`: every syscall a program asks for that this build does not model, in one pass
  instead of one per run.
- A prepared filesystem as a run input, recorded in the bundle by digest and checked before a
  replay; and a reorg, which rewinds a cluster to a common ancestor and lets history run again.
- Differential testing: one tape, several implementations of one specification, and a finding when
  what they produce differs — which needs no assertion, no annotation and no exit status.
- Project documentation: architecture, API contracts, determinism source map, testing strategy,
  platform requirements and roadmap.
- Repository infrastructure: lint rules, CI workflows, issue and pull request templates.
