#!/usr/bin/env bash
# Share the harness release downloader, metadata, checksums and libc cache key.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
harness="${HARNESS_ROOTFS_DIR:-${repo_root}/../starry-test-harness}"
if [[ ! -x "$harness/scripts/ensure_rootfs_release_cache.sh" ]]; then
    echo "Set HARNESS_ROOTFS_DIR to the matching starry-test-harness checkout" >&2
    exit 2
fi
[[ $# -gt 0 ]] || { echo "usage: $0 <arch> [<arch> ...]" >&2; exit 2; }
export ROOTFS_CACHE_DIR="${ROOTFS_CACHE_DIR:-${repo_root}/.cache/rootfs}"
export ROOTFS_VARIANT="${ROOTFS_VARIANT:-xkernel-glibc}"
export ROOTFS_RELEASE_TAG="${ROOTFS_RELEASE_TAG:-rootfs-v20261120}"
[[ "$ROOTFS_RELEASE_TAG" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ && "$ROOTFS_RELEASE_TAG" != latest ]] || {
    echo "CI requires a fixed ROOTFS_RELEASE_TAG" >&2; exit 2;
}
export ROOTFS_LATEST_FILE="${ROOTFS_LATEST_FILE:-${harness}/rootfs/releases/${ROOTFS_RELEASE_TAG}.json}"
[[ "$ROOTFS_VARIANT" == xkernel-glibc ]] || {
    echo "CI requires ROOTFS_VARIANT=xkernel-glibc" >&2; exit 2;
}
mkdir -p "$ROOTFS_CACHE_DIR"
for arch in "$@"; do
    case "$arch" in aarch64|x86_64|riscv64) ;; *) echo "unsupported arch: $arch" >&2; exit 2 ;; esac
    # Setup and test stages can run concurrently on a shared node.
    (
        flock -x 9
        image="$("$harness/scripts/ensure_rootfs_release_cache.sh" --arch "$arch" --variant "$ROOTFS_VARIANT" | tail -n 1)"
        ARCH="$arch" "$harness/scripts/check-longevity-glibc-bins.sh" >&2
        printf '%s\n' "$image"
    ) 9>"$ROOTFS_CACHE_DIR/rootfs-$arch-$ROOTFS_VARIANT.download.lock"
done
