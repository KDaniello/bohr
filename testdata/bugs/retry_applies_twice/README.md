# retry_applies_twice

**The mistake:** a request's id recorded in a write apart from the update it guards.

**Caught by:** R4.1 (`BOHR-2052`), under `scenario.toml`. The kill lands between the second request's update and its id, and the retry after the restart applies the update again: the counter ends at 5 where four requests make 4.

The guest writes the `kv.*` records of `docs/API.md` §6.4 through `testdata/guests/bohr_kv.h`; the
ledger judges each counter from the operations begun and acknowledged on it, never from what the
store says it holds.
