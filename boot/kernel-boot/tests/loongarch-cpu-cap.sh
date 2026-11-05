#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

set -euo pipefail

kernel=${1:?Usage: bash loongarch-cpu-cap.sh KERNEL_BIN (built with NR_CPUS=1)}
bundle="$(dirname "$kernel")/bundle.toml"
if ! grep -qx 'nr-cpus-max = 1' "$bundle"; then
    echo 'FAIL: regression requires an XKMake image built with NR_CPUS=1' >&2
    exit 1
fi
logs=$(mktemp -d)
trap 'rm -rf "$logs"' EXIT

for cpus in 1 2 4 8; do
    status=0
    timeout --kill-after=2 10 qemu-system-loongarch64 \
        -machine virt -m 1g -smp "$cpus" -kernel "$kernel" \
        -nographic -net none >"$logs/$cpus.log" 2>&1 || status=$?
    if [[ $status != 0 && $status != 124 ]] ||
        ! grep -q 'loongarch primary switched cpu=0 ' "$logs/$cpus.log" ||
        ! grep -q 'kruntime allocator ready' "$logs/$cpus.log"; then
        cat "$logs/$cpus.log"
        echo "FAIL: early boot with $cpus vCPUs (status $status)" >&2
        exit 1
    fi
    echo "PASS: early boot with $cpus vCPUs"
done
