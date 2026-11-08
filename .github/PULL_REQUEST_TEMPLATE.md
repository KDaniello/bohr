<!--
Title: <type>(<scope>): <short description>
  types: feat fix perf refactor docs test chore
  scope: tape kernel world ptrace backend explorer reducer trace report cli sdk ci docs

Keep the description short: what changed and why. Do not list changed files.
-->

Closes #

<!-- One to three sentences. Explain why if the diff does not. -->

### Determinism

<!-- Delete this section only if the change cannot affect a run's outcome. -->

- [ ] No new nondeterminism: no `Instant::now`, `thread_rng`, `HashMap` iteration or floating
      point in a decision path
- [ ] Effect on stored bundles: none / `BREAKING: invalidates bundles with tape ABI < N`
- [ ] An injection was added to the mutation harness if a new determinism guarantee was introduced

### Checks

- [ ] `make pr` is green locally
- [ ] Tests included; a bug fix names the seed or bundle that found it in the commit message
- [ ] `docs/` updated, if the behaviour it describes changed
