#!/usr/bin/env bash
# Builds an example guest statically, into a build directory beside its source.
#
# Statically, because the virtual filesystem holds what the world model puts there and a dynamic
# loader looking for libc finds nothing. Out of tree, because an example depends on whatever
# library it demonstrates and the tool's own dependency graph stays narrow (ARCHITECTURE.md 11).
set -euo pipefail

cd "$(dirname "$0")/.."
name=${1:?usage: build-example.sh <name>}
dir="examples/$name"
[ -f "$dir/Cargo.toml" ] || { echo "no example named $name" >&2; exit 2; }

# The target directory is deliberately outside the source tree: some build scripts generate paths
# longer than a mounted filesystem accepts, and a build that fails only on a developer's machine
# is a build nobody trusts.
target=${EXAMPLE_TARGET_DIR:-${TMPDIR:-/tmp}/bohr-example-$name}

# An example may ask for flags of its own — instrumentation, most often, because a guest that is
# searched rather than merely run has to say what it executed. Declared in the example rather than
# passed in, so building it by hand produces the same binary a script does.
extra=""
if [ -f "$dir/rustflags.txt" ]; then
  extra=$(grep -vE '^\s*(#|$)' "$dir/rustflags.txt" | tr '\n' ' ')
fi

# Paths of this machine are written out of the binary, so a pack's pinned digest names the program
# and not the directory someone happened to build it in.
remap="--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=$PWD=/bohr --remap-path-prefix=$target=/target"
RUSTFLAGS="-C target-feature=+crt-static $remap $extra" \
  cargo build --release --manifest-path "$dir/Cargo.toml" \
  --target x86_64-unknown-linux-gnu --target-dir "$target"

mkdir -p "$dir/build"
find "$target/x86_64-unknown-linux-gnu/release" -maxdepth 1 -type f -perm -u+x \
  ! -name '*.d' -exec cp {} "$dir/build/" \;
echo "built $name into $dir/build"
