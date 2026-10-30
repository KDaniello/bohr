# partition_split_brain

Two leaders under a one-directional partition. Node 0 leads and heartbeats node 1; node 1
promotes itself after three silent rounds and announces the claim. Cutting only the 0→1
direction starves node 1 of heartbeats while leaving 1→0 open, so the claim reaches a node
that never stopped leading — the observer of two leaders exits 1.

- **Depth:** 3 — the cut must be one-directional, outlast the election timeout, and the claim
  must cross the surviving direction before the heal.
- **Budget** (`docs/TESTING.md` §8): found in under 10 minutes of search.
- **Scenario shape:** `[[partition]]` with `directional = true` over the heartbeat direction;
  the drawn form is the explorer's from M3.
