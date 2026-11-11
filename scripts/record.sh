#!/usr/bin/env bash
# The record of a hunt: one row per target and cell, found or not.
#
# A finding sent alone says that one thing was found. The record says what was run, under which
# faults, and what held, so a cell that found nothing is stated as plainly as one that did.
#
# Usage: scripts/record.sh OUT ROOT...
#
# Each ROOT holds, per cell, `<target>-<cell>.log` (what `bohr explore` printed), and where the cell
# found something `<target>-<cell>/` (its bundles) and `<target>-<cell>.groups` (what
# `bohr findings` printed). The cell's scenario is read from the log's directory name only; the
# record does not run anything.
set -euo pipefail

out=${1:?usage: scripts/record.sh OUT ROOT...}
shift
[ $# -gt 0 ] || { echo "usage: scripts/record.sh OUT ROOT..." >&2; exit 2; }

{
  echo "# Hunt record"
  echo
  echo "| Target and cell | Runs | Runs/s | Faults landed | Findings | Groups | Verdicts |"
  echo "|---|---|---|---|---|---|---|"
  for root in "$@"; do
    for log in "$root"/*.log; do
      [ -e "$log" ] || continue
      name=$(basename "$log" .log)
      case "$name" in campaign) continue ;; esac
      line=$(grep -E '^campaign ' "$log" | tail -1 || true)
      runs=$(sed -nE 's/^campaign +([0-9]+) runs.*/\1/p' <<<"$line")
      rate=$(sed -nE 's/.* ([0-9.]+) runs\/s.*/\1/p' <<<"$line")
      found=$(grep -E '^ +findings [0-9]+' "$log" | tail -1 || true)
      findings=$(sed -nE 's/^ +findings ([0-9]+).*/\1/p' <<<"$found")
      landed=$(sed -nE 's/.*adversity landed ([0-9]+).*/\1/p' <<<"$found")
      groups=0 verdicts="-"
      if [ -f "$root/$name.groups" ]; then
        groups=$(sed -nE 's/^[0-9]+ findings in ([0-9]+) groups.*/\1/p' "$root/$name.groups" | head -1)
        verdicts=$(sed -nE 's/^ +verdict: ([^·]*[^ ·]).*/\1/p' "$root/$name.groups" | sed -E 's/: .*//' \
          | sort | uniq -c | awk '{c=$1; $1=""; printf "%s%s %s", (n++ ? ", " : ""), c, substr($0,2)}')
        [ -n "$verdicts" ] || verdicts="-"
      fi
      echo "| \`$name\` | ${runs:-?} | ${rate:-?} | ${landed:-?} | ${findings:-?} | $groups | $verdicts |"
    done
  done
  echo
  echo "A row with no findings is a result: that cell ran, and nothing it judges broke."
} > "$out"
echo "record: $(($(wc -l < "$out") - 6)) rows in $out"
