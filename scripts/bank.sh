#!/usr/bin/env bash
# Replays every banked finding to its digest and fails if one no longer matches.
#
# The bank is findings/bank/, private. Each finding has a directory with its bundles, an
# entry.toml, bin/ holding the exact binaries it was found on under the names the bundles record,
# and the prepared state the run began from, under the path its configuration names. A rebuilt
# binary or state is a different run, so the bank keeps the originals and replays each bundle from
# its own directory.
#
# A bundle that stops replaying means the tool changed what a stored run means.
set -u
cd "$(dirname "$0")/.."
BOHR=$(realpath "${BOHR:-target/release/bohr}")
bank=findings/bank
if [ ! -d "$bank" ]; then
  echo "no bank here: $bank is private and is not part of a published tree"
  exit 0
fi
total=0
failed=0
for entry in "$bank"/*/; do
  [ -f "$entry/entry.toml" ] || continue
  # The bundles the entry lists; one listed and absent fails, so a bank copied without its
  # bundles does not pass as replaying.
  listed=$(sed -n 's/^bundles = \[\(.*\)\]/\1/p' "$entry/entry.toml" | tr -d '",')
  for name in $listed; do
    bundle="$entry$name"
    total=$((total + 1))
    if [ ! -f "$bundle" ]; then
      failed=$((failed + 1))
      echo "MISSING $bundle"
      continue
    fi
    if out=$(cd "$entry" && "$BOHR" replay --verify-only --binaries bin "$name" 2>&1); then
      echo "ok      $bundle"
    else
      failed=$((failed + 1))
      echo "FAILED  $bundle"
      printf '%s\n' "$out" | sed 's/^/        /'
    fi
  done
done
echo "$((total - failed)) of $total banked bundles replay to their digest"
[ "$failed" -eq 0 ]
