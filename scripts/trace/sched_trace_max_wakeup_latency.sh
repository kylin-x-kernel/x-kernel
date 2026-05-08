#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# 固定流程：开启 sched 事件 → 观测一段时间 → 关闭 → 向 stdout 打印两行：
#   max_wakeup_to_run_latency_ns=...
#   matched_pairs=...
#
# 观测时长仅在本文件内配置（秒）。
# 使用 POSIX sh（无 bash 依赖），便于 initrd/最小 rootfs。

set -eu

OBSERVE_SECONDS=5
PROC_ROOT=/proc

sched_events_disable() {
	printf '0\n' >"${PROC_ROOT}/tracing/events/sched/sched_wakeup/enable" 2>/dev/null || true
	printf '0\n' >"${PROC_ROOT}/tracing/events/sched/sched_switch/enable" 2>/dev/null || true
}

sched_events_enable() {
	printf '1\n' >"${PROC_ROOT}/tracing/events/sched/sched_wakeup/enable"
	printf '1\n' >"${PROC_ROOT}/tracing/events/sched/sched_switch/enable"
}

trap sched_events_disable EXIT

sched_events_enable
sleep "${OBSERVE_SECONDS}"

awk '
BEGIN { max_ns = 0; pairs = 0 }
{
	line = $0
	if (match(line, /sched_wakeup\(/) && match(line, /woken_tid=[0-9]+/)) {
		tid = substr(line, RSTART + 10, RLENGTH - 10) + 0
		tail = substr(line, RSTART + RLENGTH)
		if (match(tail, /ts_ns=[0-9]+/)) {
			ts = substr(tail, RSTART + 6, RLENGTH - 6) + 0
			pending[tid] = ts
		}
	} else if (match(line, /sched_switch\(/) && match(line, /prev_tid=[0-9]+/)) {
		if (match(line, /next_tid=[0-9]+/)) {
			next_tid = substr(line, RSTART + 9, RLENGTH - 9) + 0
			tail = substr(line, RSTART + RLENGTH)
			if (match(tail, /ts_ns=[0-9]+/)) {
				ts2 = substr(tail, RSTART + 6, RLENGTH - 6) + 0
				if (next_tid in pending) {
					ts1 = pending[next_tid]
					delete pending[next_tid]
					if (ts2 >= ts1) {
						d = ts2 - ts1
						if (d > max_ns) {
							max_ns = d
						}
						pairs++
					}
				}
			}
		}
	}
}
END {
	print "max_wakeup_to_run_latency_ns=" max_ns
	print "matched_pairs=" pairs
}
' "${PROC_ROOT}/tracing/trace"
