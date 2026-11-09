#!/usr/bin/env bash
# Builds the op-reth this pack names: rust/op-reth in ethereum-optimism/optimism at the
# op-reth/v2.6.1 tag, statically, with paths of this machine written out of the binary. The
# ethereum-optimism/op-reth repository is archived and is not where op-reth is built from.
#
# Default features but `jemalloc`, which a static glibc cannot link beside its own allocator.
name=op-reth
set -euo pipefail
pack=$(cd "$(dirname "$0")" && pwd)
cd "$pack/../.."
# The compiler is part of the program: the monorepo's toolchain file names 1.95, and a patch
# release under it would be another compiler.
export RUSTUP_TOOLCHAIN=1.95.0
cargo_home=${CARGO_HOME:-$HOME/.cargo}
# Sources and the build tree live outside the repository: a client's tree is gigabytes, and a
# mounted filesystem that is slow for it is slow for nothing a pack needs.
work=${BOHR_TARGETS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/bohr-targets}/$name
mkdir -p "$work" "$pack/build"
repo=https://github.com/ethereum-optimism/optimism
tag=op-reth/v2.6.1
if [ ! -d "$work/src/.git" ]; then
  git clone --filter=blob:none --no-checkout --depth 1 --branch "$tag" "$repo" "$work/src"
fi
# Every step on every run, so a clone that stopped half way is finished rather than trusted.
git -C "$work/src" fetch --depth 1 origin "refs/tags/$tag:refs/tags/$tag" 2>/dev/null || true
git -C "$work/src" sparse-checkout set rust superchain-registry
git -C "$work/src" checkout --detach "$tag"
git -C "$work/src" submodule update --init --depth 1 superchain-registry
cd "$work/src/rust"
RUSTFLAGS="-C target-feature=+crt-static --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$work=/work" \
  cargo build --release --locked -p op-reth --bin op-reth --target x86_64-unknown-linux-gnu \
  --target-dir "$work/target" --no-default-features \
  --features otlp,reth-optimism-evm/portable,js-tracer,keccak-cache-global,asm-keccak
cp "$work/target/x86_64-unknown-linux-gnu/release/op-reth" "$pack/build/op-reth"
echo "built op-reth into $pack/build/op-reth"
