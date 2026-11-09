#!/usr/bin/env bash
# Surveys one target and records its walls in benches/baseline.toml as `[walls.<name>]`.
#
#   scripts/walls.sh <name> [survey flags…] -- <program> [args…]
#
# A wall is a call the target made that the model could not answer. The table is a list of them per
# target, written by this script and by nothing else, so "what stops this target" has one answer
# and it is a measurement rather than a recollection. A section already there under the same name
# is replaced whole; every other section is left as it stands.
#
# What a section is not: the target's surface. `bohr survey` answers each wall `ENOSYS` so the next
# can be reached, which sends the target down its fallback path, and a call the run never reached is
# not one the target cannot make. A section with no calls says one run at one seed met no wall.
#
# The survey is the gate on its own output: exit 0 (nothing unmodelled) and 4 (walls found) are
# recorded, and so is exit 3 with a `BOHR-2xxx` refusal — a wall a survey cannot answer its way past,
# which ends the run and is the first thing that stops the target. Anything else — a refused
# scenario, a missing binary — writes nothing and fails.
set -euo pipefail

cd "$(dirname "$0")/.."
BOHR=${BOHR:-./target/release/bohr}
BASELINE=${BASELINE:-benches/baseline.toml}
# Wall-clock seconds a survey may take. A target that makes no event for this long is spinning where
# nothing preempts it, and that is recorded as what stops it rather than waited out.
WALL_SECONDS=${WALL_SECONDS:-600}

name=${1:?usage: scripts/walls.sh <name> [survey flags…] -- <program> [args…]}
shift
case "$name" in
  *[!a-z0-9-]*) echo "a target name is lower-case letters, digits and dashes: $name" >&2; exit 2 ;;
esac
flags=()
while [ $# -gt 0 ] && [ "$1" != "--" ]; do
  flags+=("$1")
  shift
done
[ "${1:-}" = "--" ] || { echo "the program goes after --" >&2; exit 2; }
shift
program=${1:?the program goes after --}
[ -f "$program" ] || { echo "no binary at $program" >&2; exit 2; }

seed=1
for ((i = 0; i < ${#flags[@]}; i++)); do
  if [ "${flags[$i]}" = "--seed" ]; then
    seed=${flags[$((i + 1))]:?--seed needs a value}
  fi
done

out=$(mktemp -t bohr-walls-XXXXXX)
section=$(mktemp -t bohr-walls-XXXXXX)
trap 'rm -f "$out" "$section"' EXIT

set +e
timeout -k 10 "$WALL_SECONDS" "$BOHR" survey ${flags[@]+"${flags[@]}"} -- "$@" >"$out" 2>&1
code=$?
set -e
stopped_by=""
if [ "$code" = 124 ] || [ "$code" = 137 ]; then
  stopped_by="no end within ${WALL_SECONDS} s of wall time"
fi
if [ "$code" = 3 ]; then
  # The refusal that ended the run is the last such line; diagnostics before it name others.
  stopped_by=$(grep -oE 'BOHR-2[0-9]{3}: .*' "$out" | tail -1 || true)
fi
if [ "$code" != 0 ] && [ "$code" != 4 ] && [ -z "$stopped_by" ]; then
  cat "$out" >&2
  echo "survey of $name exited $code; nothing recorded" >&2
  exit 1
fi

# A dynamic binary asks the loader for its libraries first, and those calls are the scenario's
# prepared state at work rather than the target's own; the note says which kind of run this was.
if readelf -l "$program" 2>/dev/null | grep -q 'Requesting program interpreter'; then
  linkage=dynamic
else
  linkage=static
fi
sha=$(sha256sum "$program" | cut -d' ' -f1)

# What was run, so a section can be measured again: the survey flags and the arguments after the
# binary. The binary is named by its hash rather than its path, which is this host's business.
# The home directory becomes `~`, so a section names no user of the host it was measured on.
shift
survey_flags=${flags[*]:-}
survey_flags=${survey_flags//$HOME/\~}
set -- "${@//$HOME/\~}"
# Through the environment rather than `-v`, which would read escapes in them and drop backslashes.
W_NAME=$name W_BINARY=$(basename "$program") W_SHA=$sha W_LINKAGE=$linkage W_SEED=$seed \
  W_FLAGS=$survey_flags W_ARGS="$*" W_STOPPED=$stopped_by awk '
  BEGIN {
    name = ENVIRON["W_NAME"]; binary = ENVIRON["W_BINARY"]; sha = ENVIRON["W_SHA"]
    linkage = ENVIRON["W_LINKAGE"]; seed = ENVIRON["W_SEED"]; flags = ENVIRON["W_FLAGS"]
    args = ENVIRON["W_ARGS"]; stopped = ENVIRON["W_STOPPED"]
  }
  # A TOML basic string: backslashes and quotes escaped, nothing else needs to be.
  function str(s) { gsub(/\\/, "\\\\", s); gsub(/"/, "\\\"", s); return "\"" s "\"" }
  /^survey / { events = $4 }
  /did not run to completion/ { getline; sub(/^ +/, ""); outcome = $0 }
  /^ +nr +name +calls +wall$/ { table = 1; next }
  table && /^ *[0-9]+ / {
    nr = $1; call = $2; count = $3
    wall = $0; sub(/^ *[0-9]+ +[^ ]+ +[0-9]+ +/, "", wall)
    if (call == "?") call = "nr_" nr
    rows[++n] = sprintf("%s = { nr = %d, calls = %d, wall = %s }", call, nr, count, str(wall))
    next
  }
  table && /^$/ { table = 0 }
  END {
    printf "[walls.%s]\n", name
    printf "binary = %s\n", str(binary)
    printf "sha256 = %s\n", str(sha)
    printf "linkage = %s\n", str(linkage)
    printf "survey = %s\n", str(flags)
    printf "args = %s\n", str(args)
    printf "seed = %d\n", seed
    if (stopped != "") {
      printf "ran = %s\n", str(stopped ~ /^BOHR-/ ? "refused" : "no end")
      printf "stopped_by = %s\n", str(stopped)
    } else {
      printf "events = %d\n", events
      printf "ran = %s\n", str(outcome == "" ? "to completion" : outcome)
    }
    printf "walls = %d\n", n
    if (n > 0) {
      printf "\n[walls.%s.calls]\n", name
      for (i = 1; i <= n; i++) print rows[i]
    }
  }' "$out" >"$section"

# The old section and its table go, wherever they stood; the new one is appended. Comment lines
# met while skipping are held, because they head whatever section follows and go out with it.
W_NAME=$name awk '
  BEGIN { name = ENVIRON["W_NAME"] }
  /^\[/ {
    skip = ($0 == "[walls." name "]" || $0 == "[walls." name ".calls]")
    if (!skip) printf "%s", held
    held = ""
  }
  skip && /^#/ { held = held $0 "\n"; next }
  skip && /^$/ { if (held != "") held = held "\n"; next }
  skip { held = ""; next }
  { print }
  END { printf "%s", held }
' "$BASELINE" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}' >"$out"
{
  cat "$out"
  echo
  cat "$section"
} >"$BASELINE"

cat "$section"
