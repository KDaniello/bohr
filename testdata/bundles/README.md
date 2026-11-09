# Golden bundles

Stored runs that must still replay. `crates/bohr-cli/tests/golden_bundles.rs` replays every one on
every commit; a bundle that stops replaying is a determinism regression, and the failure names the
first event where the run went somewhere else.

**Adding to this collection is mandatory whenever a determinism bug is fixed** (`docs/TESTING.md`
§7.2). The bundle that caught the bug becomes the test that it stays caught, and it costs
milliseconds to check forever after.

| Bundle | What it pins |
|---|---|
| `clock-and-random-seed0.bohr` | the founding claim: a guest reading the clock and asking for randomness |
| `two-tasks-seed0.bohr` | two tasks interleaving, so the scheduler's choices are in the digest |
| `large-random-seed0.bohr` | one draw large enough to span the tape's prefix and its extension |

A bundle named `traced-*` was produced under interception and is skipped where the host cannot
trace a guest; every other bundle here comes from a guest built into the binary and replays
anywhere the tool builds.

`abi1/` keeps the same three as v0.5 recorded them, under tape ABI 1. They are not replayed: each
must be refused with both ABI versions named (`a_bundle_of_an_older_tape_abi_is_refused_by_name`),
which is what an upgrade promises a stored finding it can no longer reproduce.

Bundles are produced by `bohr selftest --save-dir`, never edited by hand: a hand-edited bundle
would fail its own section hashes, which is exactly what those hashes are for.
