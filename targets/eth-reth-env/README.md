# eth-reth-env

reth, an Ethereum execution client, driven as `eth-geth-env` drives geth, and the cell where geth
and reth are two replicas of one chain.

| Node | S0, S1 | S9 |
|---|---|---|
| 0 | reth of the `reth` pack, `--datadir /reth` | geth, which builds every block |
| 1 | the engine driver, `chain --leader 0` | reth, which follows |
| 2 | `bohr-load` | the engine driver, `chain --leader 0 --follower 1` |
| 3 | | `bohr-load` |

## The state

`build-state.sh` builds geth's datadir as `eth-geth-env` does and leaves reth's empty: reth writes
its genesis on its first start inside the world, where a database made on the host is refused at
open. The genesis, the accounts and the transfers are the same.

## Cells

| Cell | Faults |
|---|---|
| S0 | none |
| S1 | reth killed at drawn moments between 3 s and 20 s, restarted after 1 s |
| S9 | none; the window closes at 20 s with a 1 ms freeze of the driver, so R3.2 asks whether reth reached every block decided before it |
| S9-split | S9, with reth under another chain id: a seeded split |

Two clients behind one engine API do not each compute a state and compare it. The follower is
handed the builder's block, state root included, and answers `INVALID` where it computes another.
A split is therefore a follower that stops: R3.2 catches it, at the first block it refused.

## What it does not do

- **Say which client is right.** A split says the two disagree; the specification's test suites
  say which one is wrong.
- **Run a real consensus client**, or mainnet's state, as in `eth-geth-env`.
