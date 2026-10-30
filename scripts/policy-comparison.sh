#!/usr/bin/env bash
# Which scheduling policy reaches the seeded defects, and how often.
#
# The claim under test is that PCT is worth defaulting to: it carries a probabilistic guarantee that
# uniform choice does not, and the corpus is where that either shows or does not. Both policies get
# the same seeds, so the comparison is of the policies and nothing else.
#
# The measure is the share of seeds that expose the defect rather than the first seed that does.
# "First seed" says nothing once a defect is reachable from most orderings — both policies answer 1
# — while the share separates a policy that reaches it from every angle from one that stumbles into
# it. See docs/TESTING.md 8.
set -uo pipefail

cd "$(dirname "$0")/.."

BOHR=${BOHR:-target/release/bohr}
GUESTS=${GUESTS:-testdata/guests/build}
SEEDS=${SEEDS:-60}

if [ ! -x "$BOHR" ]; then
  echo "no binary at $BOHR — build it with: cargo build --release -p bohr-cli" >&2
  exit 2
fi

# Seeds out of `SEEDS` that expose the defect. A guest that exits non-zero has caught itself out;
# nothing about the defect is known to the tool in advance.
exposures() { # guest, policy
  local guest=$1 policy=$2 seed=1 hits=0
  while [ "$seed" -le "$SEEDS" ]; do
    if ! "$BOHR" run --seed "$seed" --scheduler "$policy" -- "$GUESTS/$guest" >/dev/null 2>&1; then
      hits=$((hits + 1))
    fi
    seed=$((seed + 1))
  done
  echo "$hits"
}

printf '%-18s %12s %12s\n' defect pct random

fail=0
for guest in race_counter race_init; do
  [ -x "$GUESTS/$guest" ] || continue
  pct=$(exposures "$guest" pct)
  random=$(exposures "$guest" random)
  printf '%-18s %9s/%-2s %9s/%-2s\n' "$guest" "$pct" "$SEEDS" "$random" "$SEEDS"

  if [ "$pct" -eq 0 ]; then
    echo "  pct did not reach $guest in $SEEDS seeds" >&2
    fail=1
  fi
  # PCT falling behind uniform choice means either the implementation broke or the reasoning behind
  # defaulting to it did, and both are worth stopping for.
  if [ "$pct" -lt "$random" ]; then
    echo "  pct reached $guest less often than random" >&2
    fail=1
  fi
done

exit $fail
