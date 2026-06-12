#!/bin/sh
set -u

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
echo "DEBUG: SCRIPT_DIR=$SCRIPT_DIR"
if [ -d /mnt ] && [ -x /mnt/storage_test ]; then
    RUNDIR=/mnt
else
    RUNDIR="$SCRIPT_DIR"
fi
echo "DEBUG: RUNDIR=$RUNDIR"

REPEAT=${1:-1}
case "$REPEAT" in
    ''|*[!0-9]*|0)
        echo "Error: repeat must be a positive integer, got '$REPEAT'" >&2
        exit 2
        ;;
esac

run_cmd() {
    cmd="$1"
    i=1
    while [ "$i" -le "$REPEAT" ]; do
        echo "[$i/$REPEAT] Running: $cmd (cwd=$RUNDIR)"
        sh -c "cd \"$RUNDIR\" && $cmd"
        rc=$?
        echo "[$i/$REPEAT] rc=$rc for: $cmd"
        if [ "$rc" -ne 0 ]; then
            echo "Error: failed on iteration $i/$REPEAT: $cmd" >&2
            exit 1
        fi
        i=$((i + 1))
    done
}

run_cmd "./tee_test_multi_process_with_diff_objid.sh 2"
run_cmd "./tee_test_multi_process_with_diff_objid.sh 8"
run_cmd "./tee_test_multi_process_with_diff_objid.sh 12"

run_cmd "./tee_test_multi_process_with_same_objid.sh 1 1 1"
run_cmd "./tee_test_multi_process_with_same_objid.sh 1 1 6"
run_cmd "./tee_test_multi_process_with_same_objid.sh 1 6 6"
run_cmd "./tee_test_multi_process_with_same_objid.sh 1 12 12"

run_cmd "./storage_test -S3 1 4"
run_cmd "./storage_test -S3 1 8"
run_cmd "./storage_test -S3 1 12"

echo "=== All storage concurrency tests finished successfully. ==="