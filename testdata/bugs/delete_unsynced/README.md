# delete_unsynced

**The mistake:** a delete acknowledged before its tombstone is durable.

**Caught by:** R1.2 (`BOHR-2057`), under `scenario.toml`. The kill lands between the second tombstone and its sync, so the deleted value comes back when the log is replayed.

The guest writes the `kv.*` records of `docs/API.md` §6.5 through `testdata/guests/bohr_kv.h`, and
every start reads each key back before it does anything else. The law's finding is placed at the
line of the acknowledgement that was lost, in `delete_unsynced.c`.
