#!/usr/bin/env bash
# Builds the four-validator network the scenarios here start from, with the cometbft binary of the
# `cometbft` pack.
#
# The validators sit at the addresses the world gives nodes 0 to 3 (10.66.0.1–10.66.0.4), the
# genesis is dated 2025-01-01 where the world's wall clock starts, and blocks are committed every
# 100 ms of virtual time. The keys are test keys of this synthetic network and nothing else.
#
# Run from the repository root; it replaces targets/cometbft-net/state.
set -euo pipefail

cometbft=targets/cometbft/build/cometbft
[ -x "$cometbft" ] || { echo "build the cometbft pack first: $cometbft" >&2; exit 2; }
out=targets/cometbft-net/state
rm -rf "$out"
"$cometbft" testnet --v 4 --o "$out/net" --starting-ip-address 10.66.0.1 >/dev/null

for n in 0 1 2 3; do
  config="$out/net/node$n/config/config.toml"
  sed -i \
    -e 's/^addr_book_strict = true/addr_book_strict = false/' \
    -e 's#^laddr = "tcp://127.0.0.1:26657"#laddr = "tcp://0.0.0.0:26657"#' \
    -e 's/^timeout_commit = .*/timeout_commit = "100ms"/' \
    -e 's/^timeout_propose = .*/timeout_propose = "500ms"/' \
    "$config"
  python3 - "$out/net/node$n/config/genesis.json" <<'EOF'
import json, sys
path = sys.argv[1]
genesis = json.load(open(path))
genesis["genesis_time"] = "2025-01-01T00:00:00Z"
json.dump(genesis, open(path, "w"), indent=2)
EOF
done
echo "built $out"
