# raft_toy

An acknowledged write lost when a leader dies before the entry reaches a majority, because the
node acknowledges on its own append rather than on the commit.

The program is `examples/raft-toy` — a three-node `raft-rs` cluster, built out of the tool's
workspace like every example that depends on a third-party library, and run under
`examples/raft-toy/scenario.toml`. It lives there rather than here because it is a Rust crate with
a consensus library behind it and the rest of this directory is single C files; what belongs here
is the description of the defect, which is this file.

**The defect.** The leader appends an entry to its own log and tells the client the write is
durable in the same breath, before a single peer has seen it. That is a real mistake and a common
one — the local append *feels* like the durable moment — and Raft's guarantee is about the majority,
never about any one log.

- **Depth:** 3 — the leader must acknowledge, then die before the entry replicates, and the term
  that follows must elect a node without it. None of the three alone loses anything.
- **Reachable only through faults.** With node faults disabled the leader keeps replicating and
  every acknowledged entry commits. The suite asserts that rather than assuming it: a defect
  reachable without faults would measure the scheduler instead of the search.
- **Self-checking.** Each node writes what it acknowledged to its own disk and `fsync`s it, so the
  promises survive the crash. On restart it requires every promise to be present in what the
  cluster committed — and only once the cluster has committed *past* that index, because a node
  that has not caught up yet has not been wronged. It exits 1 when a promise is broken. Nothing
  about the defect is known to the tool.

**What the gate measures** ([`docs/TESTING.md`](../../../docs/TESTING.md) §8): whether the search
**reaches** this defect, on a majority of master seeds, at the one fault probability used across the
whole corpus. Not how quickly — the cheapest way to make a defect appear sooner is to raise the
fault probability, which is more chaos rather than a better search.
