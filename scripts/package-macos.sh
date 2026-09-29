#!/usr/bin/env bash
# Builds a universal (arm64 + x86_64) macOS .app and .dmg for kGraf, locally.
#
# No Apple Developer ID exists yet, so signing is ad-hoc and notarization is wired but
# inactive (fail loud: it prints that it is skipped, it never skips silently). This is
# the local release path (see scripts/release-local.md, which this script implements
# end to end for macOS).
#
# Usage:
#   scripts/package-macos.sh            # build + sign (ad-hoc) + dmg + verify
#   scripts/package-macos.sh --skip-verify   # skip the mount/launch verification step
#
# Notarization (inactive by default): set KGRAF_DEVELOPER_ID_IDENTITY to a
# "Developer ID Application: ..." codesign identity name, and either
# KGRAF_NOTARY_PROFILE (a `xcrun notarytool store-credentials` keychain profile
# name) or the trio KGRAF_APPLE_ID / KGRAF_APP_SPECIFIC_PASSWORD /
# KGRAF_TEAM_ID. Without these, the script signs ad-hoc and prints, loudly, that
# notarization was skipped -- it never skips in silence (SR "fail loud").

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Cleans up the verification mount/copy on every exit path, including a launch-probe
# failure or any earlier `set -e` abort -- not just the success path. Both variables
# start empty so an early failure (before the verify section runs) makes this a no-op.
MOUNT_DIR=""
TEMP_APPS=""
cleanup() {
  if [[ -n "$MOUNT_DIR" ]]; then
    hdiutil detach "$MOUNT_DIR" -quiet 2>/dev/null || true
    rmdir "$MOUNT_DIR" 2>/dev/null || true
  fi
  if [[ -n "$TEMP_APPS" ]]; then
    rm -rf "$TEMP_APPS"
  fi
}
trap cleanup EXIT

SKIP_VERIFY=0
for arg in "$@"; do
  case "$arg" in
    --skip-verify) SKIP_VERIFY=1 ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

# Decision 0062: these variables were HAUSTORIA_* before the rename. A value set only
# under the former name is used, with a warning naming the new one; one set under both
# names uses the new one and says the former is ignored (fail loud, never silent).
for suffix in DEVELOPER_ID_IDENTITY NOTARY_PROFILE APPLE_ID APP_SPECIFIC_PASSWORD TEAM_ID BUNDLE_ID; do
  new_var="KGRAF_$suffix"
  old_var="HAUSTORIA_$suffix"
  if [[ -z "${!new_var:-}" && -n "${!old_var:-}" ]]; then
    echo "warning: using $old_var; rename it to $new_var (Haustoria was renamed kGraf, decision 0062)" >&2
    export "$new_var=${!old_var}"
  elif [[ -n "${!new_var:-}" && -n "${!old_var:-}" ]]; then
    echo "warning: $old_var is ignored because $new_var is set (decision 0062)" >&2
  fi
done

APP_NAME="kGraf"
CLI_NAME="kgraf"
SHELL_BIN="kgraf-app"
# Reverse-DNS bundle id. No registered domain or Developer ID exists yet,
# so this is a placeholder scoped to the repository owner's GitHub
# handle; change it here (or via KGRAF_BUNDLE_ID) once a real one is assigned --
# Gatekeeper and TCC both key permission grants off this string, so changing it later
# resets any such grants on machines that already ran an earlier build.
# It was com.marceux-croix.haustoria before the rename (decision 0062), which did exactly
# that once: TCC grants given to Haustoria.app are not carried over, and kGraf.app is a
# new bundle beside an installed Haustoria.app, not a replacement for it. Nothing else
# keys off the id: the app is not sandboxed (no container), keeps nothing in
# NSUserDefaults, and its settings and logs move by name at first launch
# (crates/kgraf-shell/src/app_dirs.rs).
BUNDLE_ID="${KGRAF_BUNDLE_ID:-com.marceux-croix.kgraf}"

# Version comes from Cargo, not hand-maintained here (task requirement). Every
# crate inherits `[workspace.package].version` from the root Cargo.toml, so
# reading it there (rather than any one crate's Cargo.toml, which now says
# `version.workspace = true`, not a literal) covers kgraf-shell,
# kgraf-cli, and everything else in lockstep.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
if [[ -z "$VERSION" ]]; then
  echo "could not read [workspace.package].version from Cargo.toml" >&2
  exit 1
fi
echo "==> Packaging kGraf $VERSION for macOS (universal)"

BUILD_ROOT="$ROOT/target/package/macos"
STAGE_DIR="$BUILD_ROOT/stage"
DIST_DIR="$BUILD_ROOT/dist"
APP_DIR="$STAGE_DIR/$APP_NAME.app"
ICONSET_DIR="$BUILD_ROOT/AppIcon.iconset"
rm -rf "$STAGE_DIR" "$ICONSET_DIR"
mkdir -p "$STAGE_DIR" "$DIST_DIR" "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources/bin" "$ICONSET_DIR"

ARM_TARGET="aarch64-apple-darwin"
X86_TARGET="x86_64-apple-darwin"

echo "==> Ensuring both macOS Rust targets are installed"
rustup target add "$ARM_TARGET" >/dev/null
rustup target add "$X86_TARGET" >/dev/null

echo "==> Building $SHELL_BIN and $CLI_NAME for $ARM_TARGET and $X86_TARGET (release)"
cargo build --release --target "$ARM_TARGET" -p kgraf-shell --bin "$SHELL_BIN"
cargo build --release --target "$X86_TARGET" -p kgraf-shell --bin "$SHELL_BIN"
cargo build --release --target "$ARM_TARGET" -p kgraf-cli --bin "$CLI_NAME"
cargo build --release --target "$X86_TARGET" -p kgraf-cli --bin "$CLI_NAME"

echo "==> Joining architectures with lipo"
lipo -create \
  "target/$ARM_TARGET/release/$SHELL_BIN" \
  "target/$X86_TARGET/release/$SHELL_BIN" \
  -output "$APP_DIR/Contents/MacOS/$SHELL_BIN"
lipo -create \
  "target/$ARM_TARGET/release/$CLI_NAME" \
  "target/$X86_TARGET/release/$CLI_NAME" \
  -output "$APP_DIR/Contents/Resources/bin/$CLI_NAME"
chmod +x "$APP_DIR/Contents/MacOS/$SHELL_BIN" "$APP_DIR/Contents/Resources/bin/$CLI_NAME"

echo "==> lipo -info (both binaries must report two architectures)"
lipo -info "$APP_DIR/Contents/MacOS/$SHELL_BIN"
lipo -info "$APP_DIR/Contents/Resources/bin/$CLI_NAME"

echo "==> Building AppIcon.icns from the designed icon asset"
# The iconset just copies the designed PNGs (crates/kgraf-shell/assets/app-icon/) to
# the standard iconset names; no cargo build step or rasterizer is needed for this.
ICON_DIR="$ROOT/crates/kgraf-shell/assets/app-icon"
cp "$ICON_DIR/icon-16.png"   "$ICONSET_DIR/icon_16x16.png"
cp "$ICON_DIR/icon-32.png"   "$ICONSET_DIR/icon_16x16@2x.png"
cp "$ICON_DIR/icon-32.png"   "$ICONSET_DIR/icon_32x32.png"
cp "$ICON_DIR/icon-64.png"   "$ICONSET_DIR/icon_32x32@2x.png"
cp "$ICON_DIR/icon-128.png"  "$ICONSET_DIR/icon_128x128.png"
cp "$ICON_DIR/icon-256.png"  "$ICONSET_DIR/icon_128x128@2x.png"
cp "$ICON_DIR/icon-256.png"  "$ICONSET_DIR/icon_256x256.png"
cp "$ICON_DIR/icon-512.png"  "$ICONSET_DIR/icon_256x256@2x.png"
cp "$ICON_DIR/icon-512.png"  "$ICONSET_DIR/icon_512x512.png"
cp "$ICON_DIR/icon-1024.png" "$ICONSET_DIR/icon_512x512@2x.png"
iconutil -c icns "$ICONSET_DIR" -o "$APP_DIR/Contents/Resources/AppIcon.icns"

echo "==> Writing Info.plist"
cat > "$APP_DIR/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>$APP_NAME</string>
  <key>CFBundleDisplayName</key>
  <string>$APP_NAME</string>
  <key>CFBundleIdentifier</key>
  <string>$BUNDLE_ID</string>
  <key>CFBundleVersion</key>
  <string>$VERSION</string>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleExecutable</key>
  <string>$SHELL_BIN</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundleSupportedPlatforms</key>
  <array>
    <string>MacOSX</string>
  </array>
  <!-- Decision 0052: the shell has a tray background mode, but it is an opt-in the
       running app switches into; the app is a normal foreground Dock app on launch,
       so LSUIElement is explicitly false rather than omitted. -->
  <key>LSUIElement</key>
  <false/>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
</dict>
</plist>
PLIST

# The CLI ships inside the bundle (Contents/Resources/bin), not Contents/MacOS, so
# CFBundleExecutable unambiguously names the GUI shell and Gatekeeper/Finder never try
# to launch the CLI as the app's main executable. It is not on PATH by itself: a
# terminal user adds it by linking or exporting, e.g. after installing to /Applications:
#   ln -s "/Applications/$APP_NAME.app/Contents/Resources/bin/$CLI_NAME" /usr/local/bin/$CLI_NAME
# or: export PATH="/Applications/$APP_NAME.app/Contents/Resources/bin:$PATH"
echo "==> CLI installed at Contents/Resources/bin/$CLI_NAME (see PATH note above and scripts/release-local.md)"

# LICENSE and THIRD_PARTY_NOTICES.md exist at the repository root but were never
# actually copied into the bundle -- a user who only ever has the .app has no way to
# see either. Copied before the bundle-level `codesign` below, so the ad-hoc signature
# covers them like any other bundle resource.
echo "==> Staging LICENSE and THIRD_PARTY_NOTICES.md into the bundle"
if [[ ! -f "$ROOT/LICENSE" ]]; then
  echo "LICENSE is missing from the repository root" >&2
  exit 1
fi
if [[ ! -f "$ROOT/THIRD_PARTY_NOTICES.md" ]]; then
  echo "THIRD_PARTY_NOTICES.md is missing; run scripts/third-party-notices.sh first" >&2
  exit 1
fi
# N2: this bundle links ported code from codex (the macOS seatbelt policy) and a
# rerun re_renderer-derived shader (kgraf-galaxy); scripts/third-party-notices.sh
# appends both vendored directories' LICENSE/NOTICE text after cargo-about's own
# crates.io-dependency section -- check for it specifically, not just that the file
# exists at all.
for name in "Vendored code: ported or copied" "### codex" "### rerun-re_renderer"; do
  if ! grep -qF "$name" "$ROOT/THIRD_PARTY_NOTICES.md"; then
    echo "THIRD_PARTY_NOTICES.md is missing the vendored-code section ('$name'); regenerate with scripts/third-party-notices.sh" >&2
    exit 1
  fi
done
cp "$ROOT/LICENSE" "$APP_DIR/Contents/Resources/LICENSE.txt"
cp "$ROOT/THIRD_PARTY_NOTICES.md" "$APP_DIR/Contents/Resources/THIRD_PARTY_NOTICES.md"

echo "==> Ad-hoc code-signing (no Developer ID identity configured by default)"
# Signed inside-out, one codesign invocation per binary, bundle last -- never
# `--force --deep` to sign. `--deep` signs nested code with the outer invocation's
# flags applied uniformly from the outside in; Apple's own codesign man page calls it
# a "convenience" for simple cases and warns it can produce incorrect signatures on
# nested binaries (e.g. it would not apply --options runtime per-binary correctly in
# the notarization branch below). Signing each Mach-O explicitly, then the bundle,
# with a single identity, avoids that.
#
# `-s -` is the ad-hoc identity: it satisfies Gatekeeper's "is this signed at all"
# check and lets codesign --verify pass, but it is not a real identity and cannot be
# notarized. Hardened runtime is deliberately not requested here (`--options runtime`
# is omitted): hardened runtime is a notarization prerequisite, and enabling it without
# also notarizing would fail the tighter runtime checks for no benefit.
codesign --force -s - "$APP_DIR/Contents/Resources/bin/$CLI_NAME"
codesign --force -s - "$APP_DIR/Contents/MacOS/$SHELL_BIN"
codesign --force -s - "$APP_DIR"
# --verify --deep is fine (and recommended) here: unlike signing, --deep on verify
# only walks and checks nested signatures recursively, it never re-signs anything.
echo "==> codesign --verify --deep"
codesign --verify --deep --verbose=2 "$APP_DIR"
codesign -dv "$APP_DIR" 2>&1

NOTARIZE=0
if [[ -n "${KGRAF_DEVELOPER_ID_IDENTITY:-}" ]]; then
  if [[ -n "${KGRAF_NOTARY_PROFILE:-}" ]] || {
       [[ -n "${KGRAF_APPLE_ID:-}" ]] && [[ -n "${KGRAF_APP_SPECIFIC_PASSWORD:-}" ]] && [[ -n "${KGRAF_TEAM_ID:-}" ]]
     }; then
    NOTARIZE=1
  fi
fi

if [[ "$NOTARIZE" -eq 1 ]]; then
  echo "==> Developer ID identity + notary credentials found: re-signing and notarizing"
  # Same inside-out order as the ad-hoc path, no --deep. --timestamp requests a
  # trusted timestamp from Apple for each signature; notarization requires it, and it
  # needs a real (non ad-hoc) identity and network access, which is only true here.
  codesign --force --options runtime --timestamp -s "$KGRAF_DEVELOPER_ID_IDENTITY" \
    "$APP_DIR/Contents/Resources/bin/$CLI_NAME"
  codesign --force --options runtime --timestamp -s "$KGRAF_DEVELOPER_ID_IDENTITY" \
    "$APP_DIR/Contents/MacOS/$SHELL_BIN"
  codesign --force --options runtime --timestamp -s "$KGRAF_DEVELOPER_ID_IDENTITY" \
    "$APP_DIR"
  ZIP_FOR_NOTARY="$BUILD_ROOT/$APP_NAME-notarize.zip"
  ditto -c -k --keepParent "$APP_DIR" "$ZIP_FOR_NOTARY"
  if [[ -n "${KGRAF_NOTARY_PROFILE:-}" ]]; then
    xcrun notarytool submit "$ZIP_FOR_NOTARY" --keychain-profile "$KGRAF_NOTARY_PROFILE" --wait
  else
    xcrun notarytool submit "$ZIP_FOR_NOTARY" \
      --apple-id "$KGRAF_APPLE_ID" \
      --password "$KGRAF_APP_SPECIFIC_PASSWORD" \
      --team-id "$KGRAF_TEAM_ID" \
      --wait
  fi
  xcrun stapler staple "$APP_DIR"
  echo "==> Notarization complete and stapled"
else
  # Fail-loud rule: a substituted/degraded step must say so, never pass silently.
  echo "NOTICE: signing is ad-hoc only; notarization was SKIPPED (no Developer ID" >&2
  echo "        identity configured). Set KGRAF_DEVELOPER_ID_IDENTITY plus either" >&2
  echo "        KGRAF_NOTARY_PROFILE or KGRAF_APPLE_ID/KGRAF_APP_SPECIFIC_PASSWORD/" >&2
  echo "        KGRAF_TEAM_ID to sign for real and notarize. This build will not" >&2
  echo "        survive Gatekeeper's quarantine check on a machine that downloaded it." >&2
fi

echo "==> Verifying LICENSE and THIRD_PARTY_NOTICES.md are staged in the bundle (C3)"
for f in LICENSE.txt THIRD_PARTY_NOTICES.md; do
  [[ -f "$APP_DIR/Contents/Resources/$f" ]] || {
    echo "$APP_DIR/Contents/Resources/$f is missing" >&2
    exit 1
  }
done

echo "==> Building .dmg"
DMG_STAGE="$BUILD_ROOT/dmg-stage"
rm -rf "$DMG_STAGE"
mkdir -p "$DMG_STAGE"
cp -R "$APP_DIR" "$DMG_STAGE/"
ln -s /Applications "$DMG_STAGE/Applications"
DMG_PATH="$DIST_DIR/$APP_NAME-$VERSION-macos-universal.dmg"
rm -f "$DMG_PATH"
hdiutil create -volname "$APP_NAME" -srcfolder "$DMG_STAGE" -ov -format UDZO "$DMG_PATH"

echo "==> Sizes"
APP_SIZE="$(du -sh "$APP_DIR" | cut -f1)"
DMG_SIZE="$(du -sh "$DMG_PATH" | cut -f1)"
echo "app: $APP_SIZE  ($APP_DIR)"
echo "dmg: $DMG_SIZE  ($DMG_PATH)"

if [[ "$SKIP_VERIFY" -eq 1 ]]; then
  echo "==> --skip-verify: not mounting/launching. Done."
  exit 0
fi

echo "==> Verifying: mount dmg, copy to a temp Applications dir, launch once, close"
MOUNT_DIR="$(mktemp -d "$BUILD_ROOT/mount.XXXXXX")"
hdiutil attach "$DMG_PATH" -mountpoint "$MOUNT_DIR" -nobrowse -quiet
TEMP_APPS="$(mktemp -d "$BUILD_ROOT/apps.XXXXXX")"
cp -R "$MOUNT_DIR/$APP_NAME.app" "$TEMP_APPS/"
hdiutil detach "$MOUNT_DIR" -quiet
rmdir "$MOUNT_DIR" 2>/dev/null || true
# Detached cleanly above; clear the variable so the exit trap does not try to detach
# an already-detached mount point (the launch probe below can still fail, and the
# trap must not error out trying to redo work this line already did).
MOUNT_DIR=""

INSTALLED_APP="$TEMP_APPS/$APP_NAME.app"
codesign --verify --deep --verbose=2 "$INSTALLED_APP"

echo "==> Launching $INSTALLED_APP/Contents/MacOS/$SHELL_BIN --measure 2 --no-tray"
set +e
"$INSTALLED_APP/Contents/MacOS/$SHELL_BIN" --measure 2 --no-tray
LAUNCH_STATUS=$?
set -e
if [[ "$LAUNCH_STATUS" -eq 0 ]]; then
  echo "==> Launch probe succeeded (exit 0): the app started, presented a frame, and exited cleanly."
else
  echo "LAUNCH PROBE FAILED with exit code $LAUNCH_STATUS" >&2
  exit "$LAUNCH_STATUS"
fi

rm -rf "$TEMP_APPS"
TEMP_APPS=""

echo "==> Done. App: $APP_DIR ($APP_SIZE)  DMG: $DMG_PATH ($DMG_SIZE)"
