# restart_refuses_new_store

**The mistake:** a store whose interrupted creation is refused forever.

**Caught by:** R1.4 (`BOHR-2055`), under `scenario.toml`. The restarted node finds a manifest without an entry point, calls the store corrupt and exits 1 without answering, so R1.4 is broken; every fault the cell arms is real.

The guest writes the `kv.*` records of `docs/API.md` §6.5 through `testdata/guests/bohr_kv.h`, and
every start reads each key back before it does anything else. The law's finding is placed at the
line of the acknowledgement that was lost, in `restart_refuses_new_store.c`.
