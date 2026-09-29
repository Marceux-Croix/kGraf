#!/usr/bin/env bash
# Reproduces the x86_64 run of crates/kgraf-sandbox's Landlock, seccomp
# and namespace tests from a macOS/Apple Silicon host, where the CPU is
# aarch64 and every container runtime normally available (Docker Desktop,
# Colima with `--platform linux/amd64`) executes x86_64 binaries by
# *translating instructions* on top of the same aarch64 kernel: Apple's
# Rosetta, or QEMU's user-mode (`qemu-x86_64`) emulation.
#
# That translation is NOT enough for this crate. `seccomp()` and
# `landlock_create_ruleset()` are raw syscalls whose correctness depends on
# the kernel that receives them reporting the right `AUDIT_ARCH_*` and
# resolving the right syscall-number table. An aarch64 kernel always reports
# AUDIT_ARCH_AARCH64, no matter what instruction-translation layer sits above
# it, so a seccomp filter built for TargetArch::x86_64 cannot be validated
# against instruction-level translation: this repository measured Rosetta
# returning EINVAL from SECCOMP_SET_MODE_FILTER outright, and separately
# measured the `landlock` crate reporting a bogus ABI (0) and a "fully
# incompatible access-rights" error under the same translation.
#
# The only way to get real x86_64 syscall semantics on Apple Silicon is a
# full-system emulation of an x86_64 machine (QEMU TCG, not qemu-user): a
# genuine x86_64 kernel boots and reports AUDIT_ARCH_X86_64 for real. This
# script drives exactly that, in a Colima profile dedicated to it so the
# host's normal (shared, aarch64) Colima instance is untouched.
#
# TCG (no hardware acceleration is available for a foreign guest arch on
# Apple Silicon) is slow: expect the first run's dependency compile to take
# significantly longer than on native hardware.
#
# Usage: scripts/run-x86_64-sandbox-tests.sh
# Requires: colima, docker, qemu (`brew install qemu`), and
# lima-additional-guestagents (`brew install lima-additional-guestagents`)
# on the host.

set -euo pipefail

PROFILE="kgraf-x86"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v qemu-img > /dev/null 2>&1; then
  echo "qemu-img not found: run 'brew install qemu'" >&2
  exit 1
fi

if ! colima list 2>/dev/null | grep -q "^${PROFILE} .*Running"; then
  echo "starting the genuine x86_64 Colima profile '${PROFILE}' (QEMU TCG; slow to boot) ..." >&2
  colima start --profile "${PROFILE}" --arch x86_64 --vm-type qemu \
    --cpus 4 --memory 6 --disk 40 --runtime docker
fi

# Wait for sshd inside the guest: the first boot downloads a disk image and
# installs the guest agent before it answers.
until colima -p "${PROFILE}" ssh -- true > /dev/null 2>&1; do
  echo "waiting for the ${PROFILE} guest to answer ssh ..." >&2
  sleep 5
done

DOCKER_SOCK="${HOME}/.colima/${PROFILE}/docker.sock"
export DOCKER_HOST="unix://${DOCKER_SOCK}"

until docker version > /dev/null 2>&1; do
  echo "waiting for dockerd inside ${PROFILE} ..." >&2
  sleep 5
done

# rust:*-bookworm ships git 2.39.5, below the 2.42 crates/kgraf-sandbox
# merge-back needs (MIN_GIT in src/merge/parse.rs); rust:*-trixie has 2.47.x.
# This mirrors scripts/package-linux.sh's own git-version note.
IMAGE="rust:1.95-trixie"
docker pull "${IMAGE}"

docker run --rm \
  --privileged \
  --security-opt seccomp=unconfined \
  --security-opt apparmor=unconfined \
  -v "${REPO_DIR}:/repo:ro" \
  -w /repo \
  "${IMAGE}" \
  bash -c '
    set -uo pipefail
    apt-get update -qq > /dev/null 2>&1
    apt-get install -y -qq gcc > /dev/null 2>&1
    mkdir -p /build-target
    export CARGO_TARGET_DIR=/build-target
    export CARGO_HOME=/build-target/cargo-home
    export HOME=/root
    cargo test -p kgraf-sandbox --no-fail-fast -- --test-threads=1
  '

echo
echo "To stop the profile without deleting it: colima stop --profile ${PROFILE}"
echo "To remove it entirely:                   colima delete --profile ${PROFILE} --force"
