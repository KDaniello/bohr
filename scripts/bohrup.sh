#!/bin/sh
# bohrup: installs bohr from a release, checked before anything of it runs.
#
#   curl -sSf https://github.com/KDaniello/bohr/releases/latest/download/bohrup.sh | sh
#
# Fetches the static binary, the target packs and SHA256SUMS; checks the signature on SHA256SUMS,
# then every file against it; installs to ~/.bohr/bin and ~/.bohr/packs, links ~/.local/bin/bohr,
# and runs `bohr doctor`. Nothing is installed from a file that fails either check, and nothing
# but that link is written outside ~/.bohr. A host other than Linux on x86-64 is refused with the
# reason, before any download.
#
# Settings, all optional:
#   BOHR_VERSION   a release tag, v1.0.0; the latest release where unset
#   BOHR_HOME      where to install; ~/.bohr where unset
#   BOHRUP_SOURCE  where releases are fetched from: a URL, or a local directory holding one
#                  release's files, which is how the installer is tested without a network
#   BOHRUP_SIGNER  the release key, as one allowed-signers line; the one written below where unset
#
# What this does not do: edit a shell profile, install a Rust toolchain, or start anything but
# `bohr doctor`. It says what to add to PATH and stops.
set -eu

REPO_URL=https://github.com/KDaniello/bohr/releases
TARGET=x86_64-unknown-linux-musl
# The release workflow writes the release key here when it publishes this file.
SIGNER_LINE='@BOHR_RELEASE_SIGNER@'

say() { printf 'bohrup: %s\n' "$*" >&2; }
fail() { say "$*"; exit 1; }

# Everything runs from here, called on the last line: a download cut short defines a function
# and runs nothing, rather than running the half of the script that arrived.
main() {
case "$(uname -s)" in
  Linux) ;;
  *) fail "bohr runs programs under ptrace on Linux; this host is $(uname -s), and there is no build for it" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ;;
  *) fail "bohr intercepts x86-64 system calls and instructions; this host is $(uname -m), and there is no build for it" ;;
esac

for tool in tar sha256sum ssh-keygen; do
  command -v "$tool" >/dev/null 2>&1 || fail "needs $tool, which is not on PATH"
done

home=${BOHR_HOME:-$HOME/.bohr}
source=${BOHRUP_SOURCE:-$REPO_URL}
signer=${BOHRUP_SIGNER:-$SIGNER_LINE}
case "$signer" in
  @*) fail "this copy of bohrup carries no release key; use the one published with a release, or set BOHRUP_SIGNER" ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 1' INT TERM

# One file of a release, from a directory or over HTTPS.
fetch() {
  case "$source" in
    http://*|https://*)
      if command -v curl >/dev/null 2>&1; then
        curl -sSfL --proto '=https' --tlsv1.2 -o "$work/$1" "$base/$1" || fail "cannot fetch $base/$1"
      elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$work/$1" "$base/$1" || fail "cannot fetch $base/$1"
      else
        fail "needs curl or wget to fetch a release"
      fi ;;
    *) cp "$source/$1" "$work/$1" 2>/dev/null || fail "no $1 in $source" ;;
  esac
}

# The tag, which every file name carries: the one asked for, the latest release's, or for a local
# directory the one its SHA256SUMS names.
version=${BOHR_VERSION:-}
case "$source" in
  http://*|https://*)
    if [ -z "$version" ]; then
      command -v curl >/dev/null 2>&1 || fail "needs curl to find the latest release; set BOHR_VERSION"
      version=$(curl -sSfI "$REPO_URL/latest" | tr -d '\r' | grep -i '^location:' | sed 's|.*/tag/||' | tail -n 1)
      [ -n "$version" ] || fail "cannot tell which release is the latest; set BOHR_VERSION"
    fi
    base="$source/download/$version" ;;
  *)
    base=$source
    if [ -z "$version" ]; then
      version=$(sed -n "s|.*bohr-\(v[^ ]*\)-$TARGET\.tar\.gz$|\1|p" "$source/SHA256SUMS" | head -n 1)
      [ -n "$version" ] || fail "$source/SHA256SUMS names no release of bohr"
    fi ;;
esac

binary="bohr-$version-$TARGET.tar.gz"
packs="bohr-packs-$version.tar.gz"
for file in SHA256SUMS SHA256SUMS.sig "$binary" "$packs"; do
  fetch "$file"
done

# The signature first: a checksum file anybody could have written proves nothing about the files
# it lists. Then every file against it, by name, so a file the release did not list is refused.
printf '%s\n' "$signer" > "$work/allowed_signers"
ssh-keygen -Y verify -f "$work/allowed_signers" -I bohr-release -n bohr-release \
  -s "$work/SHA256SUMS.sig" < "$work/SHA256SUMS" >/dev/null 2>&1 \
  || fail "SHA256SUMS does not carry the release signature; nothing was installed"
for file in "$binary" "$packs"; do
  expected=$(awk -v f="$file" '$2 == f || $2 == "*" f { print $1 }' "$work/SHA256SUMS")
  [ -n "$expected" ] || fail "SHA256SUMS does not list $file; nothing was installed"
  actual=$(sha256sum "$work/$file" | cut -d' ' -f1)
  [ "$actual" = "$expected" ] || fail "$file is not the file the release signed; nothing was installed"
done

mkdir -p "$home/bin" "$home/packs"
tar -xzf "$work/$binary" -C "$work"
# Moved into place rather than written over, so a bohr already running keeps its own file.
cp "$work/bohr" "$home/bin/bohr.new"
chmod 755 "$home/bin/bohr.new"
mv -f "$home/bin/bohr.new" "$home/bin/bohr"
rm -rf "$home/packs.new"
mkdir -p "$home/packs.new"
tar -xzf "$work/$packs" -C "$home/packs.new"
rm -rf "$home/packs"
mv "$home/packs.new" "$home/packs"
say "installed bohr $version to $home/bin/bohr, its packs to $home/packs"

"$home/bin/bohr" doctor < /dev/null || say "bohr doctor found something this host has to change; it says what above"
# A link in ~/.local/bin, so the next command is `bohr` and not an export: Ubuntu's own profile
# puts that directory on PATH for every login shell once it exists, and no profile is edited here.
mkdir -p "$HOME/.local/bin"
ln -sf "$home/bin/bohr" "$HOME/.local/bin/bohr"
case ":$PATH:" in
  *":$HOME/.local/bin:"*|*":$home/bin:"*) say "bohr is on PATH" ;;
  *) say "linked $HOME/.local/bin/bohr, on PATH from the next login shell; in this one: export PATH=\"$HOME/.local/bin:\$PATH\"" ;;
esac
}

main "$@"
