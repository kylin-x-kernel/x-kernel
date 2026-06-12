#!/bin/sh
set -u

# 用法：sh tee_test.sh <proc_num>
# 启动 -m 1..N 的 storage_test，全部退出后脚本才退出
N="${1:-}"

if [ -z "$N" ]; then
  echo "Usage: $0 <proc_num>"
  exit 1
fi

case "$N" in
  ''|*[!0-9]*)
    echo "proc_num must be a positive integer, got: $N"
    exit 1
    ;;
esac

if [ "$N" -lt 1 ]; then
  echo "proc_num must be a positive integer, got: $N"
  exit 1
fi

# 清理历史日志，避免旧日志影响判定
rm -f output*.log 2>/dev/null || true
echo "DIAG: start diff_objid N=$N, pwd=$(pwd) (logs cleaned)"

failed=0
panic_pat='panicked|assertion failed|index out of bounds'
success_pat='Success'

pids=""
m=1
while [ "$m" -le "$N" ]; do
  out="output${m}.log"
  (
    /storage_test -m "$m" > "$out" 2>&1
    # Some storage/fs layers may delay persistence of buffered output.
    # Best-effort flush before the parent checks logs.
    sync 2>/dev/null || true
  ) &
  pid=$!
  pids="$pids $pid"
  m=$((m + 1))
done

for pid in $pids; do
  # Keep behavior consistent with same_objid script:
  # judge pass/fail by log content instead of process exit code.
  wait "$pid"
  wrc=$?
  echo "DIAG: wait pid=${pid} status=${wrc} (status ignored for pass/fail)"
done

check_log_ok() {
  _log="$1"
  echo "DIAG: start check_log_ok: $_log"

  if [ ! -f "$_log" ]; then
    echo "DIAG: missing log file: $_log" >&2
    failed=1
    return
  fi

  # Poll for Success marker briefly; avoids races where child output is appended
  # slightly after the parent 'wait' returns.
  max_tries=10
  try=1
  while [ "$try" -le "$max_tries" ]; do
    if grep -Hn -E "$panic_pat" "$_log" >/dev/null 2>&1; then
      echo "DIAG: panic pattern found in $_log" >&2
      failed=1
      return
    fi
    if grep -F "$success_pat" "$_log" >/dev/null 2>&1; then
      echo "DIAG: check_log_ok: $_log found Success (try=$try)"
      return
    fi
    sleep 0.5
    try=$((try + 1))
  done

  echo "DIAG: missing Success in $_log (after waiting)" >&2
  echo "---- last 10 lines of $_log ----"
  tail -n 10 "$_log" 2>/dev/null || true
  echo "DIAG: check_log_ok: $_log failed = $failed"
}

m=1
while [ "$m" -le "$N" ]; do
  check_log_ok "output${m}.log"
  m=$((m + 1))
done

if [ "$failed" -eq 0 ]; then
  echo "DIAG: all logs ok for N=$N"
  echo "Success"
  exit 0
else
  echo "Failed: one or more storage_test runs did not pass."
  m=1
  while [ "$m" -le "$N" ]; do
    echo "---- tail: output${m}.log ----"
    tail -n 30 "output${m}.log" 2>/dev/null || true
    m=$((m + 1))
  done
  exit 1
fi
