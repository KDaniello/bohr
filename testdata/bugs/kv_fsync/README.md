# kv_fsync

An acknowledged write is lost after a crash, because the acknowledgement goes out before the
`fsync` that would have made the write durable.

The store keeps a synced count of acknowledged puts and an unsynced-until-later data file. On
restart it compares the two: fewer records than acknowledgements is the defect, and the program
exits 1 — a self-checking guest, nothing about the bug is known to the tool.

- **Depth:** 2 — a node kill must land between one put's `ACK` and its `fsync`.
- **Budget** (`docs/TESTING.md` §8): found in under 2 minutes of search.
- **Scenario shape:** any kill-and-restart schedule over a single node; the drawn form is what
  the explorer reaches for from M3.
