#!/usr/bin/env bash
# Builds the Windows self-test bundle: kGraf's test binaries cross-built for
# x86_64-pc-windows-msvc with cargo-xwin, kgraf.exe, the kgraf-wintest runner,
# the sources and fixtures the tests read, and a README, in one zip.
#
# Usage:
#   scripts/package-wintest.sh            # build and zip
#
# Output: dist/wintest/kgraf-wintest-<version>-<commit>-windows-x86_64.zip
#
# Native Windows mode (KGRAF_WINTEST_NATIVE=1, or run under Git Bash on
# Windows): builds with plain cargo for the host target, skips the cargo-xwin
# and LLVM checks, the file(1) check and the zip, and leaves the staged folder
# in target/wintest/bundle/<name>. The cross-build path does not change.
#
# Outside native mode nothing here runs a Windows binary; the zip is meant to
# be unzipped and run on a Windows machine (see docs/WINDOWS-TESTING.md and
# the README in the zip). Only x86_64 is built: ARM64 Windows runs it under
# emulation, and the report records that. A native aarch64-pc-windows-msvc
# bundle needs that Rust target and the ARM64 Windows SDK libraries in the
# xwin cache.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"

TARGET="x86_64-pc-windows-msvc"
NATIVE=0
case "${KGRAF_WINTEST_NATIVE:-}$(uname -s)" in
  1* | *MINGW* | *MSYS* | *CYGWIN*) NATIVE=1 ;;
esac
# In native mode cargo is a Windows program: give it Windows spellings of paths.
winpath() { if [[ "$NATIVE" == 1 ]]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

# The same pins as scripts/package-windows.sh, so both link the same SDK and CRT.
CARGO_XWIN_VERSION="0.23.1"
export XWIN_VERSION="17"
export XWIN_SDK_VERSION="10.0.26100"
export XWIN_CRT_VERSION="14.44.17.14"
# Test binaries without debug information: the MSVC debug information goes to
# separate .pdb files that the bundle does not ship anyway, and this halves
# the build. Its own target folder, so the regular debug build is not rebuilt.
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_TARGET_DIR="$(winpath "$ROOT/target/wintest")"
JOBS="${JOBS:-3}"

if [[ "$NATIVE" == 0 ]]; then
  rustup target list --installed | grep -qx "$TARGET" || {
    echo "the $TARGET Rust target is not installed: rustup target add $TARGET" >&2
    exit 1
  }
  INSTALLED_XWIN_VERSION="$(cargo xwin --version 2>/dev/null | head -1 | sed -n 's/.*-xwin \([0-9.]*\)$/\1/p')"
  if [[ "$INSTALLED_XWIN_VERSION" != "$CARGO_XWIN_VERSION" ]]; then
    echo "cargo-xwin $CARGO_XWIN_VERSION is needed (found: ${INSTALLED_XWIN_VERSION:-none}):" >&2
    echo "  cargo install cargo-xwin --version $CARGO_XWIN_VERSION --locked" >&2
    exit 1
  fi
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
  [[ -n "$LLVM_BIN" ]] || {
    echo "llvm-rc not found; install LLVM (for example: brew install llvm)" >&2
    exit 1
  }
  export PATH="$LLVM_BIN:$PATH"
fi

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
COMMIT="$(git -C "$ROOT" rev-parse --short=12 HEAD)"
if [[ -n "$(git -C "$ROOT" status --porcelain)" ]]; then
  COMMIT="$COMMIT-dirty"
fi
BUILT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
NAME="kgraf-wintest-$VERSION-$COMMIT-windows-x86_64"
WORK="$ROOT/target/wintest/bundle"
STAGE="$WORK/$NAME"
DIST="$ROOT/dist/wintest"
ZIP="$DIST/$NAME.zip"
rm -rf "$WORK"
mkdir -p "$STAGE"
[[ "$NATIVE" == 1 ]] || mkdir -p "$DIST"

if [[ "$NATIVE" == 1 ]]; then
  echo "==> Building every workspace test binary natively"
  cargo test --no-run --workspace -j "$JOBS" \
    --message-format=json-render-diagnostics >"$WORK/messages.jsonl"

  echo "==> Building the runner (release)"
  cargo build --release -p kgraf-wintest --bin kgraf-wintest -j "$JOBS"
  RUNNER="$ROOT/target/wintest/release/kgraf-wintest.exe"

  echo "==> Building the packer"
  cargo build -p kgraf-wintest --bin kgraf-wintest -j "$JOBS"
  PACKER="$ROOT/target/wintest/debug/kgraf-wintest.exe"

  # cargo metadata writes the root as a JSON string: undo the escaped backslashes.
  BUILD_ROOT="$(cargo metadata --format-version 1 --no-deps |
    sed -n 's/.*"workspace_root":"\([^"]*\)".*/\1/p' | sed 's/\\\\/\\/g')"
else
  echo "==> Cross-building every workspace test binary for $TARGET"
  cargo xwin test --no-run --workspace --target "$TARGET" -j "$JOBS" \
    --message-format=json-render-diagnostics >"$WORK/messages.jsonl"

  echo "==> Cross-building the runner (release)"
  cargo xwin build --release -p kgraf-wintest --bin kgraf-wintest --target "$TARGET" -j "$JOBS"
  RUNNER="$CARGO_TARGET_DIR/$TARGET/release/kgraf-wintest.exe"
  # file(1) prints, for example, "PE32+ executable (console) x86-64, for MS Windows".
  RUNNER_KIND="$(file -b "$RUNNER")"
  for want in "PE32+ executable" "(console)" "x86-64"; do
    if [[ "$RUNNER_KIND" != *"$want"* ]]; then
      echo "the runner is not a PE32+ x86-64 console program: $RUNNER_KIND" >&2
      exit 1
    fi
  done

  echo "==> Building the packer for this machine"
  cargo build -p kgraf-wintest --bin kgraf-wintest -j "$JOBS"
  PACKER="$CARGO_TARGET_DIR/debug/kgraf-wintest"

  # The path the test binaries hold as their workspace root, in cargo's spelling.
  BUILD_ROOT="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["workspace_root"])')"
fi
[[ -n "$BUILD_ROOT" ]] || {
  echo "cargo metadata gave no workspace root" >&2
  exit 1
}

# The sources and fixtures the tests read at run time: every tracked file of
# the crates, the vendored code, the top-level documents and the decisions.
git -C "$ROOT" ls-files -- crates third_party Cargo.toml Cargo.lock LICENSE README.md ARCHITECTURE.md \
  ':(glob)docs/*.md' docs/decisions >"$WORK/src-list.txt"

echo "==> Staging the bundle"
"$PACKER" pack --messages "$(winpath "$WORK/messages.jsonl")" --build-root "$BUILD_ROOT" \
  --out "$(winpath "$STAGE")" --version "$VERSION" --commit "$COMMIT" --built "$BUILT" \
  --src-list "$(winpath "$WORK/src-list.txt")"
cp "$RUNNER" "$STAGE/kgraf-wintest.exe"
# Windows line endings for the two text files a person opens in Notepad.
for f in README.txt MANUAL-NETWORK-TEST.txt; do
  perl -pe 's/\r?\n/\r\n/' "$ROOT/crates/kgraf-wintest/bundle/$f" >"$STAGE/$f"
  if LC_ALL=C grep -q '[^ -~[:space:]]' "$STAGE/$f"; then
    echo "$f holds characters outside ASCII" >&2
    exit 1
  fi
done

for entry in kgraf-wintest.exe README.txt MANUAL-NETWORK-TEST.txt bundle-manifest.json tests programs src; do
  [[ -e "$STAGE/$entry" ]] || {
    echo "the staged bundle is missing $entry" >&2
    exit 1
  }
done
[[ -f "$STAGE/programs/kgraf.exe" ]] || {
  echo "the staged bundle has no programs/kgraf.exe" >&2
  exit 1
}

if [[ "$NATIVE" == 1 ]]; then
  echo "==> Done: $STAGE"
  echo "    $(find "$STAGE/tests" -name '*.exe' | wc -l | tr -d ' ') test binaries, $(du -sh "$STAGE" | cut -f1) staged"
  exit 0
fi

echo "==> Zipping"
rm -f "$ZIP"
(cd "$WORK" && zip -q -r -X "$ZIP" "$NAME")
TESTS="$(find "$STAGE/tests" -name '*.exe' | wc -l | tr -d ' ')"
echo "==> Done: $ZIP"
echo "    $(du -h "$ZIP" | cut -f1) zipped, $(du -sh "$STAGE" | cut -f1) unzipped, $TESTS test binaries"
rm -rf "$WORK"
