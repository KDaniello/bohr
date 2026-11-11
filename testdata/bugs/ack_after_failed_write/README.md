# ack_after_failed_write

**The mistake:** a write's ENOSPC ignored, and the record acknowledged.

**Caught by:** R7.1 (`BOHR-2060`), under `scenario.toml`. A refused append leaves its key out of the store, and the acknowledgement that followed it is lost: R7.1 names the refused write and R1.1 the read.

The guest writes the `kv.*` records of `docs/API.md` §6.4 through `testdata/guests/bohr_kv.h`. A
store told no by its disk or its memory is judged by what it did with the answer, never by the
refusal itself: stopping is a correct answer.
