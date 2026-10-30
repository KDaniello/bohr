#!/usr/bin/env bash
# The milestone gate, run end to end on one host.
#
# Every check here is a claim the project makes about itself, and each is measured rather than
# asserted. Run it in the same environment continuous integration uses: a host that can trace, with
# unprivileged user namespaces available.
set -uo pipefail

cd "$(dirname "$0")/.."

BOHR=${BOHR:-target/release/bohr}
GUESTS=${GUESTS:-testdata/guests/build}
SEED=${SEED:-42}
RUNS=${RUNS:-1000}

fail=0
check() { # name, command...
  local name=$1; shift
  printf '%-34s ' "$name"
  if "$@" >/tmp/bohr-gate.log 2>&1; then
    printf '\033[32mok\033[0m\n'
  else
    printf '\033[31mFAILED\033[0m\n'
    sed 's/^/    /' /tmp/bohr-gate.log | tail -20
    fail=1
  fi
}

# One seed, a thousand runs, every run required to agree with the first.
reproduces() { # guest, args...
  local guest=$1; shift
  "$BOHR" run --seed "$SEED" --repeat "$RUNS" -- "$GUESTS/$guest" "$@" >/dev/null
}

# A seeded defect is found by searching seeds, which is what a campaign does at scale. The tool is
# told nothing about the defect: the guest checks itself and a non-zero exit is a finding.
finds() { # guest, budget
  local guest=$1 budget=$2 seed=1
  while [ "$seed" -le "$budget" ]; do
    if ! "$BOHR" run --seed "$seed" -- "$GUESTS/$guest" >/dev/null 2>&1; then
      echo "found at seed $seed"
      return 0
    fi
    seed=$((seed + 1))
  done
  echo "not found in $budget seeds"
  return 1
}

# The bytes a guest puts on the pipe are the same on two separate invocations of one seed.
#
# The emptiness check is what makes the comparison mean anything: two runs that printed nothing at
# all agree with each other, and a guest whose output never arrives would pass silently.
same_bytes() { # guest, args...
  local guest=$1; shift
  local a b
  a=$("$BOHR" run --seed "$SEED" -- "$GUESTS/$guest" "$@" 2>/dev/null | head -c 100 | xxd -p)
  b=$("$BOHR" run --seed "$SEED" -- "$GUESTS/$guest" "$@" 2>/dev/null | head -c 100 | xxd -p)
  [ -n "$a" ] && [ "$a" = "$b" ]
}

# The M2 gate: five real raft-rs processes through a split and a heal, one digest.
#
# Skipped rather than failed where the example has not been built: it needs a network to fetch
# its dependencies, and a gate that cannot run is not a gate that failed.
cluster_gate() {
  local node="examples/split-brain/build/split-brain-node"
  if [ ! -x "$node" ]; then
    echo "not built; run: make example-split-brain"
    return 0
  fi
  "$BOHR" run --seed 42 --repeat 100 --config examples/split-brain/scenario.toml -- "$node" >/dev/null
}

check "guest reproduces, $RUNS runs" reproduces clock_and_random
check "urandom reproduces, $RUNS runs" reproduces cat /dev/urandom
check "urandom bytes are identical" same_bytes cat /dev/urandom
check "timestamp counter reproduces" reproduces rdtsc_and_clock
check "threads reproduce, $RUNS runs" reproduces two_threads
check "files reproduce, $RUNS runs" reproduces file_roundtrip
check "a day of sleep costs no real time" reproduces long_sleep
check "a known race is found" finds race_counter 100
check "pct is no worse than random" bash scripts/policy-comparison.sh
check "slowdown is within its ceiling" bash scripts/bench-slowdown.sh
check "the cluster gate repeats 100x" cluster_gate
check "every injection breaks the suite" bash scripts/mutation-harness.sh

exit $fail
