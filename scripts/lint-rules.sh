#!/usr/bin/env bash
# Mechanical architecture rules from docs/ARCHITECTURE.md 8, rule 14.
#
# These are architectural violations, not style nits: each one names the decision it protects.
# The fix is to move the code, never to silence the check.
set -uo pipefail

cd "$(dirname "$0")/.."

fail=0
report() { # rule, decision, message
  printf '\033[31mRULE %s (%s)\033[0m %s\n' "$1" "$2" "$3" >&2
  fail=1
}

# Nothing to check before the first crate lands.
[ -d crates ] || { echo "no crates yet, skipping"; exit 0; }

src_of() { # crate -> its src dir, if present
  [ -d "crates/$1/src" ] && echo "crates/$1/src"
}

SANS_IO=(bohr-kernel bohr-world bohr-explorer bohr-reducer)
ABOVE_BOUNDARY=(bohr-types bohr-tape bohr-trace bohr-backend bohr-kernel bohr-world \
                bohr-explorer bohr-reducer bohr-report bohr-sdk-proto bohr-x86)

grep_in() { # pattern, dirs...
  local pat="$1"; shift
  [ $# -eq 0 ] && return 1
  grep -rnE --include='*.rs' "$pat" "$@" 2>/dev/null
}

# The determinism rules alone skip `chaos.rs`, which holds the deliberate defects the mutation
# harness enables one at a time: each is a violation on purpose, and confined to those files so
# that the check over everything else stays exact. Every other rule still applies to them.
grep_in_determinism() { # pattern, dirs...
  local pat="$1"; shift
  [ $# -eq 0 ] && return 1
  grep -rnE --include='*.rs' --exclude='chaos.rs' "$pat" "$@" 2>/dev/null
}

# §5.2: the backend boundary does not leak.
dirs=(); for c in "${ABOVE_BOUNDARY[@]}"; do d=$(src_of "$c") && dirs+=("$d"); done
if hits=$(grep_in '\b(ptrace|seccomp|waitpid|libc::)' "${dirs[@]}"); then
  report 14a §5.2 $'ptrace vocabulary above the backend boundary:\n'"$hits"
fi

# §5.12: unsafe lives in exactly one crate.
dirs=(); for c in "${ABOVE_BOUNDARY[@]}" bohr-cli bohr-testkit; do d=$(src_of "$c") && dirs+=("$d"); done
if hits=$(grep_in '(^|[^_[:alnum:]])unsafe[[:space:]]' "${dirs[@]}"); then
  report 14b §5.12 $'unsafe outside bohr-backend-ptrace:\n'"$hits"
fi

# §5.1 and rule 9: no host clock, no ambient randomness and no host file system in decision paths.
# A scenario that read the host while it was parsed would differ between two machines holding the
# same text; the command line reads the host and hands the world what it read.
dirs=(); for c in "${SANS_IO[@]}"; do d=$(src_of "$c") && dirs+=("$d"); done
if hits=$(grep_in_determinism '(Instant::now|SystemTime::now|thread_rng|rand::random|std::fs::|File::(open|create))' "${dirs[@]}"); then
  report 14c §5.1 $'host clock, ambient RNG or the host file system in a sans-io crate:\n'"$hits"
fi

# Rule 10: hash iteration order is not stable across runs.
if hits=$(grep_in_determinism '\b(HashMap|HashSet)\b' "${dirs[@]}"); then
  report 14d 'rule 10' $'HashMap/HashSet in a decision path; use BTreeMap or IndexMap:\n'"$hits"
fi

# §3: every crate depends on exactly what ARCHITECTURE.md's table says. A graph drawn by hand drifts
# from the manifests, and it had: the explorer and the reducer named crates they never used.
graph=$(sed -n '/<!-- crate-graph -->/,/<!-- \/crate-graph -->/p' docs/ARCHITECTURE.md \
  | grep -E '^\| `bohr-' | sed -E 's/^\| `([a-z0-9-]+)` \|(.*)\|$/\1:\2/' | tr -d '`' \
  | while IFS=: read -r crate deps; do
      printf '%s:%s\n' "$crate" "$(echo "$deps" | tr ' ' '\n' | grep -E '^bohr-' | sort | tr '\n' ' ')"
    done)
manifests=$(for m in crates/*/Cargo.toml; do
    crate=$(basename "$(dirname "$m")")
    deps=$(sed -n '/^\[dependencies\]/,/^\[/p' "$m" | grep -oE '^bohr-[a-z0-9-]+' | grep -v '^bohr-sdk$' \
      | sort | tr '\n' ' ')
    printf '%s:%s\n' "$crate" "$deps"
  done)
if [ "$(echo "$graph" | sort)" != "$(echo "$manifests" | sort)" ]; then
  report 14e §3 $'the manifests and ARCHITECTURE.md §3 name different dependencies:\n'"$(
    diff <(echo "$graph" | sort) <(echo "$manifests" | sort) \
      | sed 's/^</  the table says: /;s/^>/  the manifest says: /' | grep -E '^  ')"
fi

# §5.2, §5.16, §5.17: the kernel declares its ports and depends on none of their implementations.
if [ -f crates/bohr-kernel/Cargo.toml ]; then
  if grep -qE '^bohr-backend-(ptrace|null|kvm)' crates/bohr-kernel/Cargo.toml; then
    report 14e §5.2 'bohr-kernel depends on a backend implementation'
  fi
  if grep -qE '^bohr-(world|oracle)' crates/bohr-kernel/Cargo.toml; then
    report 14e2 '§5.16, §5.17' 'bohr-kernel depends on a world-model or oracle implementation'
  fi
fi

# API 1 rule 2: floating point in a decision path is a cross-machine reproduction failure.
if hits=$(grep_in '\b(f32|f64)\b' "${dirs[@]}"); then
  report 14f 'API 1.2' $'floating point in a sans-io crate; use fixed point:\n'"$hits"
fi

# 8.1: everything in code is English.
#
# Matched by UTF-8 lead byte rather than by code point: `grep -P` is a GNU extension that is
# absent or locale-restricted on some hosts, and there it fails open — the rule looks green while
# checking nothing. 0xD0/0xD1 lead the two-byte sequences covering U+0400-U+04FF.
if hits=$(LC_ALL=C grep -rn --include='*.rs' $'[\xd0\xd1]' crates 2>/dev/null); then
  report 14g '8.1' $'non-English text in code:\n'"$hits"
fi

# 8.1: comments describe the code, never the project's plans. Milestone names and "for now" date
# themselves the moment they are written and become lies shortly after.
if hits=$(grep -rnE --include='*.rs' \
    '(\bM[0-6]\b|[Rr]oadmap|for now|for the moment|added later|implemented later|will be added)' \
    crates 2>/dev/null); then
  report 14j '8.1' $'project meta in a code comment:\n'"$hits"
fi

# Readability R2: what a person reads is never debug output. An outcome, an event or a pass printed
# with `{:?}` reads as the identifier it came from; the words for each live in bohr-report.
if hits=$(grep -rnE --include='*.rs' 'e?print(ln)?!\(.*\{[a-z_.]*:\?\}' crates/bohr-cli/src 2>/dev/null); then
  report R2 'readability' $'debug formatting in what a user reads; use bohr-report::narrate or describe:
'"$hits"
fi

# 8.1: a TODO without an owner and an issue link lives forever.
if hits=$(grep -rnE --include='*.rs' 'TODO' crates 2>/dev/null | grep -vE 'TODO\([^)]+\).*#[0-9]+'); then
  report 14h '8.1' $'TODO without an owner and an issue link:\n'"$hits"
fi

# API 9: the documented bundle format version must match the code.
#
# sed rather than `grep -P`, which is a GNU extension that fails open where it is unavailable and
# would leave this check silently passing. The tape ABI needs no such check: it is one constant in
# bohr-types, which the tape and the bundle both read.
if [ -f crates/bohr-trace/src/lib.rs ]; then
  code=$(sed -nE 's/^pub const BUNDLE_FORMAT: u16 = ([0-9]+);.*/\1/p' crates/bohr-trace/src/lib.rs | head -1)
  docv=$(sed -nE 's/^ *[0-9]+ +[0-9]+ +bundle_format +u16, currently ([0-9]+).*/\1/p' docs/API.md | head -1)
  if [ -n "$code" ] && [ -n "$docv" ] && [ "$code" != "$docv" ]; then
    report 14i 'API 9' "bundle_format is $code in code and $docv in docs/API.md"
  fi

  # The trace format names the faults a run can carry, and a world narrates them as they fire.
  # Those are two lists, and nothing at run time holds them together, so they drift: a kind the
  # format names and no world fires is a promise nobody keeps, and a kind fired and never named
  # cannot be read back.
  world=crates/bohr-world/src
  if [ -d "$world" ]; then
    # The leading boundary matters: `NodeFaultKind::Kill` contains `FaultKind::Kill`, and a
    # pattern without it reports every scheduled fault as a narrated one.
    narrated=$(grep -rhoE 'fault: FaultKind::[A-Za-z]+' "$world" | sed 's/.*:://' | sort -u)
    named=$(sed -n '/^pub enum FaultKind/,/^}/p' crates/bohr-trace/src/event.rs \
      | grep -oE '^    [A-Z][A-Za-z]+' | tr -d ' ' | sort -u)
    if [ -n "$named" ] && [ "$named" != "$narrated" ]; then
      report 14m §4.5 $'the trace format names fault kinds no world fires, or the reverse:\n'"$(
        diff <(echo "$named") <(echo "$narrated") \
          | sed 's/^</  named, never fired: /;s/^>/  fired, never named: /')"
    fi
  fi

  # §5.13: the report is one static file. Nothing in its behaviour may reach the network, load a
  # module or build markup from a string — the last because every string in the page came out of a
  # guest. The linter greps only Rust, so this is the one check these assets will ever get.
  js=crates/bohr-report/assets/report.js
  if [ -f "$js" ]; then
    if hits=$(grep -nE 'fetch\(|XMLHttpRequest|eval\(|innerHTML|import |https?://|new Worker|localStorage' "$js"); then
      report 14l §5.13 $'the report page reaches for something a single static file may not:
'"$hits"
    fi
  fi

  # A syscall is named twice: by the backend that decides what to do with it, and by the asset the
  # layers above it read on a host where no backend is built. A number the two disagree about would
  # print under one name in a trace and another in the report made from it, and — since the asset
  # also says which registers a call reads — would hash the wrong ones into that run's digest.
  # Compared on the first two columns, because the third exists only in the asset.
  table=crates/bohr-backend-ptrace/src/linux/syscalls.rs
  asset=crates/bohr-trace/assets/syscalls-x86_64.tsv
  if [ -f "$table" ] && [ -f "$asset" ]; then
    from_backend=$(grep -oE '(admit|trace)\((-?[0-9]+), "([a-z0-9_]+)"\)' "$table" \
      | sed -E 's/(admit|trace)\((-?[0-9]+), "([a-z0-9_]+)"\)/\2\t\3\t\1/' | sort -n -u)
    from_asset=$(grep -vE '^#|^$' "$asset" | cut -f1,2,4 | sort -n -u)
    if [ "$from_backend" != "$from_asset" ]; then
      report 14k §5.7 $'the syscall table and the trace asset disagree:\n'"$(
        diff <(echo "$from_backend") <(echo "$from_asset") | head -20)"
    fi
  fi


  # The mutation harness is only as good as its inventory: an injection the table does not list is
  # one nobody knows is running, and a table row with no injection behind it is a promise of
  # coverage that does not exist. The three lists are the features, the harness array and the
  # documented table, and they drifted before.
  harness=scripts/mutation-harness.sh
  manifest=crates/bohr-testkit/Cargo.toml
  table=docs/TESTING.md
  if [ -f "$harness" ] && [ -f "$manifest" ] && [ -f "$table" ]; then
    in_harness=$(grep -oE '^  chaos-[a-z-]+' "$harness" | tr -d ' ' | sort -u)
    in_manifest=$(grep -oE '^chaos-[a-z-]+' "$manifest" | sort -u)
    in_table=$(grep -oE '`chaos-[a-z-]+`' "$table" | tr -d '`' | sort -u)
    if [ "$in_harness" != "$in_manifest" ]; then
      report 14n §7.4 $'the harness and the manifest list different injections:\n'"$(
        diff <(echo "$in_harness") <(echo "$in_manifest") | head -20)"
    fi
    if [ "$in_harness" != "$in_table" ]; then
      report 14n §7.4 $'the harness and TESTING.md list different injections:\n'"$(
        diff <(echo "$in_harness") <(echo "$in_table") | head -20)"
    fi
  fi
fi

exit $fail
