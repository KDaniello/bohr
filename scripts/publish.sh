#!/usr/bin/env bash
# Publishes the committed private tree to the public repository, without what is not disclosed.
#
# The private repository carries findings in other people's projects that are not disclosed yet;
# the public one carries everything else. What stays private lives under findings/, and
# findings/publish.conf names any other path that does, local paths to replace, and markers whose
# presence anywhere in the public tree stops the publication. Without the file nothing is
# published: an absent list of secrets is not an empty one.
#
# The public history is its own: each publication is one commit naming the private commit it was
# made from. Without --push the tree is prepared, checked and summarised, and nothing leaves.
#
#   scripts/publish.sh                                 prepare and check
#   scripts/publish.sh --push -m "<msg>"               prepare, check, commit and push to main
#   scripts/publish.sh --push -m "<msg>" --branch <b>  the same onto branch <b>, cut from main,
#                                                      for a pull request into main
set -euo pipefail
cd "$(dirname "$0")/.."

REMOTE=${BOHR_PUBLIC_REMOTE:-https://github.com/KDaniello/bohr.git}
CONF=${BOHR_PUBLISH_CONF:-$PWD/findings/publish.conf}
push=0
message=""
branch=""
while [ $# -gt 0 ]; do
  case $1 in
    --push) push=1 ;;
    -m) message=$2; shift ;;
    --branch) branch=$2; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

[ -f "$CONF" ] || { echo "no $CONF: nothing says what is private, so nothing is published" >&2; exit 2; }
if [ -n "$(git status --porcelain)" ]; then
  echo "the working tree has uncommitted changes; a publication is made from a commit" >&2
  exit 2
fi
if [ "$push" = 1 ] && [ -z "$message" ]; then
  echo "--push needs -m: the public history says what each publication brought" >&2
  exit 2
fi

rules() { grep -vE '^[[:space:]]*(#|$)' "$CONF"; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/tree"

# The tree as committed, never as it happens to sit on disk.
git archive HEAD | tar -x -C "$work/tree"
rm -rf "$work/tree/findings"
while read -r kind from to; do
  case $kind in
    exclude) rm -rf "${work:?}/tree/$from" ;;
    rewrite) grep -rlIF -- "$from" "$work/tree" | while IFS= read -r f; do
               sed -i "s|$from|$to|g" "$f"
             done ;;
    marker) ;;
    *) echo "unknown rule in $CONF: $kind" >&2; exit 2 ;;
  esac
done < <(rules)

# The gate. Checked after every exclusion and rewrite, on exactly the bytes that would be pushed.
# Fails closed: grep answers 0 for a hit and 1 for none, and anything else is the check itself
# failing, which must never read as "nothing found". The marker is escaped and matched as an
# extended expression because some builds of grep abort on -i together with -F.
leaked=0
while read -r kind text; do
  [ "$kind" = marker ] || continue
  pattern=$(printf '%s' "$text" | sed 's/[][\.^$*+?(){}|]/\\&/g')
  set +e
  hits=$(grep -rlIiE -- "$pattern" "$work/tree")
  status=$?
  set -e
  case $status in
    0) printf 'LEAK  %-36s %s\n' "$text" "$(echo "$hits" | sed "s|^$work/tree/||" | tr '\n' ' ')" >&2
       leaked=1 ;;
    1) ;;
    *) echo "the leak check failed on '$text' (grep exit $status); nothing is published" >&2
       exit 1 ;;
  esac
done < <(rules)
[ "$leaked" = 0 ] || { echo "not published: the tree above names an undisclosed finding" >&2; exit 1; }

git clone -q "$REMOTE" "$work/public"
find "$work/public" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
cp -a "$work/tree/." "$work/public/"

rev=$(git rev-parse --short HEAD)
cd "$work/public"
git add -A
if git diff --cached --quiet; then
  echo "the public repository already holds this tree"
  exit 0
fi
git diff --cached --stat | tail -1
echo "markers checked: $(rules | grep -c '^marker')"

if [ "$push" = 1 ]; then
  git commit -q -m "$message" -m "Published from private $rev."
  # A branch is cut from public main, so the pull request it backs carries this publication alone.
  git push -q origin "HEAD:${branch:-main}"
  echo "published $(git rev-parse --short HEAD) from private $rev to ${branch:-main}"
else
  echo "prepared, not pushed: rerun with --push -m \"<message>\""
fi
