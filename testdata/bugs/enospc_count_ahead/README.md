# enospc_count_ahead

**The mistake:** a record counted before it is written, and the store stopped on `ENOSPC` with the
count one ahead.

**Caught by:** R7.2 (`BOHR-2061`), under `scenario.toml`. The refused append leaves the count one
ahead of the log; the restart once the disk has room again refuses the log as torn and never
answers. The count is overwritten in place, so only the record's append can be refused.

The guest writes the `kv.*` records of `docs/API.md` §6.4 through `testdata/guests/bohr_kv.h`. A
store told no by its disk is judged by what it did with the answer, never by the refusal itself:
stopping is a correct answer, and staying stopped once the disk has room is not.
