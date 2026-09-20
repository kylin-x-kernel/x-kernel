#!/usr/bin/env bash
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../../.." && pwd)
target=${BENCH_TARGET:-/tmp/timer-core-bench-target}
target=$(realpath -m "$target")
binary="$target/release/timer-core-bench"
mapfile -t sources < <(rg --files "$here" "$repo/task/ktimer-core/src" \
    "$repo/task/kspin/src" "$repo/util/ktime_types" "$repo/util/kerrno" \
    "$repo/api/linux_sysno" "$repo/util/unittest" "$repo/util/macros" | sort)
sources+=("$here/Cargo.lock" "$repo/Cargo.toml" "$repo/rust-toolchain.toml" \
    "$repo/arch/karch/src/cache.rs")
sources+=("$repo/arch/karch/src/"*/cache_layout.rs)

case ${1:-} in
    build)
        # Avoid the kernel's generated Cargo configuration in the caller's cwd.
        cd "${TMPDIR:-/tmp}"
        env RUSTFLAGS='--check-cfg=cfg(unittest)' cargo +nightly-2026-03-08 build \
            --manifest-path "$here/Cargo.toml" --target-dir "$target" \
            --release --locked --offline
        sha256sum "${sources[@]}" "$binary" > "$target/benchmark-inputs.sha256"
        ;;
    measure|compare)
        out=${2:?usage: run.sh measure OUTPUT_DIR [HOST_CPU] [ROUNDS] [EVENTS]}
        host_cpu=${3:-16}
        rounds=${4:-5}
        events=${5:-200000}
        if test "$1" = compare; then
            rounds=3
            events=${4:-65536}
        fi
        test -x "$binary"
        sha256sum --check "$target/benchmark-inputs.sha256" > /dev/null
        test ! -e "$out/results.csv"
        mkdir -p "$out"
        # This check supplements, not replaces, an agreed quiet window.
        if pgrep -x 'rustc|cargo|make|qemu-system-.*' > "$out/conflicts-before.txt"; then
            echo "build/QEMU present; coordinate and retry with a new output directory" >&2
            exit 1
        fi
        sha256sum "${sources[@]}" "$binary" > "$out/inputs.sha256"
        {
            date -u --iso-8601=seconds
            git -C "$repo" rev-parse HEAD
            git -C "$repo" status --short -- task/ktimer-core task/kspin util/ktime_types util/kerrno
            rustc +nightly-2026-03-08 -Vv
            uname -a
            lscpu
            printf 'host_cpu=%s rounds=%s events=%s\n' "$host_cpu" "$rounds" "$events"
            for path in "/sys/devices/system/cpu/cpu$host_cpu/cpufreq/scaling_governor" \
                "/sys/devices/system/cpu/cpu$host_cpu/topology/thread_siblings_list" \
                /sys/devices/system/cpu/intel_pstate/no_turbo; do
                if test -r "$path"; then printf '%s=' "$path"; cat "$path"; fi
            done
        } > "$out/environment.txt"
        if test "$1" = compare; then
            timeout 120 taskset -c "$host_cpu" "$binary" --compare "$events" > "$out/results.csv"
        else
            timeout 120 taskset -c "$host_cpu" "$binary" "$rounds" "$events" > "$out/results.csv"
        fi
        sha256sum --check "$out/inputs.sha256" > "$out/inputs-verified.txt"
        if pgrep -x 'rustc|cargo|make|qemu-system-.*' > "$out/conflicts-after.txt"; then
            echo "build/QEMU present at end; results require overlap review" >&2
            exit 1
        fi
        summary=summary.py
        if test "$1" = compare; then summary=comparison_summary.py; fi
        python3 "$here/$summary" "$out/results.csv" > "$out/summary.md"
        cat "$out/summary.md"
        ;;
    *) echo "usage: run.sh build | measure OUTPUT_DIR [HOST_CPU] [ROUNDS] [EVENTS] | compare OUTPUT_DIR [HOST_CPU] [EVENTS]" >&2; exit 2 ;;
esac
