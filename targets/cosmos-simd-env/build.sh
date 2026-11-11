#!/usr/bin/env bash
# Builds the simd this pack names from its tag, statically, with the Go toolchain pinned here: a
# static Go build with paths trimmed is the same file wherever the same toolchain makes it.
set -euo pipefail
pack=$(cd "$(dirname "$0")" && pwd)
work=${BOHR_TARGETS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/bohr-targets}/cosmos-simd-env
export GOTOOLCHAIN=go1.23.12
mkdir -p "$work" "$pack/build"
if [ ! -d "$work/src/.git" ]; then
  git clone --filter=blob:none --no-checkout https://github.com/cosmos/cosmos-sdk "$work/src"
fi
git -C "$work/src" fetch --depth 1 origin tag v0.50.10 2>/dev/null || true
git -C "$work/src" checkout --detach v0.50.10
cd "$work/src"
CGO_ENABLED=0 go build -trimpath -ldflags "-s -w" -o "$pack/build/simd" ./simapp/simd
echo "built simd into $pack/build/simd"
