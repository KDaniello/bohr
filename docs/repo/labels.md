# Labels

One taxonomy for issues and pull requests. Every issue carries at least one `A-` and one `C-` label;
`P-` is set at triage.

## `A-` area

| Label | Scope |
|---|---|
| `A-tape` | entropy tape, draw ABI, journal |
| `A-backend` | the backend boundary and its implementations |
| `A-ptrace` | the syscall backend specifically: seccomp, namespaces, `prctl` |
| `A-kernel` | logical clock, scheduler, run loop |
| `A-world` | virtual network, disk, node lifecycle |
| `A-explorer` | mutation, corpus, coverage, fault targeting |
| `A-reducer` | delta debugging, minimisation |
| `A-trace` | event log, bundle format, digest |
| `A-report` | text, JSON and HTML output |
| `A-sdk` | any of the five SDKs |
| `A-cli` | commands, flags, exit codes |
| `A-docs` | anything in `docs/`, including the Russian mirror |
| `A-ci` | workflows, runners, caching |

## `C-` category

| Label | Meaning |
|---|---|
| `C-bug` | something is wrong |
| `C-determinism` | **a divergence or a nondeterminism source.** Always also `P-critical` |
| `C-feature` | new capability |
| `C-perf` | speed or memory |
| `C-refactor` | no behaviour change |
| `C-docs` | documentation |
| `C-test` | test coverage or test infrastructure |
| `C-syscall` | an unsupported syscall a real target needs |

`C-determinism` is separated from `C-bug` deliberately: it is the class that invalidates stored
bundles and undermines the product's premise, and it must be filterable in one click.

## `P-` priority

| Label | Meaning | Response |
|---|---|---|
| `P-critical` | determinism broken, data loss in a bundle, or a false "reproducible" claim | drop everything |
| `P-high` | blocks a milestone gate | this week |
| `P-medium` | ordinary work | scheduled |
| `P-low` | nice to have | when it surfaces |

## `S-` status

`S-needs-triage` · `S-needs-repro` (asking for a seed or a bundle) · `S-blocked` (with the blocker
linked) · `S-needs-decision` (an open question from
[`ARCHITECTURE.md`](../ARCHITECTURE.md) §14).

## Contributor-facing

`good-first-issue` — small, self-contained, with the file named in the issue body. ·
`help-wanted` — wanted, and nobody is working on it. · `E-easy` / `E-medium` / `E-hard` —
estimated difficulty; an `E-easy` that takes a week misdirects the contributor who picked it.

## Milestones

Issues are assigned to `M0`–`M6` from the roadmap. An issue in no milestone is
either backlog or should be closed; a backlog that pretends to be a plan is how scope grows.
