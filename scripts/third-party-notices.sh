#!/bin/sh
# Regenerate THIRD_PARTY_NOTICES.md from the dependency tree with cargo-about, then
# append every vendored directory's own LICENSE/NOTICE text.
#
# Install once:
#   cargo install cargo-about --locked --features cli
# (cargo-about 0.9 builds its binary only with the `cli` feature; without it the
# install succeeds and installs nothing.)
#
# Fails when a dependency's license is not accepted in about.toml, so a new
# license reaches review as a failed run, never as a silently longer file.
#
# cargo-about only ever sees crates.io/registry dependencies -- it has no idea
# `third_party/` (decision 0055: code ported or copied from codex, goose,
# rerun's re_renderer, etc. -- see third_party/README.md) exists at all, so its own
# licenses never appeared in this file, and packaging scripts never shipped them
# either. Every third_party/<name>/ directory's LICENSE*/NOTICE files (whichever
# exist -- some ported-not-copied entries, like haystack, carry only an UPSTREAM
# citation and no LICENSE, because only expression, not structure/ideas, triggers a
# license's attribution requirement) is appended below its own UPSTREAM source
# citation, so every packaged artifact (macOS .app/.dmg, Linux tarball/.deb, Windows
# .msi/.zip -- each of which ships this file) carries them too.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

if ! cargo about --version >/dev/null 2>&1; then
  echo "cargo-about is not installed: cargo install cargo-about --locked --features cli" >&2
  exit 1
fi

# cargo-about only warns, and exits 0, when a clarification in about.toml fails to
# validate (a checksum mismatch, a missing upstream file); it then falls back to the
# SPDX template, "Copyright (c) <year> <copyright holders>", which is not the notice
# MIT requires. Its log is kept and that warning fails the run.
# The notices are generated into a temporary file and moved into place only once
# they validate, so a failed run leaves the committed file as it was.
about_log=$(mktemp)
about_out=$(mktemp)
trap 'rm -f "$about_log" "$about_out"' EXIT
if ! cargo about generate \
  --config about.toml \
  --locked \
  --output-file "$about_out" \
  scripts/about.md.hbs 2>"$about_log"; then
  cat "$about_log" >&2
  exit 1
fi
cat "$about_log" >&2
if grep -q "failed to validate" "$about_log"; then
  echo "a license clarification in about.toml did not validate (see above);" \
    "THIRD_PARTY_NOTICES.md would carry a placeholder notice. Fix about.toml;" \
    "THIRD_PARTY_NOTICES.md was left unchanged." >&2
  exit 1
fi
cp "$about_out" THIRD_PARTY_NOTICES.md

{
  echo
  echo "---"
  echo
  echo "## Vendored code: ported or copied into this repository"
  echo
  echo "Code in \`third_party/\` (see \`third_party/README.md\`) is copied or ported"
  echo "directly into this repository's own source, not pulled in as a crates.io"
  echo "dependency -- \`cargo-about\` above has no visibility into it. Each entry below is"
  echo "its \`UPSTREAM\` file's own source citation (repository, commit, license, and"
  echo "what was taken), followed by the LICENSE/NOTICE text \`UPSTREAM\` says applies."
  echo

  for dir in third_party/*/; do
    name=$(basename "$dir")
    [ -f "$dir/UPSTREAM" ] || continue

    echo "### $name"
    echo
    echo '```'
    # The UPSTREAM source citation: everything up to the first blank line (repository/
    # commit/license/paths -- the format every UPSTREAM file in this repository starts
    # with; see third_party/README.md). A multi-entry UPSTREAM file (more than one
    # ported block, e.g. third_party/codex/UPSTREAM) still gets its full text below,
    # this is only the leading citation, matching this script's "with the UPSTREAM
    # source line" contract literally rather than reprinting the whole narrative twice.
    awk 'BEGIN{p=1} /^$/{if(p){exit}} {print}' "$dir/UPSTREAM"
    echo '```'
    echo

    for license_file in "$dir"LICENSE "$dir"LICENSE-*; do
      [ -f "$license_file" ] || continue
      echo "**$(basename "$license_file")** (\`$dir\`):"
      echo
      echo '```'
      cat "$license_file"
      echo '```'
      echo
    done

    if [ -f "$dir/NOTICE" ]; then
      echo "**NOTICE** (\`$dir\`):"
      echo
      echo '```'
      cat "$dir/NOTICE"
      echo '```'
      echo
    fi
  done
} >> THIRD_PARTY_NOTICES.md

echo "wrote THIRD_PARTY_NOTICES.md"
