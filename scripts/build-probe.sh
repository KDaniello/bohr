#!/usr/bin/env bash
# Builds the probe statically, where `bohr` looks for it: target/x86_64-unknown-linux-gnu/release.
#
# Statically, because the probe is a guest and a world holds no dynamic loader. With an explicit
# target, so the static build has a directory of its own and the workspace build's dynamic one,
# left beside a development `bohr`, is never mistaken for it.
set -euo pipefail

cd "$(dirname "$0")/.."
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release -p bohr-probe --target x86_64-unknown-linux-gnu
echo "built ${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-gnu/release/bohr-probe"
