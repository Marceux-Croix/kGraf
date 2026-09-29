#!/usr/bin/env python3
"""Fail-loud release gate: no development-only AI vendor key ends up in a
shipped artifact.

scripts/release-all.sh calls this once per extracted artifact (the tarball,
the .deb's payload, the .msi's or .zip's contents, the .dmg's mounted app
bundle) with the repository's `.env` as the source of the values that must
never appear. It prints ONLY counts -- never a key name, never a value,
never a line of matched context -- so its own output cannot leak the thing
it is checking for. Exit code is the guard itself: 0 means clean, 1 means at
least one value was found in something about to ship, 2 means the check
itself could not run (missing env file, unreadable target).

Usage:
  check-no-dev-keys.py --env-file PATH TARGET [TARGET ...]

Each TARGET is a file or a directory (scanned recursively, byte-for-byte,
so a compiled binary is checked exactly like a text file -- no assumption
that a leaked key would appear in a "readable" file).
"""
import argparse
import os
import sys

# Secrets are long (every provider key in this project's .env is 20+
# characters); a short value ("true", "1", "8080") would match constantly
# against arbitrary binary data and turn "0 matches" into noise nobody could
# trust. This floor is a deliberate trade-off, not an oversight: it is
# stated here, in scripts/release-local.md, and covered by
# scripts/test-check-no-dev-keys.sh so a future edit cannot silently change
# it without a test noticing.
MIN_VALUE_LENGTH = 8


def load_values(env_path):
    values = set()
    with open(env_path, "r", encoding="utf-8", errors="ignore") as fh:
        for line in fh:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            _, _, val = line.partition("=")
            val = val.strip()
            if len(val) >= 2 and val[0] == val[-1] and val[0] in ("'", '"'):
                val = val[1:-1]
            val = val.strip()
            if len(val) >= MIN_VALUE_LENGTH:
                values.add(val)
    return sorted(values)


def iter_files(target):
    if os.path.isfile(target):
        yield target
        return
    for root, _dirs, files in os.walk(target):
        for name in files:
            yield os.path.join(root, name)


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--env-file", required=True)
    parser.add_argument("targets", nargs="+")
    args = parser.parse_args(argv)

    if not os.path.isfile(args.env_file):
        print(f"env file not found: {args.env_file}", file=sys.stderr)
        return 2

    values = load_values(args.env_file)
    value_bytes = [v.encode("utf-8", errors="ignore") for v in values]
    counts = [0] * len(values)
    files_scanned = 0

    for target in args.targets:
        if not os.path.exists(target):
            print(f"scan target does not exist: {target}", file=sys.stderr)
            return 2
        for path in iter_files(target):
            files_scanned += 1
            try:
                with open(path, "rb") as fh:
                    data = fh.read()
            except OSError as exc:
                print(f"could not read a scan target file: {exc}", file=sys.stderr)
                return 2
            for i, vb in enumerate(value_bytes):
                counts[i] += data.count(vb)

    total = sum(counts)
    print(
        f"dev-key scan: {files_scanned} file(s), "
        f"{len(values)} value(s) of length >= {MIN_VALUE_LENGTH} from {os.path.basename(args.env_file)}"
    )
    for i, c in enumerate(counts):
        print(f"  value #{i + 1}: {c} match(es)")
    print(f"  total: {total} match(es)")
    return 1 if total > 0 else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
