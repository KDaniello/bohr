#!/usr/bin/env bash
# Builds the reth this pack names: paradigmxyz/reth at a pinned commit, statically, with paths of
# this machine written out of the binary, so the digest is the pack's on a host with this toolchain
# and this C library.
#
# Every default feature but two: `jit`, which needs a system LLVM and compiles contract code in
# memory, and `jemalloc`, which a static glibc cannot link beside its own allocator. The node's
# logic is the same; its allocator is glibc's, which changes how it asks for memory and nothing else.
name=reth
set -euo pipefail
pack=$(cd "$(dirname "$0")" && pwd)
cd "$pack/../.."
# The compiler is part of the program: reth carries no toolchain file, so the one this digest was
# built with is named here.
export RUSTUP_TOOLCHAIN=1.97.1
cargo_home=${CARGO_HOME:-$HOME/.cargo}
# Sources and the build tree live outside the repository: a client's tree is gigabytes, and a
# mounted filesystem that is slow for it is slow for nothing a pack needs.
work=${BOHR_TARGETS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/bohr-targets}/$name
mkdir -p "$work" "$pack/build"
repo=https://github.com/paradigmxyz/reth
commit=3641f10830f44f71c2469fdf7e818ccd4a4b0e76
if [ ! -d "$work/src/.git" ]; then
  git clone --filter=blob:none --no-checkout "$repo" "$work/src"
fi
git -C "$work/src" fetch --depth 1 origin "$commit" 2>/dev/null || true
git -C "$work/src" checkout --detach "$commit"
cd "$work/src"
RUSTFLAGS="-C target-feature=+crt-static --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$work=/work" \
  cargo build --release --locked --bin reth --target x86_64-unknown-linux-gnu \
  --target-dir "$work/target" --no-default-features \
  --features otlp,otlp-logs,reth-revm/portable,js-tracer,keccak-cache-global,asm-keccak,gmp,min-trace-logs
cp "$work/target/x86_64-unknown-linux-gnu/release/reth" "$pack/build/reth"
echo "built reth into $pack/build/reth"
