# cosmos-hub-env

The Cosmos Hub's gaiad: the first production target. gaiad is in the Cosmos bug bounty's scope and runs a production network. The release's own configuration is kept with one change, timeout_commit = 200ms.

| Part | Content |
|---|---|
| Program | `gaiad` of `https://github.com/cosmos/gaia` at `v25.1.0`, built static by `build.sh` |
| Network | 4 validators at `10.66.0.1`–`10.66.0.4`, `timeout_commit = 200ms` |
| State | genesis on 2025-01-01; 100 accounts; transfers move `utransfer`, which no module mints or burns, and pay fees in `uatom`; 5 000 transfers signed offline with the chain's own binary, by `scripts/cosmos-state.sh` |
| Workload | 20 transfers per virtual second over the four validators |
| Cells | `scenario-s0.toml`, `s1`, `s1f`, `s3`, as `cometbft-net`'s |

**Not built here yet.** Building the binary needs a Go toolchain and the chain's modules, fetched
from the network; the host this pack was written on has neither. Until it is built, the pack's
digest is a placeholder and a hunt refuses the binary by it. To build: `targets/cosmos-hub-env/build.sh`,
then `targets/cosmos-hub-env/build-state.sh`, then pin the binary's blake3 in `target.toml` and the state's
in the scenarios' `[state] digest`.

A Cosmos SDK chain is a pack, not code: this directory adds no line under `crates/` or `scripts/`.
