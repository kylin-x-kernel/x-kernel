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

ROOTFS_CACHE_DIR="${ROOTFS_CACHE_DIR:-${PWD}/.cache/rootfs}"
ROOTFS_CACHED="${ROOTFS_CACHE_DIR}/rootfs-${arch}.img"
scripts/ci/prepare_rootfs_cache.sh "${arch}"
cp --reflink=auto "${ROOTFS_CACHED}" disk.img

TIMEOUT=480
if [ "${arch}" = "aarch64" ]; then
    TIMEOUT=481
fi

if [ "${SKIP_KERNEL_BUILD:-0}" != "1" ]; then
    scripts/ci/prepare_unittest_config.sh .config
fi

set +e
make_goal=run
if [ "${SKIP_KERNEL_BUILD:-0}" = "1" ]; then
    make_goal=justrun
fi
timeout "${TIMEOUT}" stdbuf -oL -eL make UNITTEST=y VSOCK=n NET=n "${make_goal}" | ansi_filter | tee unittest-output.log
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

if grep -q "panicked at" unittest-output.log; then
    echo "Kernel panic detected during unit tests"
    exit 1
fi

if grep -q "test result:.*FAILED" unittest-output.log; then
    echo "Legacy unit test failure detected"
    exit 1
fi

if [ "${status}" -ne 0 ]; then
    echo "Unit test command or coverage generation exited with status ${status}"
    exit 1
fi

if grep -q "UNITTEST_STATUS: ALL_TESTS_PASSED" unittest-output.log; then
    exit 0
fi

if grep -q "test result: ok" unittest-output.log; then
    exit 0
fi

echo "Unable to determine test result from unit test output"
exit 1
