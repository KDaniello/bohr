#!/usr/bin/env bash
# Documentation checks: relative links resolve, and the Russian mirror keeps the same shape.
#
# The parity check exists because a translation that has silently fallen behind is worse
# than no translation: a reader trusts it. See docs/README.md.
set -uo pipefail

cd "$(dirname "$0")/.."

fail=0
report() { printf '\033[31m%s\033[0m %s\n' "$1" "$2" >&2; fail=1; }

# The documents git carries, which is what a checkout in continuous integration sees. Asking the
# filesystem instead would make this check answer differently here and there: an internal planning
# document, deliberately kept out of the repository, is not a document that owes anyone a mirror.
docs() {
  if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    git ls-files -- 'docs/*.md' 'docs/**/*.md'
  else
    find docs -name '*.md'
  fi
}

# 1. Every English document has a Russian mirror, and nothing extra exists on either side.
while IFS= read -r en; do
  ru="docs/ru/${en#docs/}"
  [ -f "$ru" ] || report "MISSING" "$ru (mirror of $en)"
done < <(docs | grep -v '^docs/ru/' | sort)

while IFS= read -r ru; do
  en="docs/${ru#docs/ru/}"
  [ -f "$en" ] || report "ORPHAN" "$ru has no English original"
done < <(docs | grep '^docs/ru/' | sort)

# 2. Heading structure parity. We compare the tree, not the prose: a section added to one
#    language and not the other is the failure mode this catches.
while IFS= read -r en; do
  ru="docs/ru/${en#docs/}"
  [ -f "$ru" ] || continue
  a=$(grep -cE '^#{1,3} ' "$en")
  b=$(grep -cE '^#{1,3} ' "$ru")
  [ "$a" = "$b" ] || report "STRUCTURE" "$en has $a headings, $ru has $b"

  # Section numbers must match one for one, in order.
  na=$(grep -oE '^#{2,3} [0-9]+(\.[0-9]+)*' "$en" | awk '{print $2}' | tr '\n' ' ')
  nb=$(grep -oE '^#{2,3} [0-9]+(\.[0-9]+)*' "$ru" | awk '{print $2}' | tr '\n' ' ')
  [ "$na" = "$nb" ] || report "NUMBERING" "$en: [$na] vs $ru: [$nb]"
done < <(docs | grep -v '^docs/ru/' | sort)

# 3. Relative links resolve. Cross-references carry section numbers, so anchors are checked
#    only for existence of the target file. Inline code spans are stripped first: documents
#    quote the link syntax itself when explaining the convention.
while IFS= read -r md; do
  dir=$(dirname "$md")
  while IFS= read -r link; do
    [ -n "$link" ] || continue
    [ -e "$dir/$link" ] || report "LINK" "$md -> $link"
  done < <(perl -pe 's/``.*?``//g' "$md" |
           grep -oE '\]\(\.[^)#]*\)' | sed -E 's/^\]\(//; s/\)$//')
done < <(find . -name '*.md' -not -path './target/*' -not -path './.git/*' | sort)

exit $fail
