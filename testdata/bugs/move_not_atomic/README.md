# move_not_atomic

**The mistake:** a move between two counters written as a debit and a credit apart.

**Caught by:** R5.1 (`BOHR-2053`), under `scenario.toml`. The kill lands between the second move's debit and its credit: the counters sum to 90 where they began at 100.

The guest writes the `kv.*` records of `docs/API.md` §6.4 through `testdata/guests/bohr_kv.h`; the
ledger judges each counter from the operations begun and acknowledged on it, never from what the
store says it holds.
