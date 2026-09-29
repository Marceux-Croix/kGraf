#!/usr/bin/env bash
# Asserts the workspace's Cargo version matches the latest git tag's numeric part
# (MAJOR.MINOR.PATCH), so `kgraf --version`, the packaged bundles
# (scripts/package-{macos,linux,windows}.sh, all of which read the same
# [workspace.package].version) and the tag history never drift apart again the way
# 0.1.0 vs v0.7.x did (see scripts/release-local.md's "Version alignment" section).
#
# Not a `cargo test`: the version-tag relationship is a fact about this git checkout
# (its tag history), not about the compiled crate, and a `cargo test` binary has no
# reliable way to see tags -- a published crate has no `.git` at all, and even here a
# shallow or tag-less CI checkout would make a test that shells out to `git tag` flake
# on missing history rather than fail on a real mismatch. A script, run deliberately
# before a release (or by CI with full history), is the honest place for this check.
#
# Usage: scripts/check-release-version.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
if [[ -z "$cargo_version" ]]; then
  echo "could not read [workspace.package].version from Cargo.toml" >&2
  exit 1
fi

if ! [[ "$cargo_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Cargo version '$cargo_version' is not MAJOR.MINOR.PATCH" >&2
  exit 1
fi

# Every tag's numeric MAJOR.MINOR.PATCH, stripping the leading `v` and any
# trailing `-suffix` (`-wip`, `-wip-macos-preview`, `-partial`, ...): a `-wip`
# tag still names a real version in progress, and this check cares about the
# number, not the suffix. `sort -V` orders numerically (0.7.2 after 0.6.10),
# unlike a plain lexical sort.
latest_tag_version="$(
  git tag --list 'v*' \
    | sed -n 's/^v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p' \
    | sort -V \
    | tail -1
)"
if [[ -z "$latest_tag_version" ]]; then
  echo "no git tag of the form vMAJOR.MINOR.PATCH[-suffix] was found; nothing to check the Cargo version against" >&2
  exit 1
fi

if [[ "$cargo_version" != "$latest_tag_version" ]]; then
  echo "Cargo version ($cargo_version) does not match the latest tag's numeric part ($latest_tag_version)" >&2
  echo "bump [workspace.package].version in Cargo.toml, or confirm the newest tag is the intended release" >&2
  exit 1
fi

echo "Cargo version $cargo_version matches the latest tag's numeric part."
