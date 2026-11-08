# Continuous integration

What runs, when, why it exists, and what to do when it is red. The job list itself is defined in
[`TESTING.md`](../TESTING.md) §15; this document is the operational side.

## Workflows

| Workflow | File | Trigger |
|---|---|---|
| Lint | `.github/workflows/lint.yml` | push, pull request |
| Test | `.github/workflows/test.yml` | push, pull request |
| Determinism | `.github/workflows/determinism.yml` | push, pull request |
| SDK | `.github/workflows/sdk.yml` | pull requests touching `sdk/` |
| Docs | `.github/workflows/docs.yml` | pull requests touching `docs/` |
| Nightly | `.github/workflows/nightly.yml` | schedule |
| Release | `.github/workflows/release.yml` | tag `v*` |

## Required checks

A pull request cannot merge without these green:

`lint` · `test-fast` (Linux, macOS, Windows) · `test-linux` · `determinism` · `mutation` · `bdd` ·
`fuzz-short` · `docs` when `docs/` changed · `sdk` when `sdk/` changed.

Nightly jobs — `portability`, `hunting`, `perf`, `fuzz-long` — do not block a merge. They block a
**release**, and a red nightly is triaged the same morning: they are the jobs that notice we have
become slower or worse at finding bugs, which no per-commit check can see.

## When a job is red

**`lint`.** `make lint` runs the same commands locally. The mechanical rules
([`ARCHITECTURE.md`](../ARCHITECTURE.md) §8 rule 14) print the rule number and the offending line —
those are architectural violations, not style nits, and the fix is to move the code, not to silence
the check.

**`determinism`.** The output names the first diverging event on both sides. The seed from the
failure line reproduces locally with `bohr run --repeat`, and it is a P1. A determinism
regression that reaches `main` invalidates stored bundles, and we will not know which ones.

**`mutation`.** A red here means an injection did **not** break the determinism suite — a hole in
the tests, not in the code. It is fixed by strengthening the test, never by removing the injection.

**`fuzz-short`.** The crashing input is uploaded as an artefact, and it joins `fuzz/corpus/` in the
fixing commit.

**`perf` (nightly).** Prints both numbers and the baseline. A regression is investigated before the
next feature; the filter ratio ([`PLATFORMS.md`](../PLATFORMS.md) §5) is the first thing to look at.

**`hunting` (nightly).** The search no longer reaches a defect it used to reach. That is a real
regression with every correctness test green, and it usually points at the scheduler, at fault
targeting, or at the mutation operators. The findings the campaigns did produce are attached to the
failed run, because the first question is always which defects it *did* still reach.

## Caching and runtime

Target directories are cached per workflow with `Swatinem/rust-cache`. Guest binaries under
`testdata/` are built in CI from committed source with pinned flags and cached by source hash —
never committed as binaries ([`TESTING.md`](../TESTING.md) §13).

Budget: the blocking set finishes in under 20 minutes. If it grows past that, work moves to nightly
rather than being deleted — a suite people stop waiting for is a suite people stop reading.

## Runner privileges

L3 and above need `ptrace` and user namespaces. GitHub-hosted Ubuntu runners provide both. Container
jobs run with `--cap-add=SYS_PTRACE --security-opt seccomp=unconfined`
([`PLATFORMS.md`](../PLATFORMS.md) §3).

Branch counters are unavailable on hosted runners, so `determinism` runs at syscall granularity
there. No runner of this repository has them, so the two granularities are not checked against each
other in CI, and `portability` replays across two hosted images rather than two CPU vendors.

## Secrets

CI holds exactly one secret: the registry token used by the release workflow. No telemetry endpoint,
no crash reporter, no analytics. Pull requests from forks never see it, and the release workflow runs
only on tags from this repository.
