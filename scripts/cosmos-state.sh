#!/usr/bin/env bash
# Builds the state of any Cosmos SDK chain for the laws of durability, from the chain's own binary:
# four validators at the addresses the world gives nodes 0 to 3, 100 accounts, two denominations,
# and transfers signed offline by the chain's own `tx sign --offline`.
#
#   scripts/cosmos-state.sh <binary> <address-prefix> <staking-denom> <transfer-denom> <out>
#
# Fees are paid in the staking denomination and transfers move the other one, which genesis creates
# and no module mints or burns: so inflation, fees and rewards never enter what a conservation law
# sums. The genesis is dated 2025-01-01, where the world's wall clock starts; blocks are committed
# every 200 ms of virtual time, the one setting changed from the release's own configuration. The
# keys are test keys of this synthetic network and nothing else.
#
# Run once on a host, from the repository root; the pack pins its output by blake3.
set -euo pipefail

bin=$1 prefix=$2 stake=$3 coin=$4 out=$5
[ -x "$bin" ] || { echo "no binary at $bin; build the pack first" >&2; exit 2; }
chain=bohr-1
validators=4
accounts=100
transfers=${TRANSFERS:-5000}
home=$(mktemp -d)
trap 'rm -rf "$home"' EXIT
rm -rf "$out"
mkdir -p "$out/net" "$out/workload"

# The genesis subcommands moved under `genesis` in cosmos-sdk 0.47; older releases have them at the
# top level.
genesis() { "$bin" genesis "$@" 2>/dev/null || "$bin" "$@"; }

for n in $(seq 0 $((validators - 1))); do
  node="$out/net/node$n"
  "$bin" init "node$n" --chain-id "$chain" --home "$node" >/dev/null 2>&1
  "$bin" keys add "validator$n" --keyring-backend test --home "$node" >/dev/null 2>&1
done
first="$out/net/node0"
for n in $(seq 0 $((validators - 1))); do
  address=$("$bin" keys show "validator$n" -a --keyring-backend test --home "$out/net/node$n")
  genesis add-genesis-account "$address" "1000000000000$stake,1000000000$coin" --home "$first"
done
for i in $(seq 0 $((accounts - 1))); do
  "$bin" keys add "account$i" --keyring-backend test --home "$home" >/dev/null 2>&1
  address=$("$bin" keys show "account$i" -a --keyring-backend test --home "$home")
  genesis add-genesis-account "$address" "1000000$stake,1000000$coin" --home "$first"
done
mkdir -p "$first/config/gentx"
for n in $(seq 0 $((validators - 1))); do
  node="$out/net/node$n"
  [ "$n" = 0 ] || cp "$first/config/genesis.json" "$node/config/genesis.json"
  genesis gentx "validator$n" "100000000$stake" --chain-id "$chain" --keyring-backend test \
    --home "$node" --ip "10.66.0.$((n + 1))" --output-document "$first/config/gentx/node$n.json" \
    >/dev/null 2>&1
done
genesis collect-gentxs --home "$first" >/dev/null 2>&1
python3 - "$first/config/genesis.json" <<'EOF'
import json, sys
path = sys.argv[1]
genesis = json.load(open(path))
genesis["genesis_time"] = "2025-01-01T00:00:00Z"
json.dump(genesis, open(path, "w"), indent=2)
EOF

peers=""
for n in $(seq 0 $((validators - 1))); do
  id=$("$bin" comet show-node-id --home "$out/net/node$n" 2>/dev/null \
    || "$bin" tendermint show-node-id --home "$out/net/node$n")
  peers="$peers${peers:+,}$id@10.66.0.$((n + 1)):26656"
done
for n in $(seq 0 $((validators - 1))); do
  node="$out/net/node$n"
  cp "$first/config/genesis.json" "$node/config/genesis.json"
  sed -i \
    -e "s/^persistent_peers = .*/persistent_peers = \"$peers\"/" \
    -e 's/^addr_book_strict = true/addr_book_strict = false/' \
    -e 's#^laddr = "tcp://127.0.0.1:26657"#laddr = "tcp://0.0.0.0:26657"#' \
    -e 's/^timeout_commit = .*/timeout_commit = "200ms"/' \
    "$node/config/config.toml"
  sed -i -e "s/^minimum-gas-prices = .*/minimum-gas-prices = \"0$stake\"/" "$node/config/app.toml"
done

# Transfers between the accounts, round the ring, each signed offline with the account's number
# and its own next sequence, and encoded as the bytes `broadcast_tx_sync` takes.
numbers=$(python3 - "$first/config/genesis.json" "$prefix" <<'EOF'
import json, sys
genesis = json.load(open(sys.argv[1]))
accounts = genesis["app_state"]["auth"]["accounts"]
print(" ".join(a["address"] for a in accounts))
EOF
)
declare -A sequence
: > "$out/workload/txs"
for t in $(seq 0 $((transfers - 1))); do
  from=$((t % accounts)) to=$(((t + 1) % accounts))
  sender=$("$bin" keys show "account$from" -a --keyring-backend test --home "$home")
  receiver=$("$bin" keys show "account$to" -a --keyring-backend test --home "$home")
  number=0
  for a in $numbers; do [ "$a" = "$sender" ] && break; number=$((number + 1)); done
  seq=${sequence[$from]:-0}
  "$bin" tx bank send "$sender" "$receiver" "1$coin" --fees "2000$stake" --chain-id "$chain" \
    --generate-only --home "$home" > "$home/unsigned.json"
  "$bin" tx sign "$home/unsigned.json" --from "account$from" --offline --account-number "$number" \
    --sequence "$seq" --chain-id "$chain" --keyring-backend test --home "$home" \
    > "$home/signed.json"
  "$bin" tx encode "$home/signed.json" >> "$out/workload/txs"
  sequence[$from]=$((seq + 1))
done
echo "built $out: $validators validators, $accounts accounts, $transfers transfers of $coin"
