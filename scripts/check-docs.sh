#!/usr/bin/env bash
# Documentation check: every relative link in the repository resolves.
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

exit $fail
