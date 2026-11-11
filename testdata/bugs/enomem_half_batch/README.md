# enomem_half_batch

**The mistake:** a batch applied key by key, each into a buffer allocated as it goes.

**Caught by:** R1.3 (`BOHR-2058`), under `scenario.toml`. An allocation refused for a batch's second key leaves its first one applied: the batch is read back in part.

The guest writes the `kv.*` records of `docs/API.md` §6.4 through `testdata/guests/bohr_kv.h`. A
store told no by its disk or its memory is judged by what it did with the answer, never by the
refusal itself: stopping is a correct answer.
