#!/usr/bin/env bash
# Every redb scenario in turn, over the same span of seeds.
#
# One script rather than a loop typed at a prompt: a scenario name that arrives empty runs nothing
# and reports nothing, and a campaign that measured nothing looks exactly like a campaign that
# found nothing.
set -uo pipefail

cd "$(dirname "$0")/.."
from=${1:-1}
to=${2:-200}

for scenario in hunt hunt-disk hunt-prepared; do
  config="examples/redb-hunt/$scenario.toml"
  [ -f "$config" ] || { echo "no scenario at $config" >&2; exit 2; }
  printf '=== %s ===\n' "$scenario"
  ./scripts/sweep.sh "$config" "$from" "$to"
  printf '\n'
done
