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

# Fault targeting: where a drawn fault lands, uniformly or only at moments a crash would lose
# something. Over every seeded defect whose scenario draws node faults, the same seeds both ways.
# The result is recorded either way; guided becomes the default only where it finds at least as
# often as uniform on every defect, and this script does not make it so — it reports.
if [ "${TARGETING:-1}" = 1 ]; then
  printf '\n%-46s %12s %12s\n' "defect scenario" uniform guided
  work=$(mktemp -d -t bohr-targeting-XXXXXX)
  trap 'rm -rf "$work"' EXIT
  found() { # scenario file
    "$BOHR" sweep "$1" --seeds "1..$SEEDS" -j "${JOBS:-4}" 2>/dev/null \
      | sed -nE 's/^ *[0-9]+ seeds · ([0-9]+) findings.*/\1/p' | head -1
  }
  for scenario in testdata/bugs/*/scenario*.toml; do
    grep -q '^kill_probability' "$scenario" || continue
    uniform=$work/uniform.toml guided=$work/guided.toml
    sed -E '/^targeting *=/d' "$scenario" > "$uniform"
    sed -E '/^targeting *=/d; /^\[faults\]$/a targeting = "guided"' "$scenario" > "$guided"
    printf '%-46s %9s/%-2s %9s/%-2s\n' "$scenario" "$(found "$uniform")" "$SEEDS" \
      "$(found "$guided")" "$SEEDS"
  done
fi

exit $fail
