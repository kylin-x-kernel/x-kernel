#!/usr/bin/env bash
set -euo pipefail

arch="${1:?usage: run_unit_tests.sh <arch>}"

ansi_filter() {
    sed -u \
        -e 's/\x1[Bb]\[[0-9;]*[a-zA-Z]//g' \
        -e 's/\x9[Bb]\[[0-9;]*[a-zA-Z]//g' \
        -e 's/\[[0-9;]*[mK]//g'
}

if [ -n "${STAGE_LOG:-}" ]; then
    exec > >(ansi_filter | tee -a "${STAGE_LOG}") 2>&1
fi

ROOTFS_VERSION=20260302
ROOTFS_CACHE="/xkernel-target/rootfs-cache"
ROOTFS_CACHED="${ROOTFS_CACHE}/rootfs-${arch}.img"
mkdir -p "${ROOTFS_CACHE}"

if [ ! -f "${ROOTFS_CACHED}" ]; then
    IMG_URL="https://gitee.com/openkylin/x-kernel-image/releases/download/${ROOTFS_VERSION}"
    curl -f -L "${IMG_URL}/rootfs-${arch}.img.xz" -o "${ROOTFS_CACHED}.xz"
    xz -df "${ROOTFS_CACHED}.xz"
fi
cp --reflink=auto "${ROOTFS_CACHED}" disk.img

TIMEOUT=480
if [ "${arch}" = "aarch64" ]; then
    TIMEOUT=481
fi

sed -i -e 's/WARN/__TEMP__/g' -e 's/ERROR/WARN/g' -e 's/__TEMP__/ERROR/g' .config

set +e
timeout "${TIMEOUT}" stdbuf -oL -eL make UNITTEST=y VSOCK=n NET=n run | ansi_filter | tee unittest-output.log
status=${PIPESTATUS[0]}
set -e

if [ "${status}" -eq 124 ]; then
    echo "Unit test timed out after ${TIMEOUT}s"
    exit 1
fi

if grep -q "UNITTEST_STATUS: TESTS_FAILED" unittest-output.log; then
    echo "Unit tests failed"
    exit 1
fi

if grep -q "UNITTEST_STATUS: ALL_TESTS_PASSED" unittest-output.log; then
    exit 0
fi

if grep -q "panicked at" unittest-output.log; then
    echo "Kernel panic detected during unit tests"
    exit 1
fi

if grep -q "test result:.*FAILED" unittest-output.log; then
    echo "Legacy unit test failure detected"
    exit 1
fi

if grep -q "test result: ok" unittest-output.log; then
    exit 0
fi

if [ "${status}" -ne 0 ]; then
    echo "Unit test command exited with status ${status}"
    exit 1
fi

echo "Unable to determine test result from unit test output"
exit 1
