# kv_ack_quiet

An acknowledged write is lost after a crash, and the store says nothing about it.

The same defect as [`kv_fsync`](../kv_fsync/), with the self-check taken out. `kv_fsync` compares
its acknowledgement count against what survived and exits 1 — so the crash oracle finds it, and
finding it proves nothing about anything else. This one serves what it has and **exits 0**, which
is what a real client does: it stays alive, keeps answering, and is simply wrong.

Only an oracle judging from the boundary can tell the two runs apart. The acknowledgement is an
outbound datagram rather than a line on standard output, because that is the form the world can
recognise — see [`API.md`](../../../docs/API.md) §5.6.

- **Depth:** 2 — a kill must land between one record's answer and the sync that would have kept it.
- **Budget** ([`TESTING.md`](../../../docs/TESTING.md) §8): found in under 2 minutes of search.
- **Scenario shape:** a two-node cluster with `[durability]`, and any kill-and-restart schedule
  over node 0.
- **Twin:** [`kv_ack_quiet_correct`](../kv_ack_quiet_correct/) — the same program with the two
  calls in the other order. The gate is the pair: found on this one on every seed, and on the twin
  on none. Without the twin, an oracle that fired on every run would pass.
