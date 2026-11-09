#!/usr/bin/env bash
# Builds the CometBFT this pack names from its tag, statically, with the Go toolchain pinned here:
# the project publishes no binary for this tag, and a static Go build with paths trimmed is the same
# file wherever the same toolchain makes it.
set -euo pipefail
pack=$(cd "$(dirname "$0")" && pwd)
work=${BOHR_TARGETS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/bohr-targets}/cometbft
export GOTOOLCHAIN=go1.23.12
mkdir -p "$work" "$pack/build"
if [ ! -d "$work/src/.git" ]; then
  git clone --filter=blob:none --no-checkout https://github.com/cometbft/cometbft "$work/src"
fi
git -C "$work/src" fetch --depth 1 origin tag v0.38.26 2>/dev/null || true
git -C "$work/src" checkout --detach v0.38.26
cd "$work/src"
CGO_ENABLED=0 go build -trimpath -ldflags "-s -w" -o "$pack/build/cometbft" ./cmd/cometbft
echo "built cometbft into $pack/build/cometbft"
