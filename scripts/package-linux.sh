#!/usr/bin/env bash
# Builds and packages the Linux release: a tarball and a .deb, each carrying
# `kgraf` (CLI) and `kgraf-app` (native shell, decision 0054), a
# .desktop entry and an icon.
#
# This script runs INSIDE a Linux environment (a container or a real Linux
# host) -- it calls apt-get, dpkg-deb and the Linux linker, none of which
# exist on the macOS host this repository is usually built on. From macOS,
# drive it through Colima:
#
#   colima start --cpu 6 --memory 12 --disk 60
#   docker run --rm \
#     -v "$PWD":/work:ro -v kgraf-linux-target:/work-target \
#     -v "$PWD/scripts/package-linux.sh":/package-linux.sh:ro \
#     -w /work rust:trixie bash /package-linux.sh
#   colima stop
#
# `rust:trixie` (Debian 13), not a pinned old point release: eframe/egui
# 0.36.2 need rustc 1.95, which is also this workspace's declared
# `rust-version` in the root Cargo.toml.
# `rust:bookworm` (Debian 12) has a new enough rustc but ships git 2.39.5,
# below the 2.42 merge-back needs (crates/kgraf-sandbox/src/merge/parse.rs
# MIN_GIT) -- fine for this build-only script, not for `cargo test`.
#
# The repository is mounted read-only: this script never writes into /work.
# All build output goes to $TARGET_DIR (a separate volume) and $DIST_DIR.
set -euo pipefail

REPO_DIR="${REPO_DIR:-/work}"
TARGET_DIR="${TARGET_DIR:-/work-target}"
DIST_DIR="${DIST_DIR:-$TARGET_DIR/dist/linux}"
STAGE_DIR="$TARGET_DIR/stage"

rust_arch="$(uname -m)"
case "$rust_arch" in
  x86_64)  rust_target="x86_64-unknown-linux-gnu"; deb_arch="amd64" ;;
  aarch64) rust_target="aarch64-unknown-linux-gnu"; deb_arch="arm64" ;;
  *) echo "package-linux.sh: unsupported arch $rust_arch" >&2; exit 1 ;;
esac
echo "packaging for $rust_target ($deb_arch)"

# --- build dependencies -----------------------------------------------
# tray-icon (libappindicator on Linux) needs GTK; winit/eframe need X11 and
# Wayland client headers; dpkg-deb builds the .deb. The icon is a copy of the
# designed asset PNG, not generated, so no rasterizer is needed here.
if ! dpkg -s libgtk-3-dev >/dev/null 2>&1; then
  apt-get update
  apt-get install -y --no-install-recommends \
    pkg-config libgtk-3-dev libayatana-appindicator3-dev \
    libxdo-dev libx11-dev libxi-dev libxrandr-dev libxcursor-dev \
    libxkbcommon-dev libwayland-dev libssl-dev libglib2.0-dev \
    dpkg-dev
fi

# --- build ---------------------------------------------------------------
rustup target add "$rust_target" >/dev/null 2>&1 || true
cd "$REPO_DIR"
cargo build --release --target "$rust_target" --target-dir "$TARGET_DIR" \
  -p kgraf-cli -p kgraf-shell

bin_dir="$TARGET_DIR/$rust_target/release"
# Every crate inherits `[workspace.package].version` from the root Cargo.toml
# (each crate's own Cargo.toml says `version.workspace = true`, not a
# literal, so grepping a crate's own file here would find nothing); read the
# one place the number actually lives.
version="$(grep -m1 '^version' "$REPO_DIR/Cargo.toml" | sed -E 's/.*"(.*)".*/\1/')"
if [[ -z "$version" ]]; then
  echo "could not read [workspace.package].version from Cargo.toml" >&2
  exit 1
fi

# --- shared payload: binaries, desktop entry, icon, LICENSE/notices --------
# LICENSE and THIRD_PARTY_NOTICES.md exist at the repository root but were never
# actually staged into either Linux artifact. `share/doc/kgraf/` is the standard
# location Debian packaging (and FHS generally) expects a package's own license/notices
# text at, so the tarball uses the same layout for consistency even though it has no
# package-manager convention of its own to satisfy.
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/bin" "$STAGE_DIR/share/applications" \
  "$STAGE_DIR/share/icons/hicolor/256x256/apps" \
  "$STAGE_DIR/share/doc/kgraf"
cp "$bin_dir/kgraf" "$STAGE_DIR/bin/kgraf"
cp "$bin_dir/kgraf-app" "$STAGE_DIR/bin/kgraf-app"
cp "$REPO_DIR/packaging/linux/kgraf.desktop" \
  "$STAGE_DIR/share/applications/kgraf.desktop"
cp "$REPO_DIR/crates/kgraf-shell/assets/app-icon/icon-256.png" \
  "$STAGE_DIR/share/icons/hicolor/256x256/apps/kgraf.png"
[[ -f "$REPO_DIR/LICENSE" ]] || {
  echo "LICENSE is missing from the repository root" >&2
  exit 1
}
[[ -f "$REPO_DIR/THIRD_PARTY_NOTICES.md" ]] || {
  echo "THIRD_PARTY_NOTICES.md is missing; run scripts/third-party-notices.sh first" >&2
  exit 1
}
# N2: this build links ported code from codex (the Linux seccomp filter) and a rerun
# re_renderer-derived shader (kgraf-galaxy); scripts/third-party-notices.sh
# appends both vendored directories' LICENSE/NOTICE text after cargo-about's own
# crates.io-dependency section -- check for it specifically, not just that the file
# exists at all.
for name in "Vendored code: ported or copied" "### codex" "### rerun-re_renderer"; do
  grep -qF "$name" "$REPO_DIR/THIRD_PARTY_NOTICES.md" || {
    echo "THIRD_PARTY_NOTICES.md is missing the vendored-code section ('$name'); regenerate with scripts/third-party-notices.sh" >&2
    exit 1
  }
done
cp "$REPO_DIR/LICENSE" "$STAGE_DIR/share/doc/kgraf/LICENSE.txt"
cp "$REPO_DIR/THIRD_PARTY_NOTICES.md" "$STAGE_DIR/share/doc/kgraf/THIRD_PARTY_NOTICES.md"

mkdir -p "$DIST_DIR"

# --- tarball ---------------------------------------------------------------
tar_name="kgraf-${version}-linux-${rust_arch}"
tar_root="$TARGET_DIR/tar-root/$tar_name"
rm -rf "$TARGET_DIR/tar-root"
mkdir -p "$tar_root"
cp -r "$STAGE_DIR/." "$tar_root/"
tar -C "$TARGET_DIR/tar-root" -czf "$DIST_DIR/${tar_name}.tar.gz" "$tar_name"
echo "tarball: $DIST_DIR/${tar_name}.tar.gz ($(du -h "$DIST_DIR/${tar_name}.tar.gz" | cut -f1))"

# --- .deb ------------------------------------------------------------------
deb_root="$TARGET_DIR/deb-root"
rm -rf "$deb_root"
mkdir -p "$deb_root/DEBIAN" "$deb_root/usr" "$deb_root/usr/bin" \
  "$deb_root/usr/share/applications" \
  "$deb_root/usr/share/icons/hicolor/256x256/apps" \
  "$deb_root/usr/share/doc/kgraf"
cp "$STAGE_DIR/bin/kgraf" "$deb_root/usr/bin/kgraf"
cp "$STAGE_DIR/bin/kgraf-app" "$deb_root/usr/bin/kgraf-app"
cp "$STAGE_DIR/share/applications/kgraf.desktop" \
  "$deb_root/usr/share/applications/kgraf.desktop"
cp "$STAGE_DIR/share/icons/hicolor/256x256/apps/kgraf.png" \
  "$deb_root/usr/share/icons/hicolor/256x256/apps/kgraf.png"
# Debian policy names a package's copyright/license file `copyright`, not `LICENSE.txt`;
# `LICENSE.txt` (matching the tarball and the other two platforms) is kept alongside it
# rather than instead of it, so all three platforms' artifacts use one consistent name.
cp "$STAGE_DIR/share/doc/kgraf/LICENSE.txt" "$deb_root/usr/share/doc/kgraf/LICENSE.txt"
cp "$STAGE_DIR/share/doc/kgraf/LICENSE.txt" "$deb_root/usr/share/doc/kgraf/copyright"
cp "$STAGE_DIR/share/doc/kgraf/THIRD_PARTY_NOTICES.md" "$deb_root/usr/share/doc/kgraf/THIRD_PARTY_NOTICES.md"
installed_size="$(du -sk "$deb_root/usr" | cut -f1)"

# Depends: computed from what the binaries actually link (ldd), not a
# hand-maintained guess -- kgraf-app pulls in GTK (tray-icon's
# libappindicator) and libxdo (tray-icon's global hotkeys) only on Linux, and
# a hand-written list silently goes stale the next time a dependency changes.
# `linux-vdso`/`ld-linux` are kernel-provided, not owned by any package; skip
# them. `dpkg -S` needs the realpath, since ldd may print a dev symlink name.
depends="libc6"
# ldd, given two files, prints a "path:" header line before each file's own
# list; skip those (they end in `:`, unlike a real "not found"/static entry).
for so in $(ldd "$STAGE_DIR/bin/kgraf" "$STAGE_DIR/bin/kgraf-app" \
    | awk '/:$/ {next} /=>/ {print $3; next} /\// {print $1}' | sort -u); do
  case "$so" in
    *linux-vdso*|*ld-linux*) continue ;;
  esac
  real="$(readlink -f "$so" 2>/dev/null || echo "$so")"
  pkg="$(dpkg -S "$real" 2>/dev/null | head -1 | cut -d: -f1)"
  [ -n "$pkg" ] && depends="$depends, $pkg"
done
depends="$(echo "$depends" | tr ',' '\n' | sed 's/^ *//' | sort -u | paste -sd, -)"

cat > "$deb_root/DEBIAN/control" <<EOF
Package: kgraf
Version: ${version}
Section: devel
Priority: optional
Architecture: ${deb_arch}
Installed-Size: ${installed_size}
Depends: ${depends}
Replaces: haustoria
Conflicts: haustoria
Provides: haustoria
Maintainer: Marco Cross
Description: Orchestrator superagent with a galaxy-style code knowledge graph.
 Local desktop harness directing worker agents across model providers.
EOF
deb_path="$DIST_DIR/kgraf_${version}_${deb_arch}.deb"
dpkg-deb --build --root-owner-group "$deb_root" "$deb_path"
echo "deb: $deb_path ($(du -h "$deb_path" | cut -f1))"

# C3: verify LICENSE/THIRD_PARTY_NOTICES.md actually landed in both artifacts, not just
# that the staging cp calls above did not error (dpkg-deb/tar could in principle drop an
# empty-looking path some other way; check the built artifacts themselves).
echo "==> Verifying LICENSE/THIRD_PARTY_NOTICES.md are present in both artifacts"
# Captured into a variable first, not `tar -tzf ... | grep -q ...` directly: under
# `set -o pipefail` (this script's own shebang line), `grep -q`'s early exit on a
# match can SIGPIPE the producer (`tar`/`dpkg-deb`) before it finishes writing, and
# pipefail then reports the whole pipeline as failed on that SIGPIPE exit status even
# though `grep` found the match -- a real, reproducible flake this exact pattern hit
# in scripts/package-windows.sh's equivalent zip-listing check during this task.
TAR_LISTING="$(tar -tzf "$DIST_DIR/${tar_name}.tar.gz")"
echo "$TAR_LISTING" | grep -q "share/doc/kgraf/LICENSE.txt" || {
  echo "tarball is missing share/doc/kgraf/LICENSE.txt" >&2
  exit 1
}
echo "$TAR_LISTING" | grep -q "share/doc/kgraf/THIRD_PARTY_NOTICES.md" || {
  echo "tarball is missing share/doc/kgraf/THIRD_PARTY_NOTICES.md" >&2
  exit 1
}
DEB_LISTING="$(dpkg-deb -c "$deb_path")"
echo "$DEB_LISTING" | grep -q "usr/share/doc/kgraf/LICENSE.txt" || {
  echo "$deb_path is missing usr/share/doc/kgraf/LICENSE.txt" >&2
  exit 1
}
echo "$DEB_LISTING" | grep -q "usr/share/doc/kgraf/THIRD_PARTY_NOTICES.md" || {
  echo "$deb_path is missing usr/share/doc/kgraf/THIRD_PARTY_NOTICES.md" >&2
  exit 1
}

echo "package-linux.sh done: $DIST_DIR"
ls -la "$DIST_DIR"
