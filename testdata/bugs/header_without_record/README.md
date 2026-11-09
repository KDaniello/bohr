# header_without_record

A store that appends a record, rewrites the header that points at it, and syncs once after both.
On start it follows the header to the last record and exits 1 when the record is not there.

| Scenario | Crash | Expected |
|---|---|---|
| `scenario.toml` | may keep some unsynced writes and lose others | found on some seeds: the header survived, the record did not |
| `scenario-in-order.toml` | `reorder_unsynced = false`: all unsynced writes or none | clean on every seed |

The pair is the gate (G6.3): found on at least 20 of seeds 1–60 with reordering, on none of them
without. The correct store syncs the record before it writes the header.
