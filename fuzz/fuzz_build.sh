#!/bin/bash
# WFUZZ local build: Kylin mirror may not ship rust-toolchain.toml version; use nightly.
set -euo pipefail

cd "$(dirname "$0")"
export RUSTUP_TOOLCHAIN=nightly

usage() {
  cat <<'EOF'
Usage: ./fuzz_build.sh

Generate entrypoints.json, merge wfuzz.json, and write entrypoints_list.txt.
Run inside the v11-2603 container as ubuntu with the WFUZZ SDK installed.

Artifacts (gitignored):
  entrypoints.json
  entrypoints_list.txt
  wfuzz-fuzz-targets/
EOF
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
  usage
  exit 0
fi

if [ ! -w . ]; then
  echo "error: current directory is not writable; run as root: chown -R ubuntu:ubuntu /code" >&2
  exit 1
fi

if ! command -v wfuzz >/dev/null 2>&1; then
  echo "error: wfuzz not found; install the WFUZZ SDK and source ~/.bashrc" >&2
  exit 1
fi

echo "==> wfuzz build (RUSTUP_TOOLCHAIN=nightly)"
wfuzz build --output-entrypoints entrypoints.json

echo "==> wfuzz entrypoints-merge"
wfuzz entrypoints-merge -w wfuzz.json -e entrypoints.json

python3 extract_entrypoints_list.py wfuzz.json entrypoints_list.txt

echo ""
echo "build complete"
echo "  entrypoints: $(tr '\n' ' ' < entrypoints_list.txt)"
echo "  run:         WFUZZ_TEST_ENTRYPOINT=fdt_parse wfuzz fuzz"
