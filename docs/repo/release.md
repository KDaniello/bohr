# Releases

## Versioning

The tool is SemVer. Three further numbers version **independently** and are printed by
`bohr version --verbose` and recorded in every bundle ([`API.md`](../API.md) §9):

| Number | Bumped when | Effect on stored bundles |
|---|---|---|
| `tool_version` | any release | none |
| `tape_abi` | the draw ABI or the extension keystream changes | **all previous bundles become unreplayable.** A major release only, and it is announced in the release notes as the headline |
| `bundle_format` | sections added, removed or re-encoded | older bundles still read; unknown sections are preserved |
| `sdk_schema` | a record field is added or changes meaning | additive only |

The rule under all of this: **an upgrade never silently changes what a stored bundle means.** It
replays it identically, or it refuses and names the version difference. A finding saved six months
ago is evidence, and evidence that quietly re-interprets itself is no longer evidence.

## Pre-1.0

Minor versions may break the CLI and configuration. They may **not** break bundle replay without a
`tape_abi` bump, and every break is in the release notes with the migration.

## Checklist

1. Nightly green for three consecutive days: `portability`, `hunting`, `perf`, `fuzz-long`.
2. `make pr` green on the full CI matrix ([`PLATFORMS.md`](../PLATFORMS.md) §6).
3. Every bundle in `testdata/bundles/` replays with the release build — this is the
   backward-compatibility gate and it is not skippable.
4. The reproducible-build job produces identical hashes on two different runners.
5. `CHANGELOG.md` written: what changed, what breaks, what to do about it.
6. If `tape_abi` moved: a **loud** section at the top of the notes, plus `bohr version` explaining
   the incompatibility in plain words when it refuses a bundle.
7. Documentation matches the release: `docs/` updated, the `docs` CI job green.
8. Tag `vX.Y.Z`, push. The release workflow builds the artefacts below, signs `SHA256SUMS`, and
   publishes them with the container image.
9. Verify the published artefacts from a clean machine: `bohrup` installs, `bohr doctor` passes,
   `bohr run --repeat 100` on the demo target is identical.

Step 9 is done by hand, every time. An install script broken in packaging is otherwise discovered
by a first-time user.

## What a release is

Not a binary but five files, and an image built from them:

| File | What it is |
|---|---|
| `bohr-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the static binary, built with the `dist` profile: release code, symbol names kept, line tables stripped |
| `bohr-packs-vX.Y.Z.tar.gz` | `targets/`, `examples/`, `sdk/c/`, `scripts/build-example.sh` and `rust-toolchain.toml`, so `bohr hunt <name>` works off a checkout |
| `SHA256SUMS` | the two archives' hashes |
| `SHA256SUMS.sig` | an `ssh-keygen -Y sign` signature over `SHA256SUMS`, namespace `bohr-release` |
| `bohrup.sh` | the installer, with the release key written into it |

The image is `ghcr.io/kdaniello/bohr:vX.Y.Z`, built from those files and not from source.

**The signature is over the sums, and the sums are over the files.** `bohrup` refuses to install
from a `SHA256SUMS` without the signature, and a file the sums do not list. The key is
`ssh-keygen -Y` rather than anything a user installs first, because every host the installer
runs on has OpenSSH. The private key is the repository secret `BOHR_RELEASE_KEY`, its public line
the variable `BOHR_RELEASE_SIGNER` (`bohr-release namespaces="bohr-release" ssh-ed25519 …`); a tag
in a repository without them fails the build rather than publishing a release nothing can verify.

**A dispatch is the whole release without the publishing.** `workflow_dispatch` runs every job a
tag runs — the two-runner hash comparison, the signature where the key exists, an install of what
was built into a scratch home, the image built and started — and keeps the result as workflow
artefacts. Only a `v*` tag pushes the image or writes a release page.

## Reproducing a build

Anyone can rebuild a released binary and get the same bytes. This is checked on every tag by two
runners on different distributions, and it is the reason a checksum is worth publishing at all: a
hash you cannot independently arrive at only proves the file did not change in transit.

```bash
git checkout vX.Y.Z
export SOURCE_DATE_EPOCH=$(git log -1 --pretty=%ct)
export RUSTFLAGS="--remap-path-prefix=$PWD=/bohr --remap-path-prefix=$HOME/.cargo=/cargo"
cargo build --profile dist --locked --target x86_64-unknown-linux-musl --bin bohr
sha256sum target/x86_64-unknown-linux-musl/dist/bohr
```

Three things and no more are pinned: the toolchain (`rust-toolchain.toml`), the dependency versions
(`--locked`), and the two ways the machine would otherwise sign the binary — the clock, and the
paths of the checkout and the crate cache, which reach the output through debug information and
panic messages. The tarball is packed with the same timestamp, fixed ownership and sorted entries,
so the archive reproduces as well as the binary inside it.

A hash that differs is a bug report we want: it means something about the machine reached the
artefact, which is the same class of defect this tool exists to find in other people's programs.

## Release notes

Written for someone deciding whether to upgrade, in this order:

**Breaking** (bundle compatibility first, then CLI, then configuration) · **Findings** — bugs this
release can find that the previous one could not · **Performance**, with numbers and the workload ·
**Fixed** · **Determinism** — every source moved to `solved` in
[`DETERMINISM.md`](../DETERMINISM.md).

No "comprehensive", no "robust", no "enhanced": the notes state what changed and what it means.

## SDK releases

SDKs version independently of the tool and declare the minimum tool version they need. A mismatch is
a startup diagnostic naming both versions, never a mysterious failure later
([`API.md`](../API.md) §6).

An SDK release is blocked on the conformance suite passing in **all five languages**, including the
languages the change did not touch: a shared contract is checked as one thing.

## Security releases

Out of band, following [`SECURITY.md`](../../SECURITY.md). Bohr is not a security sandbox and says
so, so "security release" here means: a vulnerability in our own parsers (bundle, tape, SDK records
— all of which read untrusted input), or a bug that makes the tool report a nondeterministic run as
deterministic.

The second is a security issue in the sense that applies here: a false assurance leaves the caller
worse informed than no tool at all would.
