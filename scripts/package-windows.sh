#!/usr/bin/env bash
# Cross-builds kgraf-app.exe and kgraf.exe for x86_64-pc-windows-msvc with
# cargo-xwin, from macOS (or Linux), and packages them.
#
# No Windows machine is available here, so this is the only Windows build path.
# Nothing here runs on Windows -- see "WHAT CANNOT BE VERIFIED HERE" below and
# scripts/release-local.md's matching section for the checks that still must run
# on a real Windows machine before Windows support is claimed as working, not
# just "compiles and packages".
#
# Usage:
#   scripts/package-windows.sh              # build + verify + msi (or zip fallback)
#   scripts/package-windows.sh --skip-msi    # build + verify, zip only (skips wixl)
#
# What this script does NOT do: run either .exe (Wine is not installed and is not a
# substitute for real Windows -- SR "fail loud" means this script says plainly what
# it checked and what it could not, rather than let a Wine run stand in for one).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SKIP_MSI=0
for arg in "$@"; do
  case "$arg" in
    --skip-msi) SKIP_MSI=1 ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

TARGET="x86_64-pc-windows-msvc"
APP_NAME="kGraf"

# --- pin the whole cross-compilation toolchain, not just "whatever cargo-xwin
# finds today" -----------------------------------------------------------------------
# A pinned cargo-xwin plus pinned XWIN_* versions means a rebuild next month links
# against the exact same MSVC CRT/SDK bytes as this one, rather than whatever Microsoft
# currently publishes as "latest" in xwin's manifest -- important for reproducing a bug
# report against a specific shipped installer. Versions below were resolved by this
# script's own run on 2026-09-26 (Apple M4 Max host): `cargo-xwin --xwin-version 17`
# (the default major) currently resolves to SDK 10.0.26100 and CRT 14.44.17.14; those
# concrete numbers are pinned explicitly below rather than left to XWIN_VERSION=17
# alone, so a manifest update on Microsoft's side cannot silently change what this
# script downloads.
CARGO_XWIN_VERSION="0.23.1"
export XWIN_VERSION="17"
export XWIN_SDK_VERSION="10.0.26100"
export XWIN_CRT_VERSION="14.44.17.14"
# xwin's cache (headers, .lib/.vsix payloads extracted from Microsoft's own installers)
# lives here, keyed by the versions above; deleting it forces a redownload, not a
# silent fetch of a different version, since the versions are pinned above either way.
XWIN_CACHE_DIR="${XWIN_CACHE_DIR:-$HOME/Library/Caches/cargo-xwin}"
echo "==> xwin cache: $XWIN_CACHE_DIR (SDK $XWIN_SDK_VERSION, CRT $XWIN_CRT_VERSION)"
echo "    Downloading the Windows SDK/CRT headers and import libraries through xwin"
echo "    means accepting Microsoft's own license for redistributing them for"
echo "    compilation purposes (the same terms Visual Studio's installer presents);"
echo "    this script does not present that text itself, xwin's own first-run"
echo "    download does. Running this script is the acceptance."

# Version comes from Cargo, not hand-maintained here (same rule as scripts/package-macos.sh
# and scripts/package-linux.sh). Every crate inherits `[workspace.package].version`
# from the root Cargo.toml (each crate's own Cargo.toml now says
# `version.workspace = true`, not a literal), so kgraf-shell and kgraf-cli
# stay in lockstep by construction; read it there.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
if [[ -z "$VERSION" ]]; then
  echo "could not read [workspace.package].version from Cargo.toml" >&2
  exit 1
fi
# WiX's Version attribute wants MAJOR.MINOR.BUILD[.REVISION], each numeric; Cargo's
# three-part semver (0.7.2) already fits, so a literal ".0" fourth field is appended
# rather than reusing crates/kgraf-shell/build.rs's parsing (that one tolerates a
# non-numeric or missing component; a malformed workspace version should fail this
# script loudly instead of silently becoming 0.0.0.0 in the installer users see).
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "VERSION '$VERSION' is not MAJOR.MINOR.PATCH; scripts/package-windows.sh's MSI Version needs that shape" >&2
  exit 1
fi
MSI_VERSION="${VERSION}.0"
echo "==> Packaging kGraf $VERSION for Windows ($TARGET)"

BUILD_ROOT="$ROOT/target/package/windows"
DIST_DIR="$BUILD_ROOT/dist"
STAGE_DIR="$BUILD_ROOT/stage"
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR" "$DIST_DIR"

# --- toolchain: cargo-xwin (pinned), the MSVC target, and an RC compiler --------------
echo "==> Ensuring the $TARGET Rust target is installed"
rustup target add "$TARGET" >/dev/null

INSTALLED_XWIN_VERSION="$(cargo xwin --version 2>/dev/null | head -1 | sed -n 's/.*-xwin \([0-9.]*\)$/\1/p')"
if [[ "$INSTALLED_XWIN_VERSION" != "$CARGO_XWIN_VERSION" ]]; then
  echo "==> Installing pinned cargo-xwin $CARGO_XWIN_VERSION (found: ${INSTALLED_XWIN_VERSION:-none})"
  cargo install cargo-xwin --version "$CARGO_XWIN_VERSION" --locked
fi
echo "==> cargo-xwin $(cargo xwin --version 2>&1 | head -1)"

# cargo-xwin cross-links with lld-link and clang-cl from LLVM, and needs an RC compiler
# to embed crates/kgraf-shell/build.rs and crates/kgraf-cli/build.rs's icon,
# manifest and VERSIONINFO resources (the `embed-resource` crate, MIT, picks `llvm-rc`
# for an `-msvc` target -- see build.rs's own header comment for why it is an
# unconditional, not target-gated, build-dependency). Homebrew's `llvm` formula ships
# `llvm-rc` and `clang-cl`; the bare `rustup component add llvm-tools` toolchain
# component does NOT (it carries llc/llvm-ar/etc but not llvm-rc), which is why this
# checks for the resource compiler specifically rather than assuming any "llvm-tools"
# install covers it. cargo-xwin has its own detection that adds a Homebrew LLVM keg's
# bin/ to PATH for the build it runs; this pre-check exists only so a missing
# prerequisite is reported before minutes are spent compiling the workspace, not after.
LLVM_BIN=""
for candidate in /opt/homebrew/opt/llvm/bin /usr/local/opt/llvm/bin; do
  if [[ -x "$candidate/llvm-rc" ]]; then
    LLVM_BIN="$candidate"
    break
  fi
done
if [[ -z "$LLVM_BIN" ]] && command -v llvm-rc >/dev/null 2>&1; then
  LLVM_BIN="$(dirname "$(command -v llvm-rc)")"
fi
if [[ -z "$LLVM_BIN" ]]; then
  echo "==> llvm-rc not found; installing Homebrew's llvm (ships llvm-rc + clang-cl)"
  brew install llvm
  LLVM_BIN="/opt/homebrew/opt/llvm/bin"
  [[ -x "$LLVM_BIN/llvm-rc" ]] || LLVM_BIN="/usr/local/opt/llvm/bin"
fi
# record, and floor-check, the LLVM version actually used -- clang-cl/lld-link/
# llvm-rc all come from the same Homebrew `llvm` keg, so one version number covers all
# three. A hard pin (an exact patch version) would fight Homebrew's own upgrade
# behavior for a keg-only formula every time it revs; a floor is the practical middle
# ground (recorded here rather than assumed): this project's Windows path has only ever
# been exercised against major 23, so an unexpectedly old toolchain (major < 18, the
# floor at which clang-cl's MSVC compatibility mode matured enough for eframe/wgpu's
# codebase) fails loudly here instead of producing a mysterious link error later.
LLVM_VERSION_LINE="$("$LLVM_BIN/clang" --version 2>&1 | head -1)"
LLVM_MAJOR="$(echo "$LLVM_VERSION_LINE" | grep -o '[0-9]\+' | head -1)"
echo "==> LLVM toolchain: $LLVM_VERSION_LINE ($LLVM_BIN)"
if [[ -z "$LLVM_MAJOR" ]] || (( LLVM_MAJOR < 18 )); then
  echo "LLVM major version $LLVM_MAJOR (from '$LLVM_VERSION_LINE') is below this script's floor of 18;" >&2
  echo "run 'brew upgrade llvm' -- an older clang-cl is not verified against this workspace's Windows build." >&2
  exit 1
fi
export PATH="$LLVM_BIN:$PATH"

# --- build -----------------------------------------------------------------------------
echo "==> Building kgraf (CLI) and kgraf-app (shell) for $TARGET (release)"
cargo xwin build --release --target "$TARGET" -p kgraf-cli --bin kgraf
cargo xwin build --release --target "$TARGET" -p kgraf-shell --bin kgraf-app

BIN_DIR="$ROOT/target/$TARGET/release"
CLI_EXE="$BIN_DIR/kgraf.exe"
APP_EXE="$BIN_DIR/kgraf-app.exe"
for exe in "$CLI_EXE" "$APP_EXE"; do
  [[ -f "$exe" ]] || {
    echo "expected build output missing: $exe" >&2
    exit 1
  }
done

# --- verify: PE format, and the GUI/console subsystem each binary must carry ------------
# file(1) reads the PE header's Subsystem field, no execution needed: it reports
# "(GUI)" for IMAGE_SUBSYSTEM_WINDOWS_GUI and "(console)" for
# IMAGE_SUBSYSTEM_WINDOWS_CUI. kgraf-app.exe MUST be "(GUI)" -- a coordinator
# review caught it building "(console)" (crates/kgraf-shell/src/bin/kgraf-app.rs
# lacked `windows_subsystem = "windows"`), which on Windows means a console window
# flashes up beside the shell's own window on every launch; this crate's
# `crates/kgraf-shell/src/diag.rs` module keeps --probe/--measure output reachable
# despite the fix (reattaching the launching console for a diagnostic invocation only,
# or a rotated, size-capped log file otherwise -- see its module doc). kgraf.exe
# is the CLI and MUST stay "(console)": that IS its UI.
echo "==> file(1) on both executables (expect PE32+ x86-64, MS Windows; kgraf.exe console, kgraf-app.exe GUI)"
for exe in "$CLI_EXE" "$APP_EXE"; do
  desc="$(file -b "$exe")"
  echo "  $(basename "$exe"): $desc"
  case "$desc" in
    *"PE32+ executable"*"x86-64"*"MS Windows"*) ;;
    *)
      echo "$(basename "$exe") is not a PE32+ x86-64 Windows executable: $desc" >&2
      exit 1
      ;;
  esac
done
case "$(file -b "$CLI_EXE")" in
  *"(console)"*) ;;
  *)
    echo "kgraf.exe (the CLI) is no longer a console-subsystem binary: $(file -b "$CLI_EXE")" >&2
    exit 1
    ;;
esac
case "$(file -b "$APP_EXE")" in
  *"(GUI)"*) ;;
  *)
    echo "kgraf-app.exe is not a GUI-subsystem binary (a console window would flash up on launch): $(file -b "$APP_EXE")" >&2
    exit 1
    ;;
esac

# --- verify: neither binary imports the dynamic MSVC CRT --------------------------
# .cargo/config.toml sets `-C target-feature=+crt-static` for this target, so the CRT is
# linked in statically; nothing in this repository ships VCRUNTIME140.dll/MSVCP140.dll,
# so a dynamic import of either would mean the packaged .exe fails to launch on a bare
# Windows machine with a "the program can't start because VCRUNTIME140.dll is missing"
# dialog -- exactly the defect this check catches before it reaches a user.
echo "==> Verifying neither executable imports the dynamic MSVC CRT (llvm-objdump -p)"
for exe in "$CLI_EXE" "$APP_EXE"; do
  imports="$("$LLVM_BIN/llvm-objdump" -p "$exe" | grep -i "DLL Name" || true)"
  bad="$(echo "$imports" | grep -iE "vcruntime|msvcp|concrt|ucrtbase\.dll" || true)"
  if [[ -n "$bad" ]]; then
    echo "$(basename "$exe") imports a dynamic MSVC CRT DLL (crt-static should have prevented this):" >&2
    echo "$bad" >&2
    exit 1
  fi
  echo "  $(basename "$exe"): no VCRUNTIME/MSVCP/CONCRT/UCRTBASE import (static CRT confirmed)"
done

# --- verify: the icon, manifest and version resource, by resource TYPE not raw bytes
# llvm-readobj --coff-resources parses the PE resource directory structurally and
# reports each entry's RT_* type; asserting on ICON/GROUP_ICON/VERSIONINFO/MANIFEST
# being present is a real structural check, unlike grepping the file for a PNG
# signature or a UTF-16 string (either of which could, in principle, appear by
# coincidence in unrelated data, and neither proves the bytes are actually wired up as
# a resource the OS resource loader will find).
echo "==> Verifying the icon/manifest/version resources by type (llvm-readobj --coff-resources)"
for exe in "$CLI_EXE" "$APP_EXE"; do
  types="$("$LLVM_BIN/llvm-readobj" --coff-resources "$exe" | grep -o 'Type: [A-Z_]*' | sort -u)"
  echo "  $(basename "$exe") resource types: $(echo "$types" | tr '\n' ' ')"
  for want in "Type: ICON" "Type: GROUP_ICON" "Type: VERSIONINFO" "Type: MANIFEST"; do
    if ! echo "$types" | grep -qx "$want"; then
      echo "$(basename "$exe") is missing a $want resource" >&2
      exit 1
    fi
  done
done

echo "==> Sizes"
CLI_SIZE="$(du -h "$CLI_EXE" | cut -f1)"
APP_SIZE="$(du -h "$APP_EXE" | cut -f1)"
echo "kgraf.exe: $CLI_SIZE   kgraf-app.exe: $APP_SIZE"

# --- shared payload: staged copies, generated icon, LICENSE, THIRD_PARTY_NOTICES.md
# --------------------------------------------------------------------------------------
# The MSI and zip must ship LICENSE and THIRD_PARTY_NOTICES.md, and the notices file must
# be generated against the Windows target. Otherwise it omits every crate reached only
# through the GUI shell's dependency tree (egui, eframe, wgpu, winit, tray-icon,
# arboard, accesskit_windows, gpu-allocator, and cargo-xwin's own Windows-only
# transitive crates). `scripts/third-party-notices.sh` covers `--target
# x86_64-pc-windows-msvc` already (see `about.toml`'s `targets` list), so regenerating
# it is enough; this script does not regenerate it itself (that is a repository-wide,
# multi-minute `cargo about` run, not a per-package-run step), it only asserts the file
# exists, is newer than a staleness sentinel is not tracked here, and ships it.
cp "$CLI_EXE" "$STAGE_DIR/kgraf.exe"
cp "$APP_EXE" "$STAGE_DIR/kgraf-app.exe"

if [[ ! -f "$ROOT/LICENSE" ]]; then
  echo "LICENSE is missing from the repository root" >&2
  exit 1
fi
if [[ ! -f "$ROOT/THIRD_PARTY_NOTICES.md" ]]; then
  echo "THIRD_PARTY_NOTICES.md is missing; run scripts/third-party-notices.sh first" >&2
  exit 1
fi
for name in egui eframe wgpu winit tray-icon arboard accesskit_windows gpu-allocator; do
  if ! grep -qi "$name" "$ROOT/THIRD_PARTY_NOTICES.md"; then
    echo "THIRD_PARTY_NOTICES.md does not mention '$name'; regenerate with scripts/third-party-notices.sh" >&2
    echo "(it must cover x86_64-pc-windows-msvc -- check about.toml's targets list and rerun)" >&2
    exit 1
  fi
done
# the vendored (third_party/, decision 0055) LICENSE/NOTICE texts scripts/third-
# party-notices.sh appends after cargo-about's own crates.io-dependency section --
# kgraf.exe/kgraf-app.exe both link ported code from codex (the macOS seatbelt
# policy, the Linux seccomp filter, the Windows sandbox backend) and kgraf-galaxy
# links a rerun re_renderer-derived shader, so this checks specifically for those, not
# only the crates.io section checked above.
for name in "Vendored code: ported or copied" "### codex" "### rerun-re_renderer"; do
  if ! grep -qF "$name" "$ROOT/THIRD_PARTY_NOTICES.md"; then
    echo "THIRD_PARTY_NOTICES.md is missing the vendored-code section ('$name'); regenerate with scripts/third-party-notices.sh" >&2
    exit 1
  fi
done
cp "$ROOT/LICENSE" "$STAGE_DIR/LICENSE.txt"
cp "$ROOT/THIRD_PARTY_NOTICES.md" "$STAGE_DIR/THIRD_PARTY_NOTICES.md"

echo "==> Building the installer icon (Add/Remove Programs), same asset as the embedded one"
# gen_ico.rs (crates/kgraf-shell/examples/gen_ico.rs) packs the designed icon asset's
# PNG bytes into an .ico unchanged, so this is byte-identical to the icon embedded in
# kgraf-app.exe/kgraf.exe via build.rs.
cargo build --release --example gen_ico -p kgraf-shell
GEN_ICO_BIN="$ROOT/target/release/examples/gen_ico"
"$GEN_ICO_BIN" "$STAGE_DIR/kgraf.ico"
file "$STAGE_DIR/kgraf.ico"

ZIP_PATH="$DIST_DIR/$APP_NAME-$VERSION-windows-x86_64.zip"
MSI_PATH="$DIST_DIR/$APP_NAME-$VERSION-windows-x86_64.msi"

# --- MSI, if wixl (msitools) can run here -----------------------------------------------
# `cargo-wix` was evaluated first and rejected for this host: it drives Microsoft's own
# WiX toolset binaries (candle.exe/light.exe, or wix.exe in WiX v4+), which are native
# Windows/.NET executables with no macOS or Linux build -- "cargo wix" would need to
# shell out to a tool that cannot exist here, cargo-wix installed or not. `wixl`, from
# the `msitools` project (LGPL-2.1-or-later; a build TOOL invoked as a subprocess, like
# `dpkg-deb` or `hdiutil` elsewhere in this repo's packaging scripts -- it is never
# linked into or shipped with kGraf, so the project's accepted-license list for shipped
# dependencies does not apply to it), is a real, independent, GNOME-project
# implementation of the WiX XML schema that runs natively on macOS and Linux and
# produces a genuine MSI (Composite Document File / CFBF format) -- verified below with
# `file`(1) and `msiinfo`, not merely "wixl exited 0". See docs/decisions/0061 for the
# full comparison against the originally planned cargo-dist/cargo-packager path.
if [[ "$SKIP_MSI" -eq 1 ]]; then
  echo "==> --skip-msi: building the zip only"
  HAVE_WIXL=0
elif command -v wixl >/dev/null 2>&1 && command -v msiinfo >/dev/null 2>&1; then
  HAVE_WIXL=1
else
  echo "NOTICE: wixl/msiinfo (msitools) not found; building a zip instead of an MSI." >&2
  echo "        Install with 'brew install msitools' to get a real, verified .msi from this host." >&2
  HAVE_WIXL=0
fi

if [[ "$HAVE_WIXL" -eq 1 ]]; then
  echo "==> Building the x64 MSI with wixl"
  WXS_PATH="$STAGE_DIR/kgraf.wxs"
  # UpgradeCode and every Component's Guid are fixed UUIDs, each generated with
  # python3's `uuid.uuid4()` (the LicenseFile/NoticesFile GUIDs in an earlier version
  # of this script were hand-typed rather than actually generated, and are now real
  # `uuid4()` output like the rest), recorded here, that MUST NOT change between
  # releases: WiX's major-upgrade detection keys off UpgradeCode, and a Component's
  # identity (for repair/uninstall) keys off its own Guid. Regenerating them on every
  # run, or on the next release, would make every future installer unable to
  # recognize or upgrade this one. ShortcutComponent's Guid was deliberately
  # regenerated once already: its key-path registry value
  # moved from HKCU to HKLM, which changes what the component actually represents; a
  # new Guid was the correct response to that, not the previous Guid kept
  # unchanged -- nothing had shipped yet under the old one, so nothing needed the old
  # identity preserved.
  #
  # Decision 0062 (rename to kGraf, 2026-09-27): UpgradeCode is KEPT, so installing
  # kGraf is a major upgrade of an installed Haustoria: MajorUpgrade removes the old
  # product (its `Program Files\Haustoria` folder, its PATH entry -- the `=-*PATH` row
  # it was built with removes it on uninstall -- its Start Menu folder and its
  # HKLM\Software\Haustoria value) and installs this one. Every Component Guid was
  # regenerated (python3 `uuid.uuid4()`) at the rename: each component's key path
  # changed (new file names, new install folder, new registry key), and Windows
  # Installer's component rules tie a Guid to one key path for life. The new Guids are
  # fixed from here on, under the same rule as above.
  #
  # C2: `wixl -a x64` (below) is what actually makes this an x64 package (Template
  # x64;1033 in the summary info, msidbComponentAttributes64bit (256) set on every
  # per-machine Component) -- an earlier version of this script omitted `-a x64` and
  # produced a 32-bit-tagged MSI. N5: this was never actually installed on Windows
  # (no Windows machine here); the honest claim is that a 32-bit-tagged MSI would
  # likely have installed the CLI/shell into `Program Files (x86)` on a 64-bit
  # Windows machine, not that it was observed doing so.
  #
  # H1/N1: the Environment element in CliExe adds INSTALLFOLDER to the machine PATH
  # (System="yes": HKLM, so every user gets it, matching the per-machine InstallScope
  # above; Part="last"/Action="set": an append, not an overwrite, of the existing
  # PATH). Permanent="yes" here -- NOT "no" -- because `wixl` reverses WiX's own
  # documented meaning of this attribute: verified empirically (this script's own
  # comment is not enough to trust blindly -- see scripts/release-local.md's bug
  # report text for msitools), `wixl -a x64` compiling `Permanent="yes"` emits an
  # Environment table row of `=-*PATH` (the "remove this value on uninstall" marker),
  # while `Permanent="no"` emits `=*PATH` (append, never removed). A first version of
  # this script used `Permanent="no"` -- the WiX-documented spelling for "not
  # permanent, so remove on uninstall" -- and got the opposite of what it asked for:
  # the PATH entry would have survived every uninstall. `scripts/package-windows.sh`
  # asserts the `=-*PATH` row below so a future `wixl` release fixing this reversal
  # (or an accidental revert of this comment) fails the build instead of silently
  # shipping the bug again.
  #
  # MajorUpgrade AllowSameVersionUpgrades="yes" -- rerunning the installer for the
  # exact version already present repairs/reinstalls instead of refusing with "a later
  # version is already installed" (WiX's default compares strictly greater-than
  # otherwise), which matters for this project's own iterative packaging runs during
  # development, not just an end user's upgrade path.
  #
  # the Start Menu shortcut's key path is now a real, separate HKLM registry value
  # in the ShortcutComponent itself (Root="HKLM", matching InstallScope="perMachine" --
  # HKCU would silently install "per-user" registry state under a per-machine install,
  # which breaks a second user on the same machine seeing the app as already
  # installed) with its own GUID (unrelated to CliExe/AppExe's), since a Component's
  # GUID identity must be unique and this one's role (own the shortcut + this one
  # registry value) is genuinely distinct from the other two components' (own one exe
  # file each).
  cat > "$WXS_PATH" <<WXSEOF
<?xml version="1.0" encoding="utf-8"?>
<Wix xmlns="http://schemas.microsoft.com/wix/2006/wi">
  <Product Id="*" Name="$APP_NAME" Language="1033" Version="$MSI_VERSION"
           Manufacturer="Marco Cross" UpgradeCode="9b79e695-e02f-4f9e-bc82-5829ad4dcc00">
    <Package InstallerVersion="500" Compressed="yes" InstallScope="perMachine" />
    <MajorUpgrade AllowSameVersionUpgrades="yes" DowngradeErrorMessage="A newer version of $APP_NAME is already installed." />
    <Media Id="1" Cabinet="kgraf.cab" EmbedCab="yes" />
    <Icon Id="kGrafIcon.ico" SourceFile="kgraf.ico" />
    <Property Id="ARPPRODUCTICON" Value="kGrafIcon.ico" />
    <Directory Id="TARGETDIR" Name="SourceDir">
      <Directory Id="ProgramFiles64Folder">
        <Directory Id="INSTALLFOLDER" Name="$APP_NAME">
          <Component Id="CliExe" Guid="97b81445-dceb-4618-afde-26219e4fbc6f" Win64="yes">
            <File Id="kGrafExe" Source="kgraf.exe" KeyPath="yes" />
            <Environment Id="PathEnv" Name="PATH" Value="[INSTALLFOLDER]" Permanent="yes" Part="last" Action="set" System="yes" />
          </Component>
          <Component Id="AppExe" Guid="27fc8a36-87b9-4e64-b43a-cbec00041ea8" Win64="yes">
            <File Id="kGrafAppExe" Source="kgraf-app.exe" KeyPath="yes" />
          </Component>
          <Component Id="LicenseFile" Guid="833ada7a-88cc-466e-9b10-58657c465ec1" Win64="yes">
            <File Id="LicenseTxt" Source="LICENSE.txt" KeyPath="yes" />
          </Component>
          <Component Id="NoticesFile" Guid="d7a6dcce-2a5a-40ff-9c09-504f296443ce" Win64="yes">
            <File Id="ThirdPartyNotices" Source="THIRD_PARTY_NOTICES.md" KeyPath="yes" />
          </Component>
        </Directory>
      </Directory>
      <Directory Id="ProgramMenuFolder">
        <Directory Id="ApplicationProgramsFolder" Name="$APP_NAME">
          <Component Id="ShortcutComponent" Guid="8758e87c-0781-4c7a-a8a9-1549945bd89e" Win64="yes">
            <Shortcut Id="AppShortcut" Name="$APP_NAME"
                      Target="[INSTALLFOLDER]kgraf-app.exe"
                      WorkingDirectory="INSTALLFOLDER" Icon="kGrafIcon.ico" />
            <RemoveFolder Id="RemoveAppProgramsFolder" On="uninstall" />
            <RegistryValue Root="HKLM" Key="Software\kGraf" Name="installed" Type="integer" Value="1" KeyPath="yes" />
          </Component>
        </Directory>
      </Directory>
    </Directory>
    <Feature Id="MainFeature" Title="$APP_NAME" Level="1">
      <ComponentRef Id="CliExe" />
      <ComponentRef Id="AppExe" />
      <ComponentRef Id="LicenseFile" />
      <ComponentRef Id="NoticesFile" />
      <ComponentRef Id="ShortcutComponent" />
    </Feature>
  </Product>
</Wix>
WXSEOF
  (cd "$STAGE_DIR" && wixl -a x64 -o "$MSI_PATH" "$(basename "$WXS_PATH")")

  echo "==> Verifying the MSI (file(1) + msiinfo table contents, all required -- M8)"
  file "$MSI_PATH"
  case "$(file -b "$MSI_PATH")" in
    *"MSI Installer"*) ;;
    *)
      echo "$MSI_PATH does not report as an MSI Installer" >&2
      exit 1
      ;;
  esac

  # C2: Template x64 in the summary info is the MSI-level marker of a 64-bit package.
  SUMMARY="$(msiinfo suminfo "$MSI_PATH")"
  echo "$SUMMARY" | grep "^Template:"
  if ! echo "$SUMMARY" | grep -q "^Template: x64;1033$"; then
    echo "MSI Template is not 'x64;1033' -- this is not a 64-bit-tagged package" >&2
    exit 1
  fi

  echo "-- File table (expect kgraf.exe/kgraf-app.exe/LICENSE.txt/THIRD_PARTY_NOTICES.md, real byte sizes) --"
  FILE_TABLE="$(msiinfo export "$MSI_PATH" File)"
  echo "$FILE_TABLE"
  for f in kGrafExe kGrafAppExe LicenseTxt ThirdPartyNotices; do
    echo "$FILE_TABLE" | grep -q "^$f	" || {
      echo "MSI File table is missing the $f row" >&2
      exit 1
    }
  done

  # C2: component attribute bit 0x100 (256) is msidbComponentAttributes64bit; every
  # per-machine file component set Win64="yes" above must carry it (the registry-
  # keypath shortcut component's own attributes additionally carry the
  # msidbComponentAttributesRegistryKeyPath bit, so its total is 256+4=260, not 256
  # alone -- both are checked for the bit rather than an exact equality).
  echo "-- Component table (bit 0x100 = 256 = 64-bit, on every component) --"
  COMPONENT_TABLE="$(msiinfo export "$MSI_PATH" Component)"
  echo "$COMPONENT_TABLE"
  while IFS=$'\t' read -r id _guid _dir attrs _cond _keypath; do
    case "$id" in
      Component|s72*|"") continue ;;
    esac
    if (( (attrs & 256) == 0 )); then
      echo "Component $id has attributes=$attrs, missing the 64-bit bit (256)" >&2
      exit 1
    fi
  done <<<"$COMPONENT_TABLE"

  # H1/N1: the Environment table row PATH modification. `=-*PATH`, not `=*PATH`: the
  # leading `-` is WiX's own on-disk marker for "remove this value on uninstall",
  # which is what the `.wxs` above's `Permanent="yes"` (wixl's reversed spelling of
  # "not permanent") must produce -- see this file's `.wxs`-generation comment above
  # for the full explanation. `=*PATH` (no `-`) would mean the entry survives an
  # uninstall, the opposite of N1's fix.
  echo "-- Environment table (expect one PATH row on CliExe, marked removable on uninstall) --"
  ENV_TABLE="$(msiinfo export "$MSI_PATH" Environment)"
  echo "$ENV_TABLE"
  echo "$ENV_TABLE" | grep -q '=-\*PATH.*CliExe' || {
    echo "MSI Environment table is missing the removable ('=-*PATH') PATH row on CliExe" >&2
    exit 1
  }

  # the same-version-upgrade detection row AllowSameVersionUpgrades="yes" adds.
  echo "-- Upgrade table (expect a WIX_SAME_VERSION_UPGRADE_DETECTED row) --"
  UPGRADE_TABLE="$(msiinfo export "$MSI_PATH" Upgrade)"
  echo "$UPGRADE_TABLE"
  echo "$UPGRADE_TABLE" | grep -q "WIX_SAME_VERSION_UPGRADE_DETECTED" || {
    echo "MSI Upgrade table is missing the WIX_SAME_VERSION_UPGRADE_DETECTED row (AllowSameVersionUpgrades)" >&2
    exit 1
  }

  echo "-- Shortcut table (expect one Start Menu entry to kgraf-app.exe) --"
  msiinfo export "$MSI_PATH" Shortcut

  MSI_SIZE="$(du -h "$MSI_PATH" | cut -f1)"
  echo "msi: $MSI_SIZE  ($MSI_PATH)"
fi

# --- zip: always built, so there is always at least one artifact ----------------------
# fixed mtimes (SOURCE_DATE_EPOCH-style reproducibility) on every entry, so two
# runs from the same input binaries produce byte-identical zips -- `zip -X` already
# drops extra file attributes/timestamps noise, but the entry mtimes themselves still
# default to "now" unless told otherwise. The MSI is NOT reproducible the same way and
# this script does not claim it is: wixl's `Product/@Id="*"` intentionally generates a
# fresh ProductCode GUID on every run (WiX's own convention for "a new one every
# build"), and the CFBF container format wixl produces also embeds its own creation
# timestamp in `_SummaryInformation` independent of file mtimes. Pinning ProductCode
# would defeat MSI's own mechanism for telling two builds apart during an upgrade scan;
# this is a deliberate difference from the zip, not an oversight.
echo "==> Building the zip (always, so there is at least one Windows artifact even without wixl)"
rm -f "$ZIP_PATH"
SOURCE_DATE_EPOCH="$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || date +%s)"
# `date -r EPOCH` is BSD/macOS syntax; `date -d @EPOCH` is GNU/Linux syntax. An
# earlier version of this script only tried the BSD form and swallowed a failure with
# `|| true`, so on a GNU host the zip would have silently stopped being reproducible
# instead of failing -- exactly the silent-degradation SR "fail loud" exists to catch.
# Both are tried; if neither formats the timestamp, this is now a hard error, not a
# quietly-skipped step.
if TOUCH_STAMP="$(date -r "$SOURCE_DATE_EPOCH" +%Y%m%d%H%M.%S 2>/dev/null)" && [[ -n "$TOUCH_STAMP" ]]; then
  :
elif TOUCH_STAMP="$(date -d "@$SOURCE_DATE_EPOCH" +%Y%m%d%H%M.%S 2>/dev/null)" && [[ -n "$TOUCH_STAMP" ]]; then
  :
else
  echo "could not format timestamp $SOURCE_DATE_EPOCH with either BSD 'date -r' or GNU 'date -d @';" >&2
  echo "the zip would not be reproducible -- fix this script's date invocation for this host's date(1)" >&2
  exit 1
fi
( cd "$STAGE_DIR" && touch -t "$TOUCH_STAMP" kgraf.exe kgraf-app.exe LICENSE.txt THIRD_PARTY_NOTICES.md )
# `-X`: no extra file attributes/UID/GID. `-D`: no per-directory entries (there are none
# here, but this keeps the archive minimal regardless). Entry order is already
# deterministic (explicit file list, not a glob over directory-iteration order).
(cd "$STAGE_DIR" && zip -q -X -D "$ZIP_PATH" kgraf.exe kgraf-app.exe LICENSE.txt THIRD_PARTY_NOTICES.md)
echo "==> Verifying zip contents (expect exactly the four files above)"
ZIP_LISTING="$(unzip -l "$ZIP_PATH")"
echo "$ZIP_LISTING"
# Captured into a variable first, not `unzip -l ... | grep -q ...` directly: under
# `set -o pipefail` (this script's own shebang line), `grep -q`'s early exit on a
# match can SIGPIPE `unzip` before it finishes writing, and pipefail then reports the
# whole pipeline as failed on `unzip`'s SIGPIPE exit status even though `grep` found
# the match -- a real, reproducible flake this exact pattern hit during this task.
for f in kgraf.exe kgraf-app.exe LICENSE.txt THIRD_PARTY_NOTICES.md; do
  echo "$ZIP_LISTING" | grep -q " $f\$" || {
    echo "zip is missing $f" >&2
    exit 1
  }
done
ZIP_SIZE="$(du -h "$ZIP_PATH" | cut -f1)"
echo "zip: $ZIP_SIZE  ($ZIP_PATH)"

cat <<NOTE

==> Done.
    kgraf.exe:     $CLI_SIZE
    kgraf-app.exe: $APP_SIZE
    zip:               $DIST_DIR/$(basename "$ZIP_PATH") ($ZIP_SIZE)
NOTE
if [[ "$HAVE_WIXL" -eq 1 ]]; then
  echo "    msi:               $DIST_DIR/$(basename "$MSI_PATH") ($MSI_SIZE)"
fi
cat <<'NOTE'

==> WHAT CANNOT BE VERIFIED HERE (no Windows machine, no GitHub Actions minutes):
    this script never runs either .exe -- nothing on this macOS host executes Windows
    binaries (Wine is not installed and would not be a substitute for real Windows
    even if it were: SR "fail loud" over a result that only looks like a pass). The
    owner must, on a real Windows 10+ machine, with Rust, the MSVC Build Tools (or full
    Visual Studio) and git all installed (the sandbox test below builds real code and
    needs a real linker, not just a runtime):
      1. Install (msiexec /i on the .msi, or unzip the .zip) and launch, from a
         terminal, `start /wait kgraf-app --probe` (plain `kgraf-app --probe`
         does not reliably wait for or capture a GUI-subsystem child's output/exit code
         from cmd.exe -- see crates/kgraf-shell/src/bin/kgraf-app.rs's module
         doc). This reports the graphics adapter and, per decision 0054, must say
         plainly if it fell back to WARP (software rendering) rather than silently
         rendering on the CPU.
      2. Run kgraf.exe --help and confirm the icon/version info show correctly in
         Explorer's file Properties dialog and (for the .msi path) in Add/Remove
         Programs.
      3. Run the sandbox's escape-attempt suite for real, single-threaded, as a normal
         (non-elevated) user:
           cargo test -p kgraf-sandbox --test escape_windows -- --test-threads=1
         crates/kgraf-sandbox/tests/escape_windows.rs is compile-checked only on
         this host (`#![cfg(windows)]`); it has 31 #[test] functions, of which
         `probe_entry` is the in-process probe helper the other 30 spawn as a child, not
         itself an escape scenario -- so this is 30 escape-attempt tests, not previously
         run on real Windows. Three specific behaviors the sandboxing evidence already
         flags as unverified are called out in that file's own header comment.
    Also unsigned: neither .exe nor the .msi is code-signed (no Windows code-signing
    certificate exists yet, same gap as macOS's ad-hoc-only signing). Expect a
    SmartScreen "Windows protected your PC" prompt and a Mark-of-the-Web warning on
    anything downloaded through a browser; this is PARTIAL, not fixed, until a
    certificate exists.
NOTE
