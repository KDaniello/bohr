# eth-geth-env

geth, an Ethereum execution client, run as a network runs it: driven by a consensus client, here
`examples/engine-driver chain`, which builds a block every 2 s from geth's own pool through the
engine API. `bohr-load` sends the workload through the `eth-jsonrpc` adapter, and the laws judge
what geth reports, unchanged from the Cosmos chains.

| Node | Program |
|---|---|
| 0 | geth of the `geth` pack, `--datadir /geth` |
| 1 | the engine driver, `chain --leader 0` |
| 2 | `bohr-load`, which the world adds |

## The state

`build-state.sh` builds it with the `geth` pack's binary: one genesis, Paris from block 0 and no
later fork, so the engine API's first version drives it, dated 2025-01-01 where the world's clock
starts; 2 000 accounts whose keys are a function of their index; 2 000 transfers, one from each,
signed offline by `scripts/eth-txs.py`. The state is a database, so it is built, never committed.

## Cells

| Cell | Faults |
|---|---|
| S0 | none |
| S1 | geth killed at drawn moments between 3 s and 20 s, restarted after 1 s |

## What it does not do

- **Run a real consensus client.** The driver stands in for one; a defect between a consensus
  client and an execution client is not reached.
- **Hold geth to mainnet's state.** The chain is synthetic, and what only a state of mainnet's size
  reaches is not tested.
