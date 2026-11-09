#!/usr/bin/env bash
# Fetches the geth this pack names as its maintainers publish it: the release archive, checked
# against the sha256 pinned here, unpacked into build/. geth's release binary is static, so the
# file is the same on every host and the pack's blake3 holds anywhere.
set -euo pipefail
pack=$(cd "$(dirname "$0")" && pwd)
work=${BOHR_TARGETS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/bohr-targets}/geth
archive=geth-linux-amd64-1.17.10-1df30b18.tar.gz
sha256=e016aca8cc391c8d5d14dc17c9af589488d99c37ed71d892213c80757935ba64
mkdir -p "$work" "$pack/build"
if [ ! -f "$work/$archive" ]; then
  curl -fL --retry 3 -o "$work/$archive.part" "https://gethstore.blob.core.windows.net/builds/$archive"
  mv "$work/$archive.part" "$work/$archive"
fi
echo "$sha256  $work/$archive" | sha256sum -c --quiet -
tar -xzf "$work/$archive" -C "$work"
cp "$work/${archive%.tar.gz}/geth" "$pack/build/geth"
echo "fetched geth into $pack/build/geth"
