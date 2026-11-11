#!/usr/bin/env bash
# Builds this pack's state with the one state builder every Cosmos SDK chain shares.
#
# Run from the repository root, after build.sh; it replaces targets/cosmos-simd-env/state, and the scenario's
# [state] digest is then pinned to what it wrote.
set -euo pipefail
exec scripts/cosmos-state.sh targets/cosmos-simd-env/build/simd cosmos stake utransfer \
  targets/cosmos-simd-env/state
