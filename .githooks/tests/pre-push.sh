#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

# Exercise the pre-push staleness check without touching the network.
set -euo pipefail
HOOK="$(cd "$(dirname "$0")/.." && pwd)/pre-push"
FIXTURE_ROOT=$(mktemp -d)
export FIXTURE_ROOT
trap 'rm -rf "$FIXTURE_ROOT"' EXIT
mkdir "$FIXTURE_ROOT/bin"
cat > "$FIXTURE_ROOT/bin/git" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$1" >> "$FIXTURE_ROOT/git-calls"
case "$1" in
rev-parse) exit 0 ;;
symbolic-ref)
    [[ -n "${BRANCH_NAME:-}" ]] && printf '%s\n' "$BRANCH_NAME"
    exit "${SYMBOLIC_STATUS:-0}" ;;
fetch) exit "${FETCH_STATUS:-0}" ;;
rev-list)
    [[ "${COMPARE_FAIL:-0}" = 1 ]] && exit 99
    printf '%s\n' "${BEHIND_COUNT:-0}" ;;
*) exit 99 ;;
esac
STUB
chmod +x "$FIXTURE_ROOT/bin/git"

# The line git feeds the hook for a plain push of the checked-out branch.
PUSH_LINE='refs/heads/feat 1111111111111111111111111111111111111111 refs/heads/feat 2222222222222222222222222222222222222222'

fail() {
    printf 'FAIL: %s\n' "$1" >&2
    cat "$FIXTURE_ROOT/output" "$FIXTURE_ROOT/git-calls" >&2
    exit 1
}

# run_case NAME EXPECTED_STATUS EXPECTED_OUTPUT_SUBSTRING [EXPECTED_GIT_CALLS]
#
# Remaining behaviour comes from prefix assignments visible inside this
# function: STDIN_TEXT (overrides PUSH_LINE; the githooks(5) deletion form
# is '(delete) <zero> <remote-ref> <remote-sha>'), BEHIND_COUNT,
# FETCH_STATUS, COMPARE_FAIL, BRANCH_NAME, SYMBOLIC_STATUS, SKIP_REBASE,
# SKIP_ALL. An empty EXPECTED_OUTPUT_SUBSTRING asserts silent output; an
# omitted EXPECTED_GIT_CALLS skips the call-log check.
run_case() {
    local name=$1 expected_status=$2 expected_output=$3 expected_calls=${4:-}
    local status=0
    : > "$FIXTURE_ROOT/git-calls"
    env PATH="$FIXTURE_ROOT/bin:$PATH" \
        BRANCH_NAME="${BRANCH_NAME:-feat}" SYMBOLIC_STATUS="${SYMBOLIC_STATUS:-0}" \
        FETCH_STATUS="${FETCH_STATUS:-0}" BEHIND_COUNT="${BEHIND_COUNT:-0}" \
        COMPARE_FAIL="${COMPARE_FAIL:-0}" \
        SKIP_REBASE="${SKIP_REBASE:-0}" SKIP_ALL="${SKIP_ALL:-0}" \
        bash "$HOOK" <<<"${STDIN_TEXT:-$PUSH_LINE}" \
        > "$FIXTURE_ROOT/output" 2>&1 || status=$?
    [[ "$status" = "$expected_status" ]] || fail "$name exited $status, expected $expected_status"
    if [[ -n "$expected_output" ]]; then
        grep -q "$expected_output" "$FIXTURE_ROOT/output" || fail "$name is missing '$expected_output'"
    elif [[ -s "$FIXTURE_ROOT/output" ]]; then
        fail "$name should have stayed silent"
    fi
    if [[ -n "$expected_calls" ]]; then
        grep -q "$expected_calls" "$FIXTURE_ROOT/git-calls" || fail "$name never ran git $expected_calls"
    fi
    printf 'PASS: %s\n' "$name"
}

# The hook must have exited before refreshing origin/main.
assert_no_fetch() {
    grep -q '^fetch$' "$FIXTURE_ROOT/git-calls" \
        && { fail "$1 fetched origin/main; a guard should have exited earlier"; }
    printf 'PASS: %s (no fetch)\n' "$1"
}

BEHIND_COUNT=3 run_case blocked_when_behind 1 'make rebase' rev-list
BEHIND_COUNT=3 run_case blocked_output_names_the_base 1 'behind origin/main'
BEHIND_COUNT=0 run_case pass_when_current 0 '' ''
SKIP_REBASE=1 BEHIND_COUNT=3 run_case skip_rebase_pushes_anyway 0 '' ''
SKIP_ALL=1 BEHIND_COUNT=3 run_case skip_all_pushes_anyway 0 '' ''
FETCH_STATUS=1 BEHIND_COUNT=3 run_case offline_passes_with_warning 0 'cannot refresh'
COMPARE_FAIL=1 run_case failed_count_passes_with_warning 0 'cannot compare'

# githooks(5): a deletion push supplies '(delete)' as the local ref, so the
# hook must release it at the ref-type check before any network activity.
STDIN_TEXT='(delete) 0000000000000000000000000000000000000000 refs/heads/feat 2222222222222222222222222222222222222222' \
    run_case deletion_passes_without_fetch 0 '' rev-parse
assert_no_fetch deletion_passes_without_fetch

STDIN_TEXT='refs/tags/v0 1111111111111111111111111111111111111111 refs/tags/v0 2222222222222222222222222222222222222222' \
    run_case tag_refs_pass 0 '' rev-parse
assert_no_fetch tag_refs_pass

STDIN_TEXT="$PUSH_LINE
refs/heads/next 3333333333333333333333333333333333333333 refs/heads/next 4444444444444444444444444444444444444444" \
    run_case multi_refspec_passes 0 '' rev-parse
assert_no_fetch multi_refspec_passes

STDIN_TEXT='refs/heads/other 1111111111111111111111111111111111111111 refs/heads/other 2222222222222222222222222222222222222222' \
    run_case other_branch_passes 0 '' symbolic-ref
assert_no_fetch other_branch_passes

SYMBOLIC_STATUS=1 BRANCH_NAME= \
    run_case detached_head_passes 0 '' ''
