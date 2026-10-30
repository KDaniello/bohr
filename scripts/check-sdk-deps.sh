#!/usr/bin/env bash
# SDKs are linked into other people's builds. Every dependency we add there is a version
# conflict in a customer's project and a support ticket for us. See docs/ARCHITECTURE.md 9 S3.
set -uo pipefail

cd "$(dirname "$0")/.."

fail=0
report() { printf '\033[31mDEPENDENCY\033[0m %s\n' "$1" >&2; fail=1; }

# Rust: only std plus bohr-sdk-proto.
if [ -f sdk/rust/Cargo.toml ]; then
  deps=$(awk '/^\[dependencies\]/{f=1;next} /^\[/{f=0} f && /=/ {print $1}' sdk/rust/Cargo.toml)
  for d in $deps; do
    [ "$d" = "bohr-sdk-proto" ] || report "sdk/rust depends on $d"
  done
fi

# Python: no install_requires beyond the standard library.
if [ -f sdk/python/pyproject.toml ] &&
   grep -qE '^\s*dependencies\s*=\s*\[\s*[^]]' sdk/python/pyproject.toml; then
  report "sdk/python declares runtime dependencies"
fi

# C#: no PackageReference. Searched at any depth, because a project that sits one directory
# further down than the check looks is a check that passes by missing its subject.
if find sdk/csharp -name '*.csproj' 2>/dev/null | grep -q . &&
   find sdk/csharp -name '*.csproj' -exec grep -l 'PackageReference' {} + 2>/dev/null | grep -q .; then
  report "sdk/csharp declares a PackageReference"
fi

# Go: no require block beyond the module itself.
if [ -f sdk/go/go.mod ] && grep -qE '^\s*require\s' sdk/go/go.mod; then
  report "sdk/go declares a require"
fi

# C: header-only plus a static library; no linked third party.
if [ -f sdk/c/Makefile ] && grep -qE '^\s*LDLIBS\s*[:+]?=\s*\S' sdk/c/Makefile; then
  report "sdk/c links a third-party library"
fi

exit $fail
