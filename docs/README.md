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
| [`EVIDENCE.md`](./EVIDENCE.md) | what the project claims about itself, the command that checks each claim, and what none of it proves |
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

**English, one version.** A translation kept beside the original falls behind it, and a reader
trusts the translation — so there is none. A reader who wants another language translates the
current text, not a copy of an older one.

## Conventions

- Cross-references are relative links with a section number: ``[`API.md`](./API.md) §2.3``. Section
  numbers are stable; renumbering a section means fixing every reference to it in the same change.
- **Declarative, never procedural.** Documentation states what the system is and why it is that
  way. It does not narrate how it got there: no "an earlier draft did X", no "this fixes Y", no
  "we found Z", no milestone names, no schedules. A reason is a property of the design and belongs
  here; a chronology is a property of the git history and belongs there.
- Tables carry decisions; prose carries reasons. If a paragraph does not explain *why*, it is
  probably a table.
- **A number carries its source.** Every claim about performance, effectiveness or compatibility is
  either measured or marked as a target. A measured number sits inside the sentence it supports,
  names the host it was taken on, and points at the [`benches/baseline.toml`](../benches/baseline.toml)
  key that holds it and the command that reproduces it. "Fast" and "robust" are not statements.
- **What and how to check come before why.** A page opens with what the thing does and the command
  that shows it; the reasoning follows. Short sentences, in the reader's words rather than ours.
- **Every mechanism states what it does not do**, in a paragraph of its own rather than a
  parenthesis. A limit a reader discovers on their own costs more trust than the limit itself.
- Code comments follow [`ARCHITECTURE.md`](./ARCHITECTURE.md) §8.1 — English, per block, explaining
  why. Documentation is not an excuse to comment less, and comments are not an excuse to document
  less.
