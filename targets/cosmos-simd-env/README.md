# cosmos-simd-env

Simd, the reference application of cosmos-sdk: the bench. It carries the seeded defects and the clean runs because it is small and patched easily; no production network runs it, so a finding seen through it counts once it reproduces on a production chain.

| Part | Content |
|---|---|
| Program | `simd` of `https://github.com/cosmos/cosmos-sdk` at `v0.50.10`, built static by `build.sh` |
| Network | 4 validators at `10.66.0.1`–`10.66.0.4`, `timeout_commit = 200ms` |
| State | genesis on 2025-01-01; 100 accounts; transfers move `utransfer`, which no module mints or burns, and pay fees in `stake`; 5 000 transfers signed offline with the chain's own binary, by `scripts/cosmos-state.sh` |
| Workload | 20 transfers per virtual second over the four validators |
| Cells | `scenario-s0.toml`, `s1`, `s1f`, `s3`, as `cometbft-net`'s |

**Not built here yet.** Building the binary needs a Go toolchain and the chain's modules, fetched
from the network; the host this pack was written on has neither. Until it is built, the pack's
digest is a placeholder and a hunt refuses the binary by it. To build: `targets/cosmos-simd-env/build.sh`,
then `targets/cosmos-simd-env/build-state.sh`, then pin the binary's blake3 in `target.toml` and the state's
in the scenarios' `[state] digest`.

A Cosmos SDK chain is a pack, not code: this directory adds no line under `crates/` or `scripts/`.
