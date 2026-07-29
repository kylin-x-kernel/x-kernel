#!/usr/bin/env bash
set -euo pipefail

ROOTFS_CACHE_DIR="${ROOTFS_CACHE_DIR:-${PWD}/.cache/rootfs}"
ROOTFS_VERSION="${ROOTFS_VERSION:-20260302}"

if [[ ! "${ROOTFS_VERSION}" =~ ^[A-Za-z0-9._-]+$ ]]; then
    echo "ROOTFS_VERSION contains unsupported characters: ${ROOTFS_VERSION}" >&2
    exit 2
fi

if [ "$#" -eq 0 ]; then
    echo "usage: prepare_rootfs_cache.sh <arch> [<arch> ...]" >&2
    exit 2
fi

for command in curl flock sha256sum xz; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        echo "required command not found: ${command}" >&2
        exit 1
    fi
done

mkdir -p "${ROOTFS_CACHE_DIR}"

rootfs_is_valid() {
    local image="$1"
    local digest_file="$2"
    local cached_version
    local expected
    local actual

    [ -s "${image}" ] || return 1
    [ -s "${digest_file}" ] || return 1

    read -r cached_version expected <"${digest_file}"
    [ "${cached_version}" = "${ROOTFS_VERSION}" ] || return 1
    [[ "${expected}" =~ ^[0-9a-fA-F]{64}$ ]] || return 1
    actual="$(sha256sum "${image}" | awk '{print $1}')"
    [ "${actual,,}" = "${expected,,}" ]
}

prepare_arch() (
    set -euo pipefail

    local arch="$1"
    local image="${ROOTFS_CACHE_DIR}/rootfs-${arch}.img"
    local digest_file="${image}.sha256"
    local lock_file="${image}.lock"
    local base_url="${ROOTFS_BASE_URL:-https://gitee.com/openkylin/x-kernel-image/releases/download/${ROOTFS_VERSION}}"
    local tmp_dir
    local downloaded_image
    local downloaded_xz
    local downloaded_digest

    if [[ ! "${arch}" =~ ^[A-Za-z0-9_+-]+$ ]]; then
        echo "unsupported architecture name: ${arch}" >&2
        exit 2
    fi

    exec 9>"${lock_file}"
    flock -x 9

    if rootfs_is_valid "${image}" "${digest_file}"; then
        echo "Rootfs cache hit: ${image}"
        exit 0
    fi

    rm -f -- "${image}" "${digest_file}"
    tmp_dir="$(mktemp -d "${ROOTFS_CACHE_DIR}/.rootfs-${arch}.XXXXXX")"
    trap 'rm -rf -- "${tmp_dir}"' EXIT
    downloaded_xz="${tmp_dir}/rootfs.img.xz"
    downloaded_image="${tmp_dir}/rootfs.img"
    downloaded_digest="${tmp_dir}/rootfs.img.sha256"

    echo "Downloading rootfs for ${arch} from ${base_url}"
    curl -fL \
        --retry 4 \
        --retry-all-errors \
        --connect-timeout 15 \
        --output "${downloaded_xz}" \
        "${base_url}/rootfs-${arch}.img.xz"
    xz -t "${downloaded_xz}"
    xz -dc "${downloaded_xz}" >"${downloaded_image}"
    [ -s "${downloaded_image}" ]
    printf '%s %s\n' \
        "${ROOTFS_VERSION}" \
        "$(sha256sum "${downloaded_image}" | awk '{print $1}')" \
        >"${downloaded_digest}"

    chmod 0644 "${downloaded_image}" "${downloaded_digest}"
    mv -f -- "${downloaded_image}" "${image}"
    mv -f -- "${downloaded_digest}" "${digest_file}"
    echo "Rootfs cache ready: ${image}"
)

for arch in "$@"; do
    prepare_arch "${arch}"
done
