#!/usr/bin/env bash
# Slowdown versus native, measured side by side on one machine in one job.
#
# An absolute duration says nothing across runners; a ratio measured against the same guest run
# natively, seconds apart, is comparable everywhere. See docs/TESTING.md 11.
set -uo pipefail

cd "$(dirname "$0")/.."

BOHR=${BOHR:-target/release/bohr}
GUESTS=${GUESTS:-testdata/guests/build}
BASELINE=${BASELINE:-benches/baseline.toml}
REPEATS=${REPEATS:-3}

# Syscalls `syscall_storm` issues, matching ITERATIONS in its source.
STORM_CALLS=${STORM_CALLS:-200000}

if [ ! -x "$BOHR" ]; then
  echo "no binary at $BOHR — build it with: cargo build --release -p bohr-cli" >&2
  exit 2
fi

# Wall-clock seconds for a command, taken as the best of several attempts: the fastest run is the
# one least disturbed by everything else on the machine.
best_of() {
  local best=""
  for _ in $(seq "$REPEATS"); do
    local start end elapsed
    start=$(date +%s.%N)
    "$@" >/dev/null 2>&1 || true
    end=$(date +%s.%N)
    elapsed=$(echo "$end - $start" | bc -l)
    if [ -z "$best" ] || [ "$(echo "$elapsed < $best" | bc -l)" = 1 ]; then
      best=$elapsed
    fi
  done
  echo "$best"
}

# A value recorded for a guest, or nothing if the baseline does not carry it.
guests_in() { # baseline
  sed -nE 's/^\[guests\.([a-z0-9_]+)\]$/\1/p' "$1"
}

value_of() { # guest, key
  awk -v guest="[guests.$1]" -v key="$2" '
    $0 == guest { found = 1; next }
    found && $1 == key { print $3; exit }
    found && substr($0, 1, 1) == "[" { exit }
  ' "$BASELINE" | tr -d '"'
}

# Starting a process is paid by both sides and says nothing about interception. Measured on its own
# so the per-call figure below is the cost of the work rather than of `execve`.
traced_start=0
native_start=0
if [ -x "$GUESTS/startup" ]; then
  native_start=$(best_of "$GUESTS/startup")
  traced_start=$(best_of "$BOHR" run --seed 1 -- "$GUESTS/startup")
fi

printf '%-16s %10s %10s %9s %9s\n' guest native bohr slowdown ceiling

fail=0
# The list lives in the baseline, not here. One file naming both what is measured and what it is
# allowed to cost is one file that can be wrong; two are two that can disagree, and the one nobody
# edits is the one that goes stale.
for guest in $(guests_in "$BASELINE"); do
  path="$GUESTS/$guest"
  if [ ! -x "$path" ]; then
    printf '%-16s %10s
' "$guest" "not built"
    continue
  fi

  # shellcheck disable=SC2206 # deliberate word splitting: argv comes from the baseline
  args=($(value_of "$guest" args))

  native=$(best_of "$path" "${args[@]}")
  traced=$(best_of "$BOHR" run --seed 1 -- "$path" "${args[@]}")
  ratio=$(echo "$traced / $native" | bc -l)
  ceiling=$(value_of "$guest" ceiling)

  printf '%-16s %9.3fs %9.3fs %8.1fx %8.1fx\n' "$guest" "$native" "$traced" "$ratio" "${ceiling:-0}"
  if [ -n "$ceiling" ] && [ "$(echo "$ratio > $ceiling" | bc -l)" = 1 ]; then
    echo "  $guest is past the ceiling recorded in $BASELINE" >&2
    fail=1
  fi

  # What one intercepted syscall costs. The ratio above divides by a native time that is mostly
  # process startup and swings with it; this number does not, so it is the one to watch.
  if [ "$guest" = syscall_storm ]; then
    per_call=$(echo "($traced - $traced_start) * 1000000 / $STORM_CALLS" | bc -l)
    limit=$(value_of syscall_storm per_call_us_ceiling)
    printf '%-16s %31.1fus %7sus  per intercepted syscall\n' "" "$per_call" "${limit:-none}"
    if [ -n "$limit" ] && [ "$(echo "$per_call > $limit" | bc -l)" = 1 ]; then
      echo "  one intercepted syscall costs more than $BASELINE allows" >&2
      fail=1
    fi
  fi
done

printf '\nprocess startup, excluded from the per-call figure: %.3fs native, %.3fs traced\n' \
  "$native_start" "$traced_start"

exit $fail
