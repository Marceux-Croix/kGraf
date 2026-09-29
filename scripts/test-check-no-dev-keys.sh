#!/usr/bin/env bash
# Self-test for scripts/check-no-dev-keys.py, the release-all.sh dev-key
# guard. Uses only a planted FAKE value in a throwaway .env -- never a real
# key from the repository's own .env -- so this test is safe to run
# anywhere, including CI.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/kgraf-release-guard-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

FAKE_ENV="$TMP/fake.env"
cat > "$FAKE_ENV" <<'EOF'
# Not a real provider; this is a planted value for the guard's own test.
FAKE_PROVIDER_API_KEY=sk-fake-0123456789abcdef-not-a-real-key
SHORT_VALUE=abc
EOF

DIRTY_DIR="$TMP/dirty-artifact"
mkdir -p "$DIRTY_DIR/nested"
echo "some binary-ish payload" > "$DIRTY_DIR/readme.txt"
printf 'prefix-bytes sk-fake-0123456789abcdef-not-a-real-key-suffix' > "$DIRTY_DIR/nested/leaked.bin"

CLEAN_DIR="$TMP/clean-artifact"
mkdir -p "$CLEAN_DIR"
echo "nothing sensitive here" > "$CLEAN_DIR/readme.txt"

fail() {
  echo "FAIL: $1" >&2
  exit 1
}

echo "==> Case 1: clean artifact must report 0 matches and exit 0"
set +e
out="$(python3 "$ROOT/scripts/check-no-dev-keys.py" --env-file "$FAKE_ENV" "$CLEAN_DIR")"
status=$?
set -e
echo "$out"
[[ "$status" -eq 0 ]] || fail "clean artifact exited $status, expected 0"
echo "$out" | grep -q "total: 0 match(es)" || fail "clean artifact did not report 0 total matches"
echo "$out" | grep -qi "sk-fake" && fail "guard output leaked the planted value"

echo "==> Case 2: artifact with the planted fake value must report >0 and exit 1"
set +e
out="$(python3 "$ROOT/scripts/check-no-dev-keys.py" --env-file "$FAKE_ENV" "$DIRTY_DIR")"
status=$?
set -e
echo "$out"
[[ "$status" -eq 1 ]] || fail "dirty artifact exited $status, expected 1"
echo "$out" | grep -q "total: 1 match(es)" || fail "dirty artifact did not report exactly 1 total match"
echo "$out" | grep -qi "sk-fake" && fail "guard output leaked the planted value"
echo "$out" | grep -qi "FAKE_PROVIDER_API_KEY" && fail "guard output leaked the key name"

echo "==> Case 3: SHORT_VALUE (length 3, below the guard's floor) must never match"
grep -q "value #2" <<<"$out" && fail "a value below MIN_VALUE_LENGTH should not have been loaded at all"

echo "==> Case 4: missing env file is a loud failure (exit 2), not a silent pass"
set +e
python3 "$ROOT/scripts/check-no-dev-keys.py" --env-file "$TMP/does-not-exist.env" "$CLEAN_DIR" >/tmp/kgraf-guard-test-case4.out 2>&1
status=$?
set -e
[[ "$status" -eq 2 ]] || fail "missing env file exited $status, expected 2"
rm -f /tmp/kgraf-guard-test-case4.out

echo "==> All cases passed: scripts/check-no-dev-keys.py detects a planted value, never prints it, and never silently skips."
