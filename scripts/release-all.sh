#!/usr/bin/env bash
# One command, one tag, every installer.
#
# Builds macOS (.dmg), Linux aarch64+x86_64 (.tar.gz + .deb) and Windows x64
# (.msi + .zip) from one git tag, checks each artifact, writes SHA256SUMS, and
# attaches everything to a draft GitHub release. It reuses
# scripts/package-{macos,linux,windows}.sh UNMODIFIED: this script only
# supplies the clean checkout, the Colima/Docker driving for Linux, and the
# cross-artifact checks/upload those three scripts do not do themselves.
#
# Usage:
#   scripts/release-all.sh vX.Y.Z[-suffix] [--no-upload]
#
# --no-upload builds and checks everything but skips the `gh release`
# step (still writes SHA256SUMS locally).
#
# Preconditions (fail loud, named):
#   - the invoking working tree is clean (nothing to accidentally build from
#     mid-edit) and the tag exists locally (`git fetch origin --tags` first
#     if it does not).
#   - colima, docker, gh, msiinfo, wixl, cargo-xwin and the Homebrew llvm
#     keg (llvm-rc/clang-cl) are on PATH -- package-windows.sh installs the
#     xwin toolchain itself but not these.
#
# Colima/Docker are SHARED with the rest of this machine: every container and
# volume this script creates is named with the "kgraf-release-" prefix, and
# only those exact names are removed (docker rm -f / docker volume rm), on
# every exit path including failure (a trap), never `docker system prune` and
# never touching a container/volume/image this script did not create itself.
# An image this script had to pull because it was not already present locally
# is removed at the end too, and is printed either way, so nothing pulled by
# this run is left behind silently -- an image that already existed (for
# example this host's cached `rust:trixie`) is left exactly as found, even
# when a second, different-platform manifest for the same tag is fetched
# during the run (Docker repoints the one local tag at whichever platform was
# pulled last; the earlier platform's content is not deleted, just untagged,
# and this script never prunes dangling content -- see "maybe_pull_image"
# below for the exact, stated limit of what is tracked for removal).
#
# NO DEV KEY EVER SHIPS. Before anything is uploaded, every artifact (the
# tarball, the .deb's payload, the .msi's or .zip's contents, and the .dmg's
# mounted app bundle) is extracted and scanned, byte for byte, against every
# value configured in the repository's own .env (scripts/check-no-dev-keys.py
# -- see scripts/release-local.md's "Dev-key guard" section). The env file is
# read only inside that one check; this script never prints a value or a key
# name, only counts, and any count above zero is a hard, immediate failure
# before SHA256SUMS is even written, let alone before `gh release`. The tag
# checkout is also asserted to carry no `.env` of its own (it is gitignored,
# so a real one appearing there is itself a defect worth failing loudly on).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Overridable because a git worktree does not share another worktree's
# untracked files (`.env` is gitignored, so each worktree needs its own, or
# an explicit pointer at the one the owner actually keeps their dev keys in).
# Defaulting to "$ROOT/.env" is still the common case: most local release
# builds run from the repository's own primary checkout.
DEV_ENV_FILE="${KGRAF_RELEASE_ENV_FILE:-$ROOT/.env}"

STARTED_AT=$(date +%s)
CURRENT_STEP="argument parsing"

usage() {
  echo "usage: scripts/release-all.sh vX.Y.Z[-suffix] [--no-upload]" >&2
}

TAG=""
NO_UPLOAD=0
for arg in "$@"; do
  case "$arg" in
    --no-upload) NO_UPLOAD=1 ;;
    -h|--help) usage; exit 0 ;;
    -*)
      echo "unknown flag: $arg" >&2
      usage
      exit 2
      ;;
    *)
      if [[ -n "$TAG" ]]; then
        echo "unexpected extra argument: $arg" >&2
        usage
        exit 2
      fi
      TAG="$arg"
      ;;
  esac
done
if [[ -z "$TAG" ]]; then
  usage
  exit 2
fi

# ---------------------------------------------------------------------------
# Fail-loud step tracking: every "real" step of this script updates
# CURRENT_STEP right before it starts, so an ERR trap (triggered by `set -e`
# on any failing command) names exactly which step failed instead of leaving
# a bare exit code.
# ---------------------------------------------------------------------------
on_err() {
  local status=$?
  echo >&2
  echo "release-all.sh FAILED at step: $CURRENT_STEP (exit $status)" >&2
  exit "$status"
}
trap on_err ERR

# ---------------------------------------------------------------------------
# Cleanup: the temp worktree checkout, and every "kgraf-release-*" Docker
# container/volume/image this run created. Registered up front and appended
# to as things are actually created, so a failure halfway through a Linux
# build still cleans up whatever had been named/pulled by that point. This
# runs on every exit path (success, `set -e` failure, or the ERR trap above),
# via `trap ... EXIT`.
# ---------------------------------------------------------------------------
WORKTREE_DIR=""
WORK_TMP=""
RELEASE_CONTAINERS=()
RELEASE_VOLUMES=()
PULLED_IMAGES=()

cleanup() {
  local rc=$?
  echo
  echo "==> Cleanup: removing this run's kgraf-release-* containers and volumes"
  for c in "${RELEASE_CONTAINERS[@]:-}"; do
    [[ -n "$c" ]] || continue
    docker rm -f "$c" >/dev/null 2>&1 || true
  done
  for v in "${RELEASE_VOLUMES[@]:-}"; do
    [[ -n "$v" ]] || continue
    docker volume rm "$v" >/dev/null 2>&1 || true
  done
  for img in "${PULLED_IMAGES[@]:-}"; do
    [[ -n "$img" ]] || continue
    echo "    removing $img (pulled by this run; was not present before it)"
    docker rmi -f "$img" >/dev/null 2>&1 || true
  done
  if [[ -n "$WORKTREE_DIR" && -d "$WORKTREE_DIR" ]]; then
    git worktree remove --force "$WORKTREE_DIR" >/dev/null 2>&1 || rm -rf "$WORKTREE_DIR"
  fi
  if [[ -n "$WORK_TMP" && -d "$WORK_TMP" ]]; then
    rm -rf "$WORK_TMP"
  fi
  exit "$rc"
}
trap cleanup EXIT

# maybe_pull_image IMAGE PLATFORM
# Pulls IMAGE for PLATFORM. If IMAGE was not present under any platform
# before this call, it is pulled and tracked for removal at the end. If it
# was already present (any platform), the requested platform is still
# pulled/refreshed so the right architecture is available, but the tag is
# left alone at cleanup -- it pre-existed this run. Tracked, not deduplicated
# per platform: see this script's header comment for the stated limit.
maybe_pull_image() {
  local image="$1" platform="$2"
  if docker image inspect "$image" >/dev/null 2>&1; then
    echo "    $image already present locally; pulling $platform variant if needed"
    docker pull --platform "$platform" "$image" >/dev/null
  else
    echo "    $image not present locally; pulling $platform (will be removed at the end)"
    docker pull --platform "$platform" "$image" >/dev/null
    PULLED_IMAGES+=("$image")
  fi
}

# ---------------------------------------------------------------------------
# Preconditions
# ---------------------------------------------------------------------------
CURRENT_STEP="clean working tree check"
if [[ -n "$(git status --porcelain)" ]]; then
  echo "the working tree at $ROOT is not clean; commit or discard changes before releasing" >&2
  exit 1
fi

CURRENT_STEP="tag existence check"
if ! git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "tag $TAG does not exist locally; run 'git fetch origin --tags' first" >&2
  exit 1
fi
TAG_COMMIT="$(git rev-parse "refs/tags/$TAG^{commit}")"
echo "==> Releasing $TAG ($TAG_COMMIT)"

CURRENT_STEP="required tools check"
for tool in git colima docker gh msiinfo wixl msiextract cargo shasum python3 unzip; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "required tool not found on PATH: $tool" >&2
    exit 1
  }
done
if ! colima status >/dev/null 2>&1; then
  echo "colima is not running; 'colima start' first (it is shared -- do not stop it for other sessions without asking)" >&2
  exit 1
fi

CURRENT_STEP="dev-key env file check"
if [[ ! -f "$DEV_ENV_FILE" ]]; then
  echo "dev-key guard env file not found: $DEV_ENV_FILE" >&2
  echo "this release cannot proceed without a source of truth for 'must never ship' values;" >&2
  echo "set KGRAF_RELEASE_ENV_FILE to point at the .env that holds the development-only keys" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Clean checkout: a `git worktree`, not this (or any other) working tree that
# could change mid-build. Shares this repository's .git (object store,
# refs), so no network fetch is needed for a tag already fetched locally.
#
# Deliberately NOT under the system temp dir ($TMPDIR, /private/var/folders/...
# on macOS): Colima's default profile only bind-mounts $HOME into its VM (see
# `mounts: []` / "Colima default behaviour: $HOME is mounted as writable" in
# `colima.yaml`), so a `docker run -v /private/var/folders/...:/work` mount
# resolves to an empty directory inside the VM -- this is exactly what made
# the first real run of this script fail with "package-linux.sh: No such
# file or directory" even though the file plainly exists on the host.
# `target/release-work/` (gitignored, scratch-only) lives under $HOME by
# construction, so it is visible to Colima the same way the rest of this
# repository is.
# ---------------------------------------------------------------------------
CURRENT_STEP="creating clean worktree checkout of $TAG"
mkdir -p "$ROOT/target/release-work"
WORK_TMP="$(mktemp -d "$ROOT/target/release-work/release.XXXXXX")"
WORKTREE_DIR="$WORK_TMP/checkout"
git worktree add --detach --quiet "$WORKTREE_DIR" "$TAG"
echo "==> Building from $WORKTREE_DIR (detached at $TAG_COMMIT)"

CURRENT_STEP="checking the tag checkout carries no .env"
if [[ -e "$WORKTREE_DIR/.env" ]]; then
  echo ".env is gitignored and must never be part of a tagged checkout, but one exists at $WORKTREE_DIR/.env" >&2
  exit 1
fi
echo "==> Confirmed: no .env in the $TAG checkout"

DIST_ROOT="$WORK_TMP/release-dist"
LOG_DIR="$WORK_TMP/logs"
SCAN_TMP="$WORK_TMP/dev-key-scan"
mkdir -p "$DIST_ROOT" "$LOG_DIR" "$SCAN_TMP"

# scan_for_dev_keys LABEL TARGET...
# Runs scripts/check-no-dev-keys.py against one or more already-extracted
# targets, prints its (count-only) report, and adds its exit status to
# DEV_KEY_SCAN_FAILED so every artifact gets scanned and reported even if an
# earlier one already failed -- one combined, complete report, not a stop at
# the first hit.
DEV_KEY_SCAN_FAILED=0
scan_for_dev_keys() {
  local label="$1"
  shift
  echo "==> [dev-key guard] scanning $label"
  if ! python3 "$ROOT/scripts/check-no-dev-keys.py" --env-file "$DEV_ENV_FILE" "$@"; then
    echo "    ^ $label: dev-key guard FAILED (see counts above)" >&2
    DEV_KEY_SCAN_FAILED=1
  fi
}

CURRENT_STEP="scripts/check-release-version.sh"
( cd "$WORKTREE_DIR" && ./scripts/check-release-version.sh )
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$WORKTREE_DIR/Cargo.toml" | head -1)"
echo "==> Workspace version: $VERSION"

ALL_ARTIFACTS=()

# ---------------------------------------------------------------------------
# macOS: universal .dmg, built natively (this host is macOS).
# ---------------------------------------------------------------------------
CURRENT_STEP="scripts/package-macos.sh"
echo
echo "==> [macOS] building (unmodified scripts/package-macos.sh)"
( cd "$WORKTREE_DIR" && ./scripts/package-macos.sh ) 2>&1 | tee "$LOG_DIR/macos.log"

MACOS_DMG="$WORKTREE_DIR/target/package/macos/dist/kGraf-$VERSION-macos-universal.dmg"
[[ -f "$MACOS_DMG" ]] || { echo "expected macOS artifact missing: $MACOS_DMG" >&2; exit 1; }

CURRENT_STEP="macOS .dmg version check (independent mount)"
echo "==> [macOS] independently mounting the .dmg and checking the bundle version"
MACOS_MOUNT="$(mktemp -d "$WORK_TMP/macos-mount.XXXXXX")"
hdiutil attach "$MACOS_DMG" -mountpoint "$MACOS_MOUNT" -nobrowse -quiet
MACOS_APP="$MACOS_MOUNT/kGraf.app"
[[ -d "$MACOS_APP" ]] || { hdiutil detach "$MACOS_MOUNT" -quiet; echo "kGraf.app not found inside the mounted .dmg" >&2; exit 1; }
MACOS_PLIST_VERSION="$(plutil -extract CFBundleShortVersionString raw "$MACOS_APP/Contents/Info.plist")"

CURRENT_STEP="dev-key guard: macOS .app bundle"
# Scanned while still mounted, straight off the .dmg's own filesystem -- the
# most literal reading of "the .dmg's app bundle" this task asked for, not a
# copy of the staged pre-dmg directory.
scan_for_dev_keys "macOS .app bundle (mounted from the .dmg)" "$MACOS_APP"

hdiutil detach "$MACOS_MOUNT" -quiet
rmdir "$MACOS_MOUNT" 2>/dev/null || true
if [[ "$MACOS_PLIST_VERSION" != "$VERSION" ]]; then
  echo "macOS .dmg bundle version ($MACOS_PLIST_VERSION) does not match workspace version ($VERSION)" >&2
  exit 1
fi
echo "    CFBundleShortVersionString: $MACOS_PLIST_VERSION (matches)"

cp "$MACOS_DMG" "$DIST_ROOT/"
ALL_ARTIFACTS+=("$DIST_ROOT/$(basename "$MACOS_DMG")")

# ---------------------------------------------------------------------------
# Linux: aarch64 native, x86_64 under QEMU user-mode emulation (binfmt_misc),
# both inside Colima -- this is a compile+package step, not the seccomp/
# Landlock syscall-semantics testing that needs real x86_64 hardware
# (scripts/run-x86_64-sandbox-tests.sh is the script for that; not this one).
# ---------------------------------------------------------------------------
LINUX_DIST="$DIST_ROOT/linux"
mkdir -p "$LINUX_DIST"

# A `case`, not `declare -A`: this script's shebang resolves to whatever
# `bash` is first on PATH, and on a stock macOS host that is still /bin/bash
# 3.2.57 (Apple ships nothing newer over its GPLv3 license change) -- which
# has no associative arrays at all. A case statement is the portable
# equivalent and needs nothing newer than bash 3.
for arch in aarch64 amd64; do
  case "$arch" in
    aarch64) platform="linux/arm64"; uname_m="aarch64"; deb_arch="arm64" ;;
    amd64)   platform="linux/amd64"; uname_m="x86_64";  deb_arch="amd64" ;;
    *) echo "release-all.sh: unhandled arch $arch" >&2; exit 1 ;;
  esac
  container="kgraf-release-linux-$arch"
  volume="kgraf-release-linux-target-$arch"

  CURRENT_STEP="Linux $arch: docker volume create"
  echo
  echo "==> [Linux $arch] building in Colima ($platform, rust:trixie, scripts/package-linux.sh unmodified)"
  docker volume create "$volume" >/dev/null
  RELEASE_VOLUMES+=("$volume")

  CURRENT_STEP="Linux $arch: cargo build via package-linux.sh"
  RELEASE_CONTAINERS+=("$container")
  docker run --rm --name "$container" --platform "$platform" \
    -v "$WORKTREE_DIR":/work:ro -v "$volume":/work-target \
    -w /work rust:trixie bash /work/scripts/package-linux.sh \
    2>&1 | tee "$LOG_DIR/linux-$arch-build.log"

  CURRENT_STEP="Linux $arch: copying artifacts out of the build volume"
  # A plain filesystem copy, not a target-arch binary run -- deliberately
  # left off --platform (defaults to this host's native arm64, whose
  # rust:trixie image is already cached) so this step never needs to pull or
  # emulate anything, whichever arch's volume it is copying out of.
  docker run --rm --name "kgraf-release-linux-copy-$arch" \
    -v "$volume":/src:ro -v "$LINUX_DIST":/dst \
    rust:trixie bash -c 'cp -r /src/dist/linux/. /dst/'

  tarball="$LINUX_DIST/kgraf-${VERSION}-linux-${uname_m}.tar.gz"
  deb="$LINUX_DIST/kgraf_${VERSION}_${deb_arch}.deb"
  [[ -f "$tarball" ]] || { echo "expected Linux artifact missing: $tarball" >&2; exit 1; }
  [[ -f "$deb" ]] || { echo "expected Linux artifact missing: $deb" >&2; exit 1; }

  CURRENT_STEP="Linux $arch: verifying in a clean debian:trixie-slim container"
  echo "==> [Linux $arch] verifying --version/--probe in a fresh debian:trixie-slim container"
  maybe_pull_image debian:trixie-slim "$platform"
  verify_container="kgraf-release-linux-verify-$arch"
  RELEASE_CONTAINERS+=("$verify_container")
  docker run --rm --name "$verify_container" --platform "$platform" \
    -e VERSION="$VERSION" \
    -v "$LINUX_DIST":/artifacts:ro \
    debian:trixie-slim bash -c '
      set -euo pipefail
      apt-get update -qq
      # Mesa (llvmpipe/lavapipe software rendering) plus the tray/X11 shared
      # libraries the shell links -- same package list the 2026-09-28
      # x86_64 verification run used; the tarball ships no
      # runtime dependencies of its own.
      apt-get install -y -qq --no-install-recommends \
        libgtk-3-0 libayatana-appindicator3-1 libxdo3 mesa-utils \
        libgl1-mesa-dri libvulkan1 >/dev/null
      mkdir /check
      tar -xzf /artifacts/'"$(basename "$tarball")"' -C /check
      root="$(find /check -mindepth 1 -maxdepth 1 -type d)"
      out="$("$root/bin/kgraf" --version)"
      [[ "$out" == "kgraf $VERSION" ]] || { echo "tarball kgraf --version mismatch: got '"'"'$out'"'"', want '"'"'kgraf $VERSION'"'"'" >&2; exit 1; }
      "$root/bin/kgraf" providers >/dev/null
      "$root/bin/kgraf-app" --probe
      apt-get install -y -qq /artifacts/'"$(basename "$deb")"' >/dev/null
      out="$(kgraf --version)"
      [[ "$out" == "kgraf $VERSION" ]] || { echo ".deb kgraf --version mismatch: got '"'"'$out'"'"', want '"'"'kgraf $VERSION'"'"'" >&2; exit 1; }
    ' 2>&1 | tee "$LOG_DIR/linux-$arch-verify.log"
  echo "    kgraf --version and --probe passed for $arch (tarball and .deb)"

  CURRENT_STEP="dev-key guard: Linux $arch tarball"
  tar_extract="$SCAN_TMP/tarball-$arch"
  mkdir -p "$tar_extract"
  tar -xzf "$tarball" -C "$tar_extract"
  scan_for_dev_keys "Linux $arch tarball" "$tar_extract"

  CURRENT_STEP="dev-key guard: Linux $arch .deb"
  # dpkg-deb does not exist on macOS; extraction is a filesystem operation
  # (no target-arch code runs), so the default, already-cached, native arm64
  # rust:trixie image does it, same as the copy-out step above.
  deb_extract="$SCAN_TMP/deb-$arch"
  mkdir -p "$deb_extract"
  docker run --rm --name "kgraf-release-linux-deb-extract-$arch" \
    -v "$deb":/pkg.deb:ro -v "$deb_extract":/out \
    rust:trixie dpkg-deb -x /pkg.deb /out
  scan_for_dev_keys "Linux $arch .deb payload" "$deb_extract"

  ALL_ARTIFACTS+=("$tarball" "$deb")
done

# ---------------------------------------------------------------------------
# Windows: cross-built natively on this macOS host with cargo-xwin (no
# container -- package-windows.sh itself is what drives xwin/wixl).
# ---------------------------------------------------------------------------
CURRENT_STEP="scripts/package-windows.sh"
echo
echo "==> [Windows] building (unmodified scripts/package-windows.sh)"
( cd "$WORKTREE_DIR" && ./scripts/package-windows.sh ) 2>&1 | tee "$LOG_DIR/windows.log"

WIN_MSI="$WORKTREE_DIR/target/package/windows/dist/kGraf-$VERSION-windows-x86_64.msi"
WIN_ZIP="$WORKTREE_DIR/target/package/windows/dist/kGraf-$VERSION-windows-x86_64.zip"
[[ -f "$WIN_ZIP" ]] || { echo "expected Windows artifact missing: $WIN_ZIP" >&2; exit 1; }

CURRENT_STEP="Windows .msi version check (msiinfo)"
if [[ -f "$WIN_MSI" ]]; then
  echo "==> [Windows] checking the .msi's ProductVersion with msiinfo"
  # `msiinfo export` writes CRLF line endings (verified with `od -c`: a real
  # "\r\n", not a terminal-display artifact), so a bare awk field extraction
  # leaves a trailing \r on $2 -- invisible when printed, but never equal to
  # a bash string literal. `tr -d '\r'` strips it before the comparison.
  msi_version="$(msiinfo export "$WIN_MSI" Property | tr -d '\r' | awk -F'\t' '$1=="ProductVersion" {print $2}')"
  expected="${VERSION}.0"
  if [[ "$msi_version" != "$expected" ]]; then
    echo "MSI ProductVersion ($msi_version) does not match the expected $expected" >&2
    exit 1
  fi
  echo "    ProductVersion: $msi_version (matches)"

  CURRENT_STEP="dev-key guard: Windows .msi"
  msi_extract="$SCAN_TMP/msi"
  mkdir -p "$msi_extract"
  msiextract -C "$msi_extract" "$WIN_MSI" >/dev/null
  scan_for_dev_keys "Windows .msi contents" "$msi_extract"

  cp "$WIN_MSI" "$DIST_ROOT/"
  ALL_ARTIFACTS+=("$DIST_ROOT/$(basename "$WIN_MSI")")
else
  echo "NOTICE: no .msi was built (wixl/msiinfo missing on this host); shipping the .zip only." >&2
fi

CURRENT_STEP="dev-key guard: Windows .zip"
zip_extract="$SCAN_TMP/zip"
mkdir -p "$zip_extract"
unzip -q -o "$WIN_ZIP" -d "$zip_extract"
scan_for_dev_keys "Windows .zip contents" "$zip_extract"

cp "$WIN_ZIP" "$DIST_ROOT/"
ALL_ARTIFACTS+=("$DIST_ROOT/$(basename "$WIN_ZIP")")

# ---------------------------------------------------------------------------
# Dev-key guard verdict: every artifact above has now been scanned. Fail
# loud, before SHA256SUMS is even written, if any of them matched.
# ---------------------------------------------------------------------------
CURRENT_STEP="dev-key guard verdict"
if [[ "$DEV_KEY_SCAN_FAILED" -ne 0 ]]; then
  echo >&2
  echo "DEV-KEY GUARD FAILED: at least one artifact contains a value from $DEV_ENV_FILE." >&2
  echo "See the per-artifact counts printed above (counts only -- never the value or key name)." >&2
  echo "Nothing is uploaded. Fix the leak, then run this script again from a clean tag." >&2
  exit 1
fi
echo
echo "==> [dev-key guard] all artifacts clean: 0 matches against $(basename "$DEV_ENV_FILE") in every one"

# ---------------------------------------------------------------------------
# SHA256SUMS over everything that will be attached.
# ---------------------------------------------------------------------------
CURRENT_STEP="writing SHA256SUMS"
echo
echo "==> Writing SHA256SUMS"
# One line per artifact, keyed by the basename only: a GitHub release asset
# has no directory structure (the Linux artifacts live under $DIST_ROOT/linux
# on disk, purely to keep this script's own layout tidy), so SHA256SUMS must
# name files the way a person downloading the release actually sees them.
SUMS_FILE="$DIST_ROOT/SHA256SUMS"
: > "$SUMS_FILE"
for a in "${ALL_ARTIFACTS[@]}"; do
  ( cd "$(dirname "$a")" && shasum -a 256 "$(basename "$a")" ) >> "$SUMS_FILE"
done
sort -k2 -o "$SUMS_FILE" "$SUMS_FILE"
cat "$SUMS_FILE"
ALL_ARTIFACTS+=("$SUMS_FILE")

# ---------------------------------------------------------------------------
# Draft GitHub release.
# ---------------------------------------------------------------------------
RELEASE_URL=""
RELEASE_REPO="${KGRAF_RELEASE_REPO:-Marceux-Croix/kGraf}"
if [[ "$NO_UPLOAD" -eq 1 ]]; then
  echo
  echo "==> --no-upload: skipping the GitHub release step"
else
  CURRENT_STEP="gh release (draft only, never published)"
  echo
  echo "==> Attaching artifacts to a draft release for $TAG (draft only; never published, never marked latest)"
  # Releases go to the public repository, where the in-app update check
  # (crates/kgraf-shell/src/update_check.rs) looks for them. The tag must
  # already exist there: run scripts/export-public.sh for this tag first.
  if gh release view "$TAG" --repo "$RELEASE_REPO" >/dev/null 2>&1; then
    echo "    release $TAG already exists in $RELEASE_REPO; uploading (--clobber replaces same-named assets)"
    gh release upload "$TAG" "${ALL_ARTIFACTS[@]}" --repo "$RELEASE_REPO" --clobber
  else
    gh release create "$TAG" "${ALL_ARTIFACTS[@]}" \
      --repo "$RELEASE_REPO" \
      --verify-tag \
      --draft \
      --title "kGraf $VERSION ($TAG)" \
      --notes "Unsigned on all three platforms. Draft only; not published, not marked latest."
  fi
  RELEASE_URL="$(gh release view "$TAG" --repo "$RELEASE_REPO" --json url -q .url)"
  echo "    draft release: $RELEASE_URL"
fi

# ---------------------------------------------------------------------------
# Report.
# ---------------------------------------------------------------------------
ENDED_AT=$(date +%s)
ELAPSED=$(( ENDED_AT - STARTED_AT ))
echo
echo "==> release-all.sh done for $TAG ($VERSION) in $((ELAPSED/60))m $((ELAPSED%60))s"
echo "    artifacts in $DIST_ROOT:"
for a in "${ALL_ARTIFACTS[@]}"; do
  echo "      $(basename "$a")  $(du -h "$a" | cut -f1)"
done
if [[ -n "$RELEASE_URL" ]]; then
  echo "    draft release: $RELEASE_URL"
fi
