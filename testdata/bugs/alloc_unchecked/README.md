# alloc_unchecked

A store that maps one page per record and writes through the result without checking it against
`MAP_FAILED`. When an allocation is refused it writes to address -1 and dies.

| Scenario | Memory | Expected |
|---|---|---|
| `scenario.toml` | each allocation refused at 1/500 | found on most seeds: a crash, `SIGSEGV` |
| `scenario-ample.toml` | never refused | clean on every seed |

The pair is the gate (G5.4): found on at least 20 of seeds 1–60 with refusals drawn, on none of
them without.
