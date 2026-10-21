#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

# Exercise hook dispatch without formatting files or building a kernel.
set -euo pipefail
HOOK="$(cd "$(dirname "$0")/.." && pwd)/pre-commit"
FIXTURE_ROOT=$(mktemp -d)
export FIXTURE_ROOT
trap 'rm -rf "$FIXTURE_ROOT"' EXIT
mkdir "$FIXTURE_ROOT/bin"
cat > "$FIXTURE_ROOT/bin/git" <<'STUB'
#!/usr/bin/env bash
case "$1" in
rev-parse) printf '%s\n' "$FIXTURE_ROOT" ;;
diff) printf '%s' "${STAGED_RS:-}" ;;
add) exit 0 ;;
*) exit 99 ;;
esac
STUB
cat > "$FIXTURE_ROOT/bin/make" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FIXTURE_ROOT/calls"
if [[ "$*" == check_test_layout || "$*" == clippy ]]; then
    # Clippy's Makefile prerequisite propagates the layout check's failure.
    exit "${LAYOUT_STATUS:-0}"
fi
STUB
chmod +x "$FIXTURE_ROOT/bin/git" "$FIXTURE_ROOT/bin/make"

run_case() {
    local name=$1 expected_status=$2 expected_calls=$3
    shift 3
    : > "$FIXTURE_ROOT/calls"
    local status=0
    env PATH="$FIXTURE_ROOT/bin:$PATH" SKIP_ALL=0 SKIP_FMT=0 SKIP_CLIPPY=0 \
        STAGED_RS= LAYOUT_STATUS=0 "$@" bash "$HOOK" \
        > "$FIXTURE_ROOT/output" 2>&1 || status=$?
    if [[ "$status" != "$expected_status" || "$(<"$FIXTURE_ROOT/calls")" != "$expected_calls" ]]; then
        printf 'FAIL: %s (exit %s)\n' "$name" "$status" >&2
        cat "$FIXTURE_ROOT/calls" "$FIXTURE_ROOT/output" >&2
        exit 1
    fi
    printf 'PASS: %s\n' "$name"
}

# No .config and no staged Rust: manifests/deletions still require layout checks.
run_case manifest_only_passes 0 check_test_layout
run_case manifest_only_blocks 1 check_test_layout LAYOUT_STATUS=1
run_case rust_without_config 0 $'check_test_layout\nfmt' STAGED_RS=src/lib.rs
run_case layout_failure_without_config 1 check_test_layout STAGED_RS=src/lib.rs LAYOUT_STATUS=1
run_case environment_failure_blocks 1 check_test_layout LAYOUT_STATUS=2
if ! grep -q 'toolchain/setup' "$FIXTURE_ROOT/output"; then
    echo 'FAIL: setup failure diagnostic must not blame test locations' >&2
    exit 1
fi
touch "$FIXTURE_ROOT/.config"
run_case rust_with_config 0 $'fmt\nclippy' STAGED_RS=src/lib.rs
run_case layout_failure_via_clippy 1 $'fmt\nclippy' STAGED_RS=src/lib.rs LAYOUT_STATUS=1
run_case skip_clippy_still_checks_layout 1 check_test_layout STAGED_RS=src/lib.rs SKIP_CLIPPY=1 LAYOUT_STATUS=1
run_case skip_clippy_success 0 $'check_test_layout\nfmt' STAGED_RS=src/lib.rs SKIP_CLIPPY=1
run_case skip_fmt_still_checks_layout 1 clippy STAGED_RS=src/lib.rs SKIP_FMT=1 LAYOUT_STATUS=1
run_case skip_fmt_success 0 clippy STAGED_RS=src/lib.rs SKIP_FMT=1
run_case skip_both_still_checks_layout 1 check_test_layout STAGED_RS=src/lib.rs SKIP_FMT=1 SKIP_CLIPPY=1 LAYOUT_STATUS=1
run_case explicit_skip_all 0 '' SKIP_ALL=1 LAYOUT_STATUS=1
