# Bohr contributor documentation

This directory is the technical source of truth for the project. It is written for the people who
build Bohr and for anyone who wants to understand how it works before trusting it with their code.
These documents answer only "how is it built, and why exactly this way".

## Read in this order

| Document | What it answers |
|---|---|
| [`OVERVIEW.md`](./OVERVIEW.md) | **start here** — what the machine is, what sits at its centre, how it is driven, and which parts are reusable. Plain language, one sitting |
| [`ARCHITECTURE.md`](./ARCHITECTURE.md) | components, boundaries, the twenty invariants everything else follows from, and the code rules |
| [`API.md`](./API.md) | every contract that crosses a boundary: the tape ABI, the backend trait, the bundle format, the SDK schema, the CLI, error codes |
| [`DETERMINISM.md`](./DETERMINISM.md) | the complete map of nondeterminism sources and what we do about each one. The project's checklist |
| [`TESTING.md`](./TESTING.md) | the test pyramid, TDD and BDD practice, the determinism gates, and how every acceptance criterion maps to a test |
| [`PLATFORMS.md`](./PLATFORMS.md) | host requirements, kernel features, the guest support matrix, performance expectations |

Repository process:

| Document | What it answers |
|---|---|
| [`repo/ci.md`](./repo/ci.md) | what CI runs, when, and what to do when a job is red |
| [`repo/labels.md`](./repo/labels.md) | the issue and pull request label taxonomy |
| [`repo/release.md`](./repo/release.md) | the release checklist and versioning rules |

Root of the repository: [`../README.md`](../README.md) ·
[`../CONTRIBUTING.md`](../CONTRIBUTING.md) · [`../SECURITY.md`](../SECURITY.md).

## Language

**English is canonical.** [`ru/`](./ru/) holds an identical Russian mirror with the same heading
structure; a CI job compares the heading trees and fails when they diverge, because a translation
that has silently fallen behind is worse than no translation — a reader trusts it.

When a document changes, both versions change in the same commit.

## Conventions

- Cross-references are relative links with a section number: ``[`API.md`](./API.md) §2.3``. Section
  numbers are stable; renumbering a section means fixing every reference to it in the same change.
- **Declarative, never procedural.** Documentation states what the system is and why it is that
  way. It does not narrate how it got there: no "an earlier draft did X", no "this fixes Y", no
  "we found Z", no milestone names, no schedules. A reason is a property of the design and belongs
  here; a chronology is a property of the git history and belongs there.
- Tables carry decisions; prose carries reasons. If a paragraph does not explain *why*, it is
  probably a table.
- Every claim about performance, effectiveness or compatibility is either measured or marked as a
  target. "Fast" and "robust" are not statements.
- Code comments follow [`ARCHITECTURE.md`](./ARCHITECTURE.md) §8.1 — English, per block, explaining
  why. Documentation is not an excuse to comment less, and comments are not an excuse to document
  less.
