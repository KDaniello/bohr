#!/usr/bin/env bash
# Documentation check: every relative link in the repository resolves, and every proof
# DETERMINISM.md cites exists.
#
# A link to a moved or deleted file is the commonest way a document starts lying, and nothing
# else notices it. See docs/README.md.
set -uo pipefail

cd "$(dirname "$0")/.."

fail=0
report() { printf '\033[31m%s\033[0m %s\n' "$1" "$2" >&2; fail=1; }

# Cross-references carry section numbers, so anchors are checked only for existence of the
# target file. Inline code spans are stripped first: documents quote the link syntax itself
# when explaining the convention.
while IFS= read -r md; do
  dir=$(dirname "$md")
  while IFS= read -r link; do
    [ -n "$link" ] || continue
    [ -e "$dir/$link" ] || report "LINK" "$md -> $link"
  done < <(perl -pe 's/``.*?``//g' "$md" |
           grep -oE '\]\(\.[^)#]*\)' | sed -E 's/^\]\(//; s/\)$//')
done < <(find . -name '*.md' -not -path './target/*' -not -path './.git/*' | sort)

# A proof DETERMINISM.md cites is a test that exists. `file::name` in a table's last column names
# `fn name` in a `file.rs` under crates/: a row whose proof was never written reads as solved.
while IFS= read -r cited; do
  file=${cited%%::*}
  name=${cited##*::}
  found=0
  while IFS= read -r rs; do
    grep -qE "fn ${name}[^a-z_0-9]" "$rs" && { found=1; break; }
  done < <(find crates -name "$file.rs" -not -path '*/target/*')
  [ "$found" = 1 ] || report "PROOF" "docs/DETERMINISM.md cites $cited, which no test is"
done < <(grep '^|' docs/DETERMINISM.md | awk -F'|' '{ print $(NF-1) }' |
         grep -oE '`[a-z_0-9]+::[a-z_0-9]+`' | tr -d '`' | sort -u)

exit $fail
