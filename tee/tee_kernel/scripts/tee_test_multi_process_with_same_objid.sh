#!/bin/sh
set -u

# 用法：sh run_multi_append_share_rw.sh <obj_id_num> <writer_num> <reader_num>
OBJ_ID_NUM="${1:-1}"
WRITER_NUM="${2:-1}"
READER_NUM="${3:-1}"

if [ -z "$OBJ_ID_NUM" ] || [ -z "$WRITER_NUM" ] || [ -z "$READER_NUM" ]; then
  echo "Usage: $0 <obj_id_num> <writer_num> <reader_num>"
  exit 1
fi

# 清理历史日志，避免旧日志影响判定
rm -f output*.log 2>/dev/null || true

LOG_DIR="."
BASE="share_rw_multi_${OBJ_ID_NUM}"

echo "[main] obj_id_num=$OBJ_ID_NUM writer_num=$WRITER_NUM reader_num=$READER_NUM"
failed=0
panic_pat='panicked|assertion failed|index out of bounds'
success_pat='Success'

# 1) create（单进程，阻塞等待它结束）
echo "[create] start: writing base_data (pid will be this shell only)"
/storage_test -S2 create "$OBJ_ID_NUM" > "${LOG_DIR}/${BASE}_create.log" 2>&1
create_log="${LOG_DIR}/${BASE}_create.log"
echo "[create] done: log=${create_log}"

# 2) writers (append)
pids=""
i=1
while [ "$i" -le "$WRITER_NUM" ]; do
  LOG="${LOG_DIR}/${BASE}_w${i}.log"
  echo "[launch] writer#${i} start: pid=? append_common (log=${LOG})"
  /storage_test -S2 append "$OBJ_ID_NUM" "$i" > "$LOG" 2>&1 &
  pid=$!
  echo "[launch] writer#${i} pid=${pid} appending (log=${LOG})"
  pids="$pids $pid"
  i=$((i + 1))
done

for pid in $pids; do
  echo "[wait] writer pid=${pid} ..."
  wait "$pid" >/dev/null 2>&1 || true
  echo "[wait] writer pid=${pid} finished (wait status ignored)"
done

# 3) readers (read)
pids=""
j=1
while [ "$j" -le "$READER_NUM" ]; do
  LOG="${LOG_DIR}/${BASE}_r${j}.log"
  echo "[launch] reader#${j} start: pid=? open(no WRITE_META) read+validate (log=${LOG})"
  /storage_test -S2 read "$OBJ_ID_NUM" "$j" > "$LOG" 2>&1 &
  pid=$!
  echo "[launch] reader#${j} pid=${pid} reading (log=${LOG})"
  pids="$pids $pid"
  j=$((j + 1))
done

for pid in $pids; do
  echo "[wait] reader pid=${pid} ..."
  wait "$pid" >/dev/null 2>&1 || true
  echo "[wait] reader pid=${pid} finished (wait status ignored)"
done

#4) read after 2) and 3) finish, to print the all data in Object
/storage_test -S2 read "$OBJ_ID_NUM" "$j" > "${LOG_DIR}/${BASE}_readall.log" 2>&1

# 只根据日志判定是否成功：
# 1) 每个子进程日志必须包含 "Success"
# 2) 每个子进程日志不得出现 panic/assert/index out of bounds
check_log_ok() {
  _log="$1"
  if [ ! -f "$_log" ]; then
    failed=1
    return
  fi
  if grep -Hn -E "$panic_pat" "$_log" >/dev/null 2>&1; then
    failed=1
    return
  fi
  if ! grep -F "$success_pat" "$_log" >/dev/null 2>&1; then
    failed=1
    return
  fi
}

check_log_ok "$create_log"

i=1
while [ "$i" -le "$WRITER_NUM" ]; do
  check_log_ok "${LOG_DIR}/${BASE}_w${i}.log"
  i=$((i + 1))
done

j=1
while [ "$j" -le "$READER_NUM" ]; do
  check_log_ok "${LOG_DIR}/${BASE}_r${j}.log"
  j=$((j + 1))
done

if [ "$failed" -eq 0 ]; then
  # Color only the "[main]" and "SUCCESS" fields; keep the rest default color.
  printf '\033[34m[main]\033[0m \033[34mSUCCESS\033[0m: finished OK. Logs: %s_create.log %s_w*.log %s_r*.log\n' \
    "$BASE" "$BASE" "$BASE"
else
  # Color only the "[main]" and "FAILED" fields; keep the rest default color.
  printf '\033[31m[main]\033[0m \033[31mFAILED\033[0m: log contains panic/assert/index-out-of-bounds or missing Success marker.\n'
  echo "---- tail: ${BASE}_create.log ----"
  tail -n 30 "${LOG_DIR}/${BASE}_create.log" 2>/dev/null || true
  i=1
  while [ "$i" -le "$WRITER_NUM" ]; do
    echo "---- tail: ${BASE}_w${i}.log ----"
    tail -n 30 "${LOG_DIR}/${BASE}_w${i}.log" 2>/dev/null || true
    i=$((i + 1))
  done
  j=1
  while [ "$j" -le "$READER_NUM" ]; do
    echo "---- tail: ${BASE}_r${j}.log ----"
    tail -n 30 "${LOG_DIR}/${BASE}_r${j}.log" 2>/dev/null || true
    j=$((j + 1))
  done
fi


# Return code: success => 0, failure => 1.
if [ "$failed" -eq 0 ]; then
  exit 0
else
  exit 1
fi