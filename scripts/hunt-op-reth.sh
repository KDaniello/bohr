#!/usr/bin/env bash
# Sweeps seeds against an execution client and reports what the campaign actually checked.
#
# The totals matter as much as the findings. A sweep that reports "100 clean" without saying how
# many promises it asked back is a sweep that cannot tell a client that kept them from one that was
# never asked — and against this target most runs ask a handful, because a crash costs the run a
# second startup and a run that spent its horizon starting up asked nothing.
#
# `blocks` is the number that makes the rest readable: it counts the blocks the client built and
# validated across the sweep, and a sweep whose blocks are near zero measured its own startup.
#
# `behind` is not a failure and is counted so that a clean sweep cannot be mistaken for a gentle
# one. An execution client keeps recent blocks in memory and writes them down on a schedule of its
# own, so a crash costs it the unwritten ones and the consensus client is expected to drive it
# forward again. `forked` is the fault: a different block where a final one stood.
set -uo pipefail

cd "$(dirname "$0")/.."
BOHR=${BOHR:-./target/release/bohr}
CLIENT=${CLIENT:?set CLIENT to the op-reth binary the scenario names}
config=${1:-examples/engine-driver/hunt.toml}
from=${2:-1}
to=${3:-20}
# Where a run that found something is kept. A hunt that reports a finding and throws away the
# bundle has reported a rumour: the bundle is the finding, and everything after it — the reduction,
# the report, the thing a maintainer replays — is read off it.
findings_dir=${FINDINGS:-findings}
mkdir -p "$findings_dir"

clean=0
findings=0
other=0
acknowledged=0
verified=0
forked=0
behind=0
unanswered=0
blocks=0
crashes=0
crashed_runs=0
at_risk=0
dead_on_restart=0
bundle=$(mktemp -t bohr-hunt-XXXXXX.bohr)
trap 'rm -f "$bundle"' EXIT

for seed in $(seq "$from" "$to"); do
  out=$(timeout 900 "$BOHR" run --seed "$seed" --config "$config" --max-events 6000000 \
    --save "$bundle" -- "$CLIENT" 2>&1)
  code=$?

  line=$(printf '%s\n' "$out" | grep -m1 '^acknowledged ' || true)
  if [ -n "$line" ]; then
    # By name rather than by position, so a reader never has to know the order they were printed in.
    count() { printf '%s' "$line" | grep -oE "$1 [0-9]+" | head -1 | awk '{print $2}'; }
    acknowledged=$((acknowledged + $(count acknowledged)))
    verified=$((verified + $(count verified)))
    forked=$((forked + $(count forked)))
    behind=$((behind + $(count behind)))
    unanswered=$((unanswered + $(count unanswered)))
  fi
  blocks=$((blocks + $(printf '%s\n' "$out" | grep -c '^block ' || true)))

  # A client that will not start again. Counted separately because the panel can miss it: a run
  # whose caller finished and exited nought is judged on that, and the node that died behind it
  # never reaches the verdict. A campaign that reported such a run as clean would be reporting the
  # worst outcome this target has as the best one.
  if printf '%s\n' "$out" | grep -q 'shutting down due to error'; then
    dead_on_restart=$((dead_on_restart + 1))
    printf '=== seed %s: the client did not start again ===\n' "$seed"
    printf '%s\n' "$out" | grep -m1 '^Error:' || true
    cp "$bundle" "$findings_dir/seed-$seed-will-not-start.bohr"
  fi

  # How many crashes this run landed, and how many of them landed on a node with something
  # outstanding. The second is the one that makes a clean campaign mean anything: crashes that all
  # fell where nothing was at stake asked nothing, and nothing else in a run tells them apart.
  # `domain event 3` is what the world writes at a crash to say what it put at risk (API §5.6).
  timeline=$("$BOHR" trace "$bundle" 2>/dev/null)
  killed=$(printf '%s\n' "$timeline" | grep -c 'a node was killed' || true)
  crashes=$((crashes + killed))
  at_risk=$((at_risk + $(printf '%s\n' "$timeline" | grep -c 'domain event 3' || true)))
  [ "$killed" -gt 0 ] && crashed_runs=$((crashed_runs + 1))

  case $code in
    0) clean=$((clean + 1)) ;;
    1)
      findings=$((findings + 1))
      cp "$bundle" "$findings_dir/seed-$seed.bohr"
      # What the panel decided, as it wrote it. The client's own counts are printed beside it
      # because the two can disagree — the world's durability watcher approximates an
      # acknowledgement from the outside, and the client knows what it actually promised.
      printf '=== FINDING at seed %s ===\n' "$seed"
      "$BOHR" trace "$findings_dir/seed-$seed.bohr" --format json 2>/dev/null |
        grep -m 3 -oE '"(PropertyViolated|DurabilityViolated|LivenessViolated|GuestCrashed)[^}]*' ||
        true
      printf '  %s\n' "$line"
      printf '%s\n' "$out" | grep -E '^FINDING:|^nothing asked' || true
      ;;
    *)
      other=$((other + 1))
      printf '=== seed %s exited %s ===\n%s\n' "$seed" "$code" "$line"
      ;;
  esac
done

printf 'swept %s..%s: %s clean, %s findings, %s other\n' "$from" "$to" "$clean" "$findings" "$other"
printf '  %s blocks built and validated\n' "$blocks"
printf '  %s promises verified of %s, %s forked, %s behind, %s unanswered\n' \
  "$verified" "$acknowledged" "$forked" "$behind" "$unanswered"
printf '  %s crashes across %s runs of %s, %s of them with something at risk\n' \
  "$crashes" "$crashed_runs" "$((to - from + 1))" "$at_risk"
printf '  %s runs left the client unable to start again\n' "$dead_on_restart"
