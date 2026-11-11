#!/usr/bin/env bash
# Builds the gaiad this pack names from its tag, statically, with the Go toolchain pinned here: a
# static Go build with paths trimmed is the same file wherever the same toolchain makes it.
set -euo pipefail
pack=$(cd "$(dirname "$0")" && pwd)
work=${BOHR_TARGETS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/bohr-targets}/cosmos-hub-env
export GOTOOLCHAIN=go1.23.12
mkdir -p "$work" "$pack/build"
if [ ! -d "$work/src/.git" ]; then
  git clone --filter=blob:none --no-checkout https://github.com/cosmos/gaia "$work/src"
fi
git -C "$work/src" fetch --depth 1 origin tag v25.1.0 2>/dev/null || true
git -C "$work/src" checkout --detach v25.1.0
cd "$work/src"
CGO_ENABLED=0 go build -trimpath -ldflags "-s -w" -o "$pack/build/gaiad" ./cmd/gaiad
echo "built gaiad into $pack/build/gaiad"
