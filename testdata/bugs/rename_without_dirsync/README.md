# rename_without_dirsync

**The mistake:** a value renamed into place and acknowledged without syncing its directory.

**Caught by:** R1.1 (`BOHR-2051`), under `scenario.toml`. The temporary file is synced before the rename, and the namespace is synced whole by the next file sync, so only the last rename before the kill is lost: the key read back absent after the restart.

The guest writes the `kv.*` records of `docs/API.md` §6.5 through `testdata/guests/bohr_kv.h`, and
every start reads each key back before it does anything else. The law's finding is placed at the
line of the acknowledgement that was lost, in `rename_without_dirsync.c`.
