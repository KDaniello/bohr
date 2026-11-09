# no_checksum

A datagram protocol with no checksum: node 1 sends numbered updates, node 0 applies whatever
arrives. An update altered on the wire is applied, and node 0 exits 1 naming it.

| Scenario | Wire | Expected |
|---|---|---|
| `scenario.toml` | a datagram altered at 1/200 | found on most seeds: a crash, exit status 1 |
| `scenario-intact.toml` | nothing altered | clean on every seed |

The pair is the gate (G6.2): found on at least 20 of seeds 1–60 with corruption, on none of them
without.
