# batch_in_two_writes

**The mistake:** a batch written as two durable records with no commit marker.

**Caught by:** R1.3 (`BOHR-2058`), under `scenario.toml`. The first record is durable and the second was never written: the batch is read back in part, which R1.3 refuses whether or not it was acknowledged.

The guest writes the `kv.*` records of `docs/API.md` §6.5 through `testdata/guests/bohr_kv.h`, and
every start reads each key back before it does anything else. The law's finding is placed at the
line of the acknowledgement that was lost, in `batch_in_two_writes.c`.
