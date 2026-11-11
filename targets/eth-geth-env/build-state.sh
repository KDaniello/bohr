#!/usr/bin/env bash
# Builds the Ethereum chain the eth-geth-env scenarios start from, with the binaries of the the `geth` pack.
#
# One genesis, Paris from block 0 and no later fork, so the engine API's first version drives it;
# 2 000 accounts with keys a function of their index; 2 000 transfers among them signed offline by
# scripts/eth-txs.py, one base64 line each, for bohr-load. Each account sends once, so a submission
# a node never took leaves no gap in another's nonces: a client that starts late loses only the
# transfers sent before it listened. The shared secret is the engine
# driver's. The keys are of this synthetic chain and nothing else.
#
# Run from the repository root; it replaces targets/eth-geth-env/state.
set -euo pipefail

out=targets/eth-geth-env/state
rm -rf "$out"
mkdir -p "$out/workload"
python3 scripts/eth-txs.py check >/dev/null
python3 scripts/eth-txs.py genesis 1337 2000 > "$out/genesis.json"
cp examples/engine-driver/state/jwt.hex "$out/jwt.hex"
geth=targets/geth/build/geth
[ -x "$geth" ] || { echo "build the geth pack first: $geth" >&2; exit 2; }
"$geth" init --datadir "$out/geth" "$out/genesis.json" >/dev/null 2>&1
python3 scripts/eth-txs.py txs 1337 2000 2000 2000000000 > "$out/workload/txs"
echo "built $out"
