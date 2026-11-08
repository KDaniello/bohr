# Working in this repository

Guide for automated tooling and for anyone arriving without context. It is a condensed index into
`docs/`, not a replacement for it.

---

## Rule zero: authorship

**Every action on this repository is performed as the developer. The only author is a human.**

- **Never** add a `Co-Authored-By` trailer naming an assistant, model or tool to a commit message.
- **Never** add a "Generated with …", "Created by …" or similar footer to a commit, pull request,
  issue or release note.
- **Never** mention development tooling in the git history, tags, changelog or release notes, in any
  form.

Authorship in a repository's history is the owner's decision, and this project's history reads as
the work of its team. The tool used to type the text has nothing to do with authorship.

Full statement: [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) §8.2.

---

## What this project is

Bohr runs an unmodified Linux program inside a deterministic world and makes its execution
repeatable: one seed produces one byte-identical run, forever. On top of that it searches for
schedules and faults that break the program, reproduces what it finds, and shrinks the reproduction
to a minimal scenario.

The single idea everything follows from: **every nondeterministic decision draws bytes from one
entropy tape.** Read [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) §4.1 before touching anything.

## Layers

| Layer | Crates | Rule |
|---|---|---|
| 4 · reports and debugging | `bohr-report`, `bohr-cli` | one static HTML file, no framework |
| 3 · search and reduction | `bohr-explorer`, `bohr-reducer` | sans-io; decides, never executes (§5.19) |
| 2 · determinism kernel | `bohr-kernel`, `bohr-world`, `bohr-tape`, `bohr-trace` | sans-io, no syscalls, no host clock |
| — **backend boundary** — | `bohr-backend` | trait only; must not leak ptrace vocabulary |
| 1 · determinism backend | `bohr-backend-ptrace`, `bohr-backend-null` | the only place with `unsafe` and syscalls |

`bohr-kernel` depends on the backend **trait**, never on an implementation. Wiring happens once, in
`bohr-cli`. CI enforces this.

## Where things live

Area → files → the check that covers it. Paths are under `crates/`; `T` is
`cargo nextest run -p bohr-testkit`.

| Area | Files | Check |
|---|---|---|
| Entropy tape, draws, journal | `bohr-tape/src/{tape,cursor,fixed,journal}.rs` | `cargo nextest run -p bohr-tape` |
| Run loop, scheduling, logical time | `bohr-kernel/src/{run,schedule,clock,ports}.rs` | `cargo nextest run -p bohr-kernel`; `T --test deadlines --test horizon --test ends_with_node --test break_at` |
| Syscall model of one machine | `bohr-world/src/{minimal,nums,state,cpu,ring}.rs` | `cargo nextest run -p bohr-world` |
| Network, disk, timers, readiness | `bohr-world/src/distributed/{net,disk,timers,epoll}.rs` | `T --test crash_states --test durability --test streams` |
| Guest assertions, watchers, oracle | `bohr-world/src/distributed/{sdk,watch}.rs`, `bohr-world/src/oracle.rs` | `T --test liveness --test violation --test quiet_node` |
| Scenario configuration | `bohr-world/src/config.rs` | `cargo nextest run -p bohr-world` |
| Backend boundary | `bohr-backend/src/{lib,coverage}.rs` | `T --test backend_conformance` |
| `ptrace` backend | `bohr-backend-ptrace/src/linux/{spawn,syscalls,mem,coverage,doctor}.rs` | `make test-linux` — Linux only |
| Trace, digest, bundle | `bohr-trace/src/{event,log,bundle,redact,syscalls}.rs`, `bohr-trace/assets/syscalls-x86_64.tsv` | `cargo nextest run -p bohr-trace`; `cargo nextest run -p bohr-cli --test golden_bundles` |
| Search | `bohr-explorer/src/{campaign,mutate,corpus,choices,properties}.rs` | `T --test campaign --test drawn_faults`; `make test-hunting` |
| Reduction | `bohr-reducer/src/{ddmin,reduction,scenario,bytes}.rs` | `cargo nextest run -p bohr-reducer`; `T --test reduction` |
| Report | `bohr-report/src/{html,model,describe,jsonl,text}.rs` | `cargo nextest run -p bohr-report` |
| Command line | `bohr-cli/src/{main,execute,explore,replay,reduce,doctor,survey,handover}.rs` | `cargo nextest run -p bohr-cli --test <command>` |
| SDK record format | `bohr-sdk-proto/src/{lib,json}.rs`, `sdk/` | `cargo nextest run -p bohr-sdk-proto`; `make test-sdk` |
| Determinism guards | `*/src/chaos.rs`, `scripts/mutation-harness.sh` | `make test-mutation` |
| Documentation | `docs/` | `make docs-check` |

What each claim about the project rests on, and the command that reproduces it:
[`docs/EVIDENCE.md`](./docs/EVIDENCE.md).

## Commands

```bash
make test-fast    # L0-L2, any OS, under a minute — use this in the inner loop
make test         # everything this host can run
make test-det     # the determinism gates
make lint         # fmt, clippy -D warnings, deny, typos, and the mechanical rules
make pr           # what CI runs; must be green before opening a pull request
```

Formatting is rustfmt on the dated nightly the Makefile names: `make fmt`. Tests run under `cargo nextest`.

**Green on Windows or macOS is not green.** The `ptrace` backend and every test behind
`cfg(target_os = "linux")` are skipped there and report success. A change to layer 2 or below is
verified on Linux.

## Rules that will get a change rejected

These are the ones broken most often. The full set is
[`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) §8.

1. **No nondeterminism in the tool.** No `Instant::now`, `SystemTime::now`, `thread_rng`,
   `rand::random`, or `HashMap`/`HashSet` iteration anywhere a decision or a trace can be affected.
   Use `&mut TapeCursor` for choices, the logical clock for time, `BTreeMap`/`IndexMap` for ordered
   iteration. CI greps for all of these.
2. **No `unsafe` outside `bohr-backend-ptrace`.** There, every block carries a `// SAFETY:` comment
   naming the invariant the caller must uphold.
3. **No ptrace vocabulary above the boundary.** `ptrace`, `seccomp`, `waitpid`, `libc::` must not
   appear outside `bohr-backend-ptrace`.
4. **No floating point in a decision path.** Probabilities are rationals (`"1/1000"`), distributions
   are fixed-point. `f64::ln` differs between platforms and that is a cross-machine reproduction
   failure. [`docs/API.md`](./docs/API.md) §1.
5. **An unknown syscall aborts loudly.** Never pass it to the host, never stub it with `ENOSYS`.
   [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) §5.7.
6. **Newtypes for identifiers, units in names.** `NodeId`, `TaskId`, `Tick`, `delay_ticks`.
7. **English everywhere** — code, comments, doc comments, error text, commits, issues, `docs/`.

## Comments

**As many as it takes for the code to be understandable, and no more.** A comment on every line is
harmful: it doubles the reading volume, diverges from the code at the first edit, and lies from then
on. A lying comment is worse than a missing one, because it is believed.

**A comment explains a block, an algorithm or a contract — never a line. Explain *why*, not *what*.**
If the "what" is unclear, fix the names and the structure, not the prose.

- **A doc comment** is the contract in its first line, then at most two lines more: the edge cases,
  the reference followed and why that one, or the determinism constraint. Past three lines it is a
  design argument; it moves to `docs/` and the comment keeps a § link.
- **A module comment** names what the module owns and, where a reader would assume more, what it
  does not do.
- **`///` on every public item** only in `bohr-types`, `bohr-backend`, `bohr-tape`,
  `bohr-sdk-proto`, `bohr-kernel` and the SDKs. Elsewhere, only where the signature leaves the unit,
  the bound or the meaning open.

Mandatory: a line or two on any non-trivial algorithm · invariants not expressible in the type
system · `// SAFETY:` on every `unsafe` · a workaround for a kernel or libc quirk, **with a link** ·
a deliberately non-obvious decision · **anything that exists to protect determinism**, because
otherwise it looks redundant and gets deleted in a cleanup.

Forbidden: restating the code · commented-out code · a `TODO` with no owner and no issue link ·
decorative separator bars · doc comments on obvious accessors · copying text out of `docs/` instead
of linking to it · **the history of the line** — "used to", the bug that prompted it, the test it
replaced; that is the commit message · **project meta of any kind** — milestone names, roadmap
references, schedules, "for now", "temporary", "to be added later". A comment describes the code as
it is. State a real limitation and its consequence, never when it is expected to end.

A comment is right when it stays true after the bug that prompted it is forgotten.

```rust
// Tie-break by (tick, node, seq) and never by insertion order: two packets scheduled for
// the same tick must have a total order independent of allocator addresses.
// See docs/ARCHITECTURE.md §7.2.
struct DeliveryQueue(BinaryHeap<Reverse<(Tick, NodeId, u64)>>);
```

Full rule with examples of both kinds: [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) §8.1.

## Commits

Small, and the message is about the important part.

```
<type>(<scope>): <short description>
```

Types: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `chore`.
Scope: `tape`, `kernel`, `world`, `ptrace`, `backend`, `explorer`, `reducer`, `trace`, `report`,
`cli`, `sdk`, `ci`, `docs`.

Subject: imperative, lower case, no trailing period, ≤ 72 characters. Body optional, one to three
sentences, answering *why*. Never a list of changed files. Never "comprehensive", "robust",
"enhance", "leverage".

A change to determinism-relevant behaviour states its effect on stored bundles:
`BREAKING: invalidates bundles with tape ABI < 2`.

```
fix(kernel): break packet delivery ties by (tick, node, seq)

Insertion order into the delivery heap depended on hash iteration, so two
packets scheduled for the same tick could swap under a different allocation
pattern. Digests diverged roughly once in 4000 runs.
```

## Tests

- Every bug fix carries its regression test **in the same commit**, and the message names the seed
  or bundle that found it.
- Anything in [`docs/API.md`](./docs/API.md) is written test-first — those are promises.
- Verify a new test fails for the **right reason** before making it pass. A determinism test that
  passes because it compared two empty traces is worse than no test.
- A determinism-relevant change adds an injection to the mutation harness
  ([`docs/TESTING.md`](./docs/TESTING.md) §7.4).
- **No retry annotations. Ever.** A flaky test is a P1 finding, not a nuisance.

## Documentation

`docs/` is English and the only version. A change to documented behaviour updates the document in
the same commit.

Cross-references use relative links with a section number: ``[`API.md`](./API.md) §2.3``. Renumbering
a section means fixing every reference to it in the same change.

## Where to look first

| Question | Document |
|---|---|
| why is it built this way | [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md) |
| what exactly goes over this boundary | [`docs/API.md`](./docs/API.md) |
| how do we stop *this* leaking in | [`docs/DETERMINISM.md`](./docs/DETERMINISM.md) |
| what proves it | [`docs/TESTING.md`](./docs/TESTING.md) |
| what does the host need | [`docs/PLATFORMS.md`](./docs/PLATFORMS.md) |
