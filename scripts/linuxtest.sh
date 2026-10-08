#!/usr/bin/env bash
# Steps of the Linux self-test (.github/workflows/linuxtest.yml). Each step
# appends to one report, $OUT/report.md. A failed part is a result in the
# report, never a failed step: only a failed build or a missing report fails
# the job.
#
# Usage: scripts/linuxtest.sh STEP
#   build-tests   build the test binaries and list them in $OUT/bins.tsv
#   facts         kernel, AppArmor and user namespace facts, before any change
#   prepare       make an ordinary user ("$TEST_USER", no sudo) and let it read the build
#   part-a        user namespaces allowed: sandbox and install tests as that user
#   part-b        restriction on, package installed: the installed kgraf must work
#   part-c        restriction on, profile unloaded: the installed kgraf must refuse
#   finish        check that the report exists
#
# It changes the machine it runs on (sysctl, packages, a user, an AppArmor
# profile). Run it on a throwaway machine such as a GitHub-hosted runner.
#
# Variables:
#   OUT                 report folder (default linuxtest-out)
#   TEST_USER           the ordinary user (default kgtest)
#   DEB                 the package to install in part B
#   APPARMOR_PROFILE    profile name to look for in `aa-status`; default: the
#                       name inside the packaged profile file, else the file name
#   SELFCHECK_BIN       the installed program (default /usr/bin/kgraf)
# The logs belong to the user running this script, so the redirects stay outside sudo.
# shellcheck disable=SC2024,SC1091
set -uo pipefail

OUT="${OUT:-linuxtest-out}"
TEST_USER="${TEST_USER:-kgtest}"
SELFCHECK_BIN="${SELFCHECK_BIN:-/usr/bin/kgraf}"
APPARMOR_PROFILE="${APPARMOR_PROFILE:-}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
case "$OUT" in /*) ;; *) OUT="$REPO/$OUT" ;; esac
REPORT="$OUT/report.md"
LOGS="$OUT/logs"
USERSPACE="/home/$TEST_USER"
USERSPACE_PATH_EXTRA=""
mkdir -p "$OUT" "$LOGS"

say() { printf '%s\n' "$*" >>"$REPORT"; }

# The last 4 KB of a log, in a code block.
tail_block() {
  say '```'
  tail -c 4096 "$1" >>"$REPORT"
  say '```'
}

sysctl_value() {
  cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || echo "absent"
}

# run_as_user LOG DIR PROGRAM [ARGS...]: run PROGRAM as the ordinary user in a
# clean environment, with its output in LOG. Returns the program's exit code.
run_as_user() {
  local log="$1" dir="$2"
  shift 2
  sudo -u "$TEST_USER" env -i \
    HOME="$USERSPACE" TMPDIR="$USERSPACE/tmp" \
    PATH="$USERSPACE_PATH_EXTRA/usr/local/bin:/usr/bin:/bin" \
    LANG=C.UTF-8 RUST_BACKTRACE=0 \
    bash -c 'cd "$1" && shift && exec "$@"' _ "$dir" "$@" >"$log" 2>&1
}

# result TITLE COMMAND-TEXT EXIT LOG: one section of the report. Pass is exit 0.
result() {
  local title="$1" cmd="$2" code="$3" log="$4" verdict=PASS
  [[ "$code" == 0 ]] || verdict=FAIL
  say ""
  say "### $title: $verdict"
  say ""
  say "- command: \`$cmd\`"
  say "- exit code: $code"
  local lines
  lines="$(grep -E '^test result:|^error: test failed|panicked at' "$log" | head -20)"
  if [[ -n "$lines" ]]; then
    say "- libtest summary:"
    say '```'
    say "$lines"
    say '```'
  fi
  # A test that prints "skipped:" passed without testing; show it.
  local skipped
  skipped="$(grep -c '^skipped:' "$log" || true)"
  if [[ "$skipped" != 0 ]]; then
    say "- WARNING: $skipped test(s) printed \`skipped:\` and tested nothing:"
    say '```'
    grep '^skipped:' "$log" | sort | uniq -c >>"$REPORT"
    say '```'
  fi
  if [[ "$verdict" == FAIL ]]; then
    say "- last 4 KB of output:"
    tail_block "$log"
  fi
}

# The package file and profile name inside the installed package, if any.
profile_file() {
  dpkg -L kgraf 2>/dev/null | grep '^/etc/apparmor.d/' | while read -r f; do
    [[ -f "$f" ]] && echo "$f" && break
  done
}

profile_name() {
  if [[ -n "$APPARMOR_PROFILE" ]]; then
    echo "$APPARMOR_PROFILE"
    return
  fi
  local f name
  f="$(profile_file)"
  [[ -n "$f" ]] || return 0
  name="$(sed -nE 's/^[[:space:]]*profile[[:space:]]+("[^"]+"|[^[:space:]]+).*/\1/p' "$f" | head -1 | tr -d '"')"
  echo "${name:-$(basename "$f")}"
}

step_build_tests() {
  local json="$OUT/build-tests.json"
  cargo test -j 3 --no-run --message-format=json -p kgraf-sandbox -p kgraf-host >"$json" 2>"$LOGS/build-tests.log" || {
    tail -c 4096 "$LOGS/build-tests.log" >&2
    exit 1
  }
  # package, target name, target kind, executable
  jq -r 'select(.reason == "compiler-artifact" and .profile.test == true and .executable != null)
         | [(.target.src_path | capture("/crates/(?<n>[a-z-]+)/") | .n), .target.name, .target.kind[0], .executable] | @tsv' \
    "$json" >"$OUT/bins.tsv"
  rm -f "$json"
  cat "$OUT/bins.tsv"
}

step_facts() {
  {
    echo "# kGraf Linux self-test"
    echo ""
    echo "Commit: $(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)"
    echo "Date (UTC): $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo ""
    echo "## Facts, before any change"
    echo ""
  } >"$REPORT"
  local facts="$LOGS/facts.txt" unshare_code=0
  {
    echo "kernel: $(uname -srmv)"
    echo "distribution: $(. /etc/os-release && echo "$PRETTY_NAME")"
    echo "runner user: $(id)"
    echo "kernel.apparmor_restrict_unprivileged_userns: $(sysctl_value)"
    echo "user.max_user_namespaces: $(cat /proc/sys/user/max_user_namespaces 2>/dev/null || echo absent)"
    if command -v aa-status >/dev/null 2>&1; then
      if sudo aa-status --enabled 2>/dev/null; then
        echo "aa-status --enabled: yes (AppArmor is enabled)"
      else
        echo "aa-status --enabled: no (exit $?)"
      fi
    else
      echo "aa-status: not installed"
    fi
    echo "apparmor package: $(dpkg-query -W -f='${Version}' apparmor 2>/dev/null || echo not installed)"
    echo "apparmor_parser: $(apparmor_parser --version 2>/dev/null | head -1 || echo not installed)"
    echo "git: $(git --version)"
    echo "free disk: $(df -h / | awk 'NR==2 {print $4 " of " $2}')"
    echo "node: $(node --version 2>/dev/null || echo none), npm: $(npm --version 2>/dev/null || echo none)"
    echo "curl: $(curl --version 2>/dev/null | head -1 || echo none)"
  } >"$facts"
  unshare -Ur true >>"$facts" 2>&1 || unshare_code=$?
  {
    echo "unshare -Ur true as the runner user: exit $unshare_code"
    echo ""
    echo '```'
    cat "$facts"
    echo '```'
  } >>"$REPORT"
}

step_prepare() {
  if ! id "$TEST_USER" >/dev/null 2>&1; then
    sudo useradd --create-home --shell /bin/bash "$TEST_USER"
  fi
  sudo -u "$TEST_USER" mkdir -p "$USERSPACE/tmp"
  # The test binaries hold the absolute path of their source folder, and the
  # tests read fixtures there. Rather than move the tree, make it readable
  # where it is: every folder above it searchable, everything in it readable.
  local d="$REPO"
  while [[ "$d" != "/" ]]; do
    sudo chmod o+x "$d"
    d="$(dirname "$d")"
  done
  sudo chmod -R o+rX "$REPO"
  # The test user runs cargo for the real-build tests: make the pinned
  # toolchain readable and every folder above it searchable.
  local sysroot d2
  sysroot="$(dirname "$(toolchain_bin)")"
  sudo chmod -R o+rX "$sysroot"
  d2="$sysroot"
  while [[ "$d2" != "/" ]]; do
    d2="$(dirname "$d2")"
    sudo chmod o+x "$d2"
  done
  local npm_dir
  npm_dir="$(dirname "$(command -v npm 2>/dev/null || echo /usr/bin/npm)")"
  case "$npm_dir" in /usr/bin | /usr/local/bin | /bin) ;; *) sudo chmod o+x "$npm_dir" ;; esac
  {
    echo "## Setup"
    echo ""
    echo "- ordinary user: \`$(id "$TEST_USER")\`"
    echo "- sudo rights of that user: $(sudo -l -U "$TEST_USER" 2>&1 | tail -1)"
    echo ""
  } >>"$REPORT"
}

# The npm folder, when it is not on the clean PATH already.
# The pinned toolchain's own bin folder (the real cargo and rustc, not the
# rustup proxies, which would need the runner's rustup home).
toolchain_bin() {
  (cd "$REPO" && echo "$(rustc --print sysroot)/bin")
}

user_path_extra() {
  local npm_dir
  echo -n "$(toolchain_bin):"
  npm_dir="$(dirname "$(command -v npm 2>/dev/null || echo /usr/bin/npm)")"
  case "$npm_dir" in /usr/bin | /usr/local/bin | /bin) ;; *) echo "$npm_dir:" ;; esac
}

# bin PACKAGE TARGET: the test executable of a target, from bins.tsv.
bin() {
  awk -F'\t' -v p="$1" -v t="$2" '$1 == p && $2 == t {print $4; exit}' "$OUT/bins.tsv"
}

step_part_a() {
  USERSPACE_PATH_EXTRA="$(user_path_extra)"
  say ""
  say "## Part A: user namespaces allowed, tests as \`$TEST_USER\`"
  say ""
  say "Why: the sandbox's proxy-limited network and hidden folders need unprivileged user"
  say "namespaces. Earlier runs used a root container; this part uses an ordinary user."
  local sysctl_code=0
  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 >"$LOGS/part-a-sysctl.log" 2>&1 || sysctl_code=$?
  say ""
  say "- \`sysctl -w kernel.apparmor_restrict_unprivileged_userns=0\`: exit $sysctl_code, now \`$(sysctl_value)\`"
  local unshare_code=0
  run_as_user "$LOGS/part-a-unshare.log" "$USERSPACE" unshare -Ur true || unshare_code=$?
  say "- \`unshare -Ur true\` as \`$TEST_USER\`: exit $unshare_code"

  local t exe code dir
  for t in egress escape_linux workspace merge line_endings; do
    exe="$(bin kgraf-sandbox "$t")"
    dir="$REPO/crates/kgraf-sandbox"
    code=0
    if [[ -z "$exe" ]]; then
      say ""
      say "### kgraf-sandbox $t: FAIL"
      say ""
      say "- no test binary in the build list"
      continue
    fi
    run_as_user "$LOGS/part-a-$t.log" "$dir" "$exe" --test-threads=2 --nocapture || code=$?
    result "kgraf-sandbox $t" "$exe --test-threads=2 --nocapture" "$code" "$LOGS/part-a-$t.log"
  done

  exe="$(bin kgraf-host kgraf_host)"
  dir="$REPO/crates/kgraf-host"
  code=0
  if [[ -z "$exe" ]]; then
    say ""
    say "### kgraf-host dependency install tests: FAIL"
    say ""
    say "- no test binary in the build list"
    return
  fi
  run_as_user "$LOGS/part-a-host-install.log" "$dir" "$exe" deps::install::tests:: --test-threads=2 --nocapture || code=$?
  result "kgraf-host dependency install tests" "$exe deps::install::tests:: --test-threads=2 --nocapture" "$code" "$LOGS/part-a-host-install.log"
}

selfcheck_as_user() {
  # selfcheck_as_user LOG: the installed program, as the ordinary user.
  run_as_user "$1" "$USERSPACE" "$SELFCHECK_BIN" sandbox selfcheck
}

step_part_b() {
  USERSPACE_PATH_EXTRA="$(user_path_extra)"
  say ""
  say "## Part B: restriction on, package installed"
  say ""
  say "Why: this is a user's machine on Ubuntu 23.10 or later. The package must carry a"
  say "profile that grants user namespaces to kGraf, and the installed program must work."
  local sysctl_code=0
  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1 >"$LOGS/part-b-sysctl.log" 2>&1 || sysctl_code=$?
  say ""
  say "- \`sysctl -w kernel.apparmor_restrict_unprivileged_userns=1\`: exit $sysctl_code, now \`$(sysctl_value)\`"
  local unshare_code=0
  run_as_user "$LOGS/part-b-unshare.log" "$USERSPACE" unshare -Ur true || unshare_code=$?
  say "- control, \`unshare -Ur true\` as \`$TEST_USER\` (expected to fail when the restriction is on): exit $unshare_code"

  local deb_code=0
  : >"$LOGS/part-b-dpkg.log"
  if [[ -z "${DEB:-}" || ! -f "${DEB:-}" ]]; then
    say "- package: none found (DEB=\`${DEB:-}\`)"
    deb_code=1
  else
    sudo dpkg -i "$DEB" >"$LOGS/part-b-dpkg.log" 2>&1 || deb_code=$?
  fi
  result "install the package" "sudo dpkg -i ${DEB:-}" "$deb_code" "$LOGS/part-b-dpkg.log"
  if [[ "$deb_code" != 0 ]]; then
    return
  fi
  local file name
  file="$(profile_file)"
  name="$(profile_name)"
  say ""
  say "- AppArmor files in the package: \`${file:-none}\`"
  say "- profile name looked for: \`${name:-none}\`"
  local loaded=0
  if [[ -n "$name" ]] && sudo aa-status 2>/dev/null | grep -F -- "$name" >"$LOGS/part-b-aa-status.log"; then
    loaded=1
  fi
  if [[ "$loaded" == 1 ]]; then
    say "- loaded in the kernel: yes"
    say '```'
    cat "$LOGS/part-b-aa-status.log" >>"$REPORT"
    say '```'
  else
    say "- loaded in the kernel: NO (the package carries no profile that is loaded after install)"
  fi

  local code=0
  selfcheck_as_user "$LOGS/part-b-selfcheck.log" || code=$?
  result "installed program, sandbox self-check" "$SELFCHECK_BIN sandbox selfcheck (as $TEST_USER)" "$code" "$LOGS/part-b-selfcheck.log"
  if [[ "$code" == 0 ]]; then
    say "- output:"
    tail_block "$LOGS/part-b-selfcheck.log"
  fi
}

step_part_c() {
  USERSPACE_PATH_EXTRA="$(user_path_extra)"
  say ""
  say "## Part C: restriction on, profile unloaded"
  say ""
  say "Why: a machine where the profile is missing or not loaded must get a clear refusal"
  say "that names the fix, not a hang or a silent weaker sandbox."
  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1 >/dev/null 2>&1
  local file name unload_code=0
  file="$(profile_file)"
  name="$(profile_name)"
  if [[ -n "$file" ]]; then
    sudo apparmor_parser -R "$file" >"$LOGS/part-c-unload.log" 2>&1 || unload_code=$?
    say ""
    say "- \`apparmor_parser -R $file\`: exit $unload_code"
    if [[ -n "$name" ]] && sudo aa-status 2>/dev/null | grep -qF -- "$name"; then
      say "- WARNING: the profile \`$name\` is still listed by aa-status after the unload"
    fi
  else
    say ""
    say "- no AppArmor file in the package, so there is nothing to unload; the run below is the no-profile case"
  fi

  local code=0
  selfcheck_as_user "$LOGS/part-c-selfcheck.log" || code=$?
  local verdict=PASS
  local named="no"
  if grep -qiE 'apparmor|sysctl|profile' "$LOGS/part-c-selfcheck.log"; then
    named="yes"
  fi
  # Pass: a non-zero exit with kGraf's own refusal about user namespaces.
  if [[ "$code" == 0 ]] || ! grep -q 'user namespaces' "$LOGS/part-c-selfcheck.log"; then
    verdict=FAIL
  fi
  say ""
  say "### installed program without the profile, sandbox self-check: $verdict"
  say ""
  say "- command: \`$SELFCHECK_BIN sandbox selfcheck (as $TEST_USER)\`"
  say "- exit code: $code (expected: not 0, with a message about user namespaces)"
  say "- the message names AppArmor, the sysctl or the profile as the fix: $named"
  say "- output:"
  tail_block "$LOGS/part-c-selfcheck.log"

  if [[ -n "$file" ]]; then
    local reload_code=0
    sudo apparmor_parser -r "$file" >"$LOGS/part-c-reload.log" 2>&1 || reload_code=$?
    say ""
    say "- \`apparmor_parser -r $file\` (reload): exit $reload_code"
  fi
}

step_finish() {
  if [[ ! -s "$REPORT" ]]; then
    echo "no report at $REPORT" >&2
    exit 1
  fi
  say ""
  say "## Not covered"
  say ""
  say "The desktop app window, other distributions, and a machine with a different"
  say "AppArmor version. See docs/LINUX-TESTING.md."
  cat "$REPORT"
}

case "${1:-}" in
  build-tests) step_build_tests ;;
  facts) step_facts ;;
  prepare) step_prepare ;;
  part-a) step_part_a ;;
  part-b) step_part_b ;;
  part-c) step_part_c ;;
  finish) step_finish ;;
  *)
    echo "usage: $0 build-tests|facts|prepare|part-a|part-b|part-c|finish" >&2
    exit 2
    ;;
esac
