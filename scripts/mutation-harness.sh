#!/usr/bin/env bash
# Negative determinism: a determinism test that cannot fail is worthless.
#
# Each injection deliberately breaks determinism at one named site. The determinism suite
# MUST go red for every one of them. An injection that passes means the corresponding test
# has a hole, and the hole is fixed by strengthening the test, never by removing the
# injection. See docs/TESTING.md 7.4.
set -uo pipefail

cd "$(dirname "$0")/.."

INJECTIONS=(
  chaos-ambient-rng        # replace a draw_range with rand::random
  chaos-hash-iteration     # iterate a HashMap in a decision path
  chaos-delivery-tie       # break delivery ties by insertion order
  chaos-wall-clock-digest  # include a wall-clock duration in the trace digest
  chaos-syscall-passthru   # let one emulated syscall reach the host
  chaos-futex-order        # wake the waiter that has waited longest, not the drawn one
  chaos-node-clock-shared  # answer every node from the world clock, as if one machine
  chaos-deadline-tie       # fire due timers in an ambient order, not (tick, node, seq)
  chaos-fault-moment-drift # arm a scheduled fault a few ambient ticks late
  chaos-coverage-ambient   # mix a process counter into the coverage map
  chaos-worker-order       # fold a generation in the order the workers finished
  chaos-unknown-descriptor # answer a vectored write to a socket with EBADF
  chaos-proc-on-the-disk   # leave what the kernel answers about a process on the disk
)

# Without this, a suite that fails to build would count as five injections caught: the harness reads
# a red run as proof the injection worked, and a red run has more than one cause.
printf 'baseline %-24s ' 'no injection'
if cargo nextest run -p bohr-testkit >/dev/null 2>&1; then
  printf '[32mgreen[0m
'
else
  printf '[31mNOT GREEN[0m — the suite must pass before an injection can mean anything
'
  exit 1
fi

fail=0
log=$(mktemp)
trap 'rm -f "$log"' EXIT

for inj in "${INJECTIONS[@]}"; do
  printf 'injection %-24s ' "$inj"

  # A red run has more than one cause, and a build that does not compile is red. The chaos code
  # sits behind feature gates nothing else builds, so an injection that rotted into a compile error
  # reported itself caught for as long as nobody looked. Compiled first, and separately: what is
  # under test is that the suite fails, not that the injection still exists.
  if ! cargo nextest run --no-run -p bohr-testkit --features "bohr-testkit/$inj" >"$log" 2>&1; then
    printf '\033[31mDOES NOT BUILD\033[0m the injection has rotted, and a red run would have\n'
    printf '  read as it being caught. Last lines:\n'
    tail -20 "$log" | sed 's/^/  /'
    fail=1
    continue
  fi

  if cargo nextest run -p bohr-testkit --features "bohr-testkit/$inj" >"$log" 2>&1; then
    printf '\033[31mNOT CAUGHT\033[0m the determinism suite has a hole here\n'
    # Kept for the same reason the failure is reported: a hole nobody can see the shape of is a
    # hole nobody fixes.
    tail -20 "$log" | sed 's/^/  /'
    fail=1
  else
    printf '\033[32mcaught\033[0m\n'
  fi
done

exit $fail
