# Contributing

Thanks for looking. This document is short on ceremony and specific about the things that actually
block a merge.

Before writing code, read [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md). The twenty invariants in
§5 and the code rules in §8 explain almost every line of this codebase, and a change that fights
them will be sent back regardless of how good it is.

## Getting set up

```bash
git clone https://github.com/KDaniello/bohr && cd bohr
make test-fast     # works on Linux, macOS and Windows
make test          # adds the Linux-only levels
make pr            # exactly what CI runs
```

Most of the codebase is sans-io and needs no Linux, no privileges and no guest program. Only
`bohr-backend-ptrace` and the L3+ test levels require Linux
([`docs/TESTING.md`](./docs/TESTING.md) §2).

Toolchain: pinned in `rust-toolchain.toml`, which CI reads as well. Formatting needs the dated
nightly the Makefile names (`make fmt`); everything else runs on the pinned toolchain.

## Reporting a bug

**Attach the seed or the bundle.** This is a tool whose entire purpose is reproduction; a report
without one is a report we cannot act on.

```
bohr version --verbose      # tool, tape ABI, bundle format, SDK schema — all four
bohr doctor                 # host verdict
```

Include both outputs, the command that was run, and what was expected. If the guest program is not
public, `bohr report --redact` strips its data while keeping the run structure — read
[`docs/API.md`](./docs/API.md) §4.6 for exactly what a bundle contains before attaching one to a
public issue.

An unsupported syscall (`BOHR-2001`) is a valid and welcome issue. Label it `C-syscall` and say
which program hit it — the support table grows from real targets, by design
([`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) §5.7).

## What to work on

Issues labelled `good-first-issue` name the file to start in. `help-wanted` marks work we want and
are not doing. Taxonomy in [`docs/repo/labels.md`](./docs/repo/labels.md).

Two things are always valuable and rarely claimed: a hostile guest program for `testdata/guests/`
that we currently fail on, and a syscall handler with its test.

## Pull requests

**Before opening: `make pr`.** It is the same set CI runs, so green locally means green in CI.

### Titles

[Conventional Commits](https://www.conventionalcommits.org/) with an optional scope:

```
<type>(<scope>): <short description>
```

Types: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `chore`.
Scope: the crate or area — `tape`, `kernel`, `world`, `ptrace`, `explorer`, `reducer`, `cli`, `sdk`.

```
fix(kernel): break packet delivery ties by (tick, node, seq)
perf(ptrace): filter getpid in seccomp instead of tracing it
docs(determinism): mark mmap write visibility as partial
```

### Descriptions

Short. What changed and why — nothing more.

**Do:** one to three sentences · explain *why* if the diff does not · link the issue · include
numbers for performance changes · state the effect on stored bundles if there is one.

**Don't:** list changed files (that is the diff) · repeat the title · add "Summary"/"Changes"/"Files
changed" sections · write filler like "comprehensive", "robust", "enhance", "leverage".

```
Closes #142

Insertion order into the delivery heap depended on hash iteration, so two packets
scheduled for the same tick could swap under a different allocation pattern.
Digests diverged roughly once in 4000 runs.
```

### Commits

Small, and the message is about the important part. One logical change per commit; squash checkpoint
commits before review. A commit that changes determinism-relevant behaviour says so explicitly:

```
BREAKING: invalidates bundles with tape ABI < 2
```

### Authorship

**Every commit is authored by a human.** `Co-Authored-By` trailers naming an assistant, and
"Generated with …" footers, are not accepted — see
[`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) §8.2. Whatever tooling went into writing the text
stays with its author; it has no place in `git log`.

## Tests

A pull request without tests is reviewed, but it does not merge.

- **Every bug fix carries its regression test in the same commit**, and the message names the seed
  or bundle that found it.
- Anything in [`docs/API.md`](./docs/API.md) is written test-first. Those are promises.
- A determinism-relevant change adds an injection to the mutation harness
  ([`docs/TESTING.md`](./docs/TESTING.md) §7.4), so the class cannot come back silently.
- A new test is verified to fail for the right reason before it is made to pass. A determinism test
  that passes because it compared two empty traces is worse than no test.

**There is no retry annotation in this repository.** A test that fails intermittently has found
either a real bug in Bohr or real nondeterminism in the test, and both are exactly what we sell.
Procedure in [`docs/TESTING.md`](./docs/TESTING.md) §9.

## Code style

Full rules in [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) §8. The ones that trip people up:

- **Everything is in English** — code, comments, doc comments, error messages, identifiers, commits,
  issues, documentation (§8.1).
- **Comments explain a block, an algorithm or a contract — never a line.** We comment *why*, not
  *what*. A comment on every line diverges from the code at the first edit and lies from then on.
- **A doc comment is the contract in its first line and at most two lines more.** A longer argument
  goes into `docs/` with a § link; the story of the bug that prompted a line goes into the commit
  message (§8.1).
- **No `unsafe` outside `bohr-backend-ptrace`**, and there every block carries `// SAFETY:`.
- **No `Instant::now`, no `thread_rng`, no `HashMap` iteration in a decision path.** Nondeterminism
  in the determinism tool is the one unforgivable class of bug, and CI greps for all three.
- **Newtypes for identifiers, units in field names.** `delay_ticks`, not `delay`.
- Comments that exist to protect determinism say so, or someone deletes them in a cleanup PR and we
  spend a week on it.

## Review

We look at: does it belong in this layer · is the determinism argument sound · are the tests real ·
are the commits readable. Non-blocking suggestions are prefixed `Nit:`. Incremental improvement is
enough to land a change; perfection is not required.

Design disagreement is welcome, but it goes in an issue labelled `S-needs-decision` naming the
section of `ARCHITECTURE.md` the argument is with — not in a large pull request that assumes the
argument was already won.

## Documentation

`docs/` is English and the only version. A change to documented behaviour updates the document in
the same commit, and a measured number names its source ([`docs/README.md`](./docs/README.md)).

## Code of conduct

[`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md).

## License

Contributions are licensed under Apache-2.0.
