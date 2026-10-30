#!/usr/bin/env bash
# Sweeps seeds against one scenario and reports what the campaign actually checked.
#
# The totals matter as much as the findings. A sweep that reports "200 clean" without saying how
# many acknowledgements it verified is a sweep that cannot tell a store that kept its promises from
# a store that was never asked to.
set -uo pipefail

cd "$(dirname "$0")/.."
BOHR=${BOHR:-$HOME/bohr-target/release/bohr}
GUEST=${GUEST:-examples/redb-hunt/build/redb-hunt}
config=${1:?usage: sweep.sh <scenario> <from> <to>}
from=${2:-1}
to=${3:-100}

clean=0
findings=0
other=0
acknowledged=0
verified=0
refused=0
resurrected=0
undropped=0
prepared_lost=0
crashes=0
crashed_runs=0
tears=0
bundle=$(mktemp -t bohr-sweep-XXXXXX.bohr)
trap 'rm -f "$bundle"' EXIT

for seed in $(seq "$from" "$to"); do
  out=$(timeout 300 "$BOHR" run --seed "$seed" --config "$config" --save "$bundle" -- "$GUEST" 2>/dev/null)
  code=$?
  line=$(printf '%s\n' "$out" | grep -m1 '^acknowledged ' || true)
  if [ -n "$line" ]; then
    # By name rather than by position: the client prints its counts labelled precisely so that a
    # reader never has to know how many words a list of keys took.
    count() { printf '%s' "$line" | grep -oE "$1 [0-9]+" | head -1 | awk '{print $2}'; }
    acknowledged=$((acknowledged + $(count acknowledged)))
    verified=$((verified + $(count verified)))
    refused=$((refused + $(count refused)))
    resurrected=$((resurrected + $(count resurrected)))
    undropped=$((undropped + $(count undropped)))
    prepared_lost=$((prepared_lost + $(count prepared_lost)))
  fi

  # How many crashes this run landed. A campaign that never killed anything is a campaign that
  # asked nothing, and nothing else in a run says whether it did.
  timeline=$("$BOHR" trace "$bundle" 2>/dev/null)
  killed=$(printf '%s\n' "$timeline" | grep -c 'a node was killed' || true)
  crashes=$((crashes + killed))
  [ "$killed" -gt 0 ] && crashed_runs=$((crashed_runs + 1))

  # A crash that interrupted a sync and shortened what survived. Counted because a campaign that
  # cannot say whether it tore anything cannot claim to have tested torn writes.
  tears=$((tears + $(printf '%s\n' "$timeline" | grep -c 'torn by a crash' || true)))

  case $code in
    0) clean=$((clean + 1)) ;;
    1)
      findings=$((findings + 1))
      printf '=== FINDING at seed %s ===\n%s\n' "$seed" "$line"
      ;;
    *)
      other=$((other + 1))
      printf '=== seed %s exited %s ===\n%s\n' "$seed" "$code" "$line"
      ;;
  esac
done

printf 'swept %s..%s: %s clean, %s findings, %s other\n' "$from" "$to" "$clean" "$findings" "$other"
printf '  %s acknowledgements verified of %s\n' "$verified" "$acknowledged"
printf '  %s refusals asked about, %s present anyway\n' "$refused" "$resurrected"
printf '  %s removals that came back, %s records lost that were durable before the run\n' \
  "$undropped" "$prepared_lost"
printf '  %s crashes across %s runs of %s, %s of them tore a write\n' \
  "$crashes" "$crashed_runs" "$((to - from + 1))" "$tears"
