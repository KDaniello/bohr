# fsync_retry_after_eio

**The mistake:** a failed fsync retried, and the retry's success taken as durability.

**Caught by:** R7.1 (`BOHR-2060`), under `scenario.toml`. The failed `fsync` drops the record, the retry succeeds with nothing to sync, and the acknowledgement after it is lost: R7.1 names the failed `fsync` and R1.1 the read.

The guest writes the `kv.*` records of `docs/API.md` §6.4 through `testdata/guests/bohr_kv.h`. A
store told no by its disk or its memory is judged by what it did with the answer, never by the
refusal itself: stopping is a correct answer.
