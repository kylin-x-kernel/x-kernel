#!/usr/bin/env bash
# Build one version of the X-Kernel CI builder image with docker buildx.
#
# Usage:
#   build.sh <os_dir>/Dockerfile_v<X.Y> [--repo <image-repo>] [--push]
#            [--platform <platforms>]
#
# The image tag is the version in the file name: <os>/Dockerfile_v3.0 becomes
# <image-repo>:v3.0; versions are unique across all <os> directories. Without
# --push the image is built for the host platform only and loaded into the
# local Docker image store. Incremental versions build on the image pinned by
# the BASE_IMAGE default in their Dockerfile.

set -euo pipefail

usage() {
    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

[ "$#" -ge 1 ] || usage
dockerfile="$1"
shift

repo="${XKERNEL_IMAGE_REPO:-x-kernel-builder}"
push=0
platform=""
while [ "$#" -gt 0 ]; do
    case "$1" in
    --repo) [ "$#" -ge 2 ] || usage; repo="$2"; shift 2 ;;
    --push) push=1; shift ;;
    --platform) [ "$#" -ge 2 ] || usage; platform="$2"; shift 2 ;;
    *) usage ;;
    esac
done

[ -f "${dockerfile}" ] || { echo "no such Dockerfile: ${dockerfile}" >&2; exit 2; }

context="$(cd "$(dirname "${dockerfile}")" && pwd)"
file_name="$(basename "${dockerfile}")"
version="${file_name#Dockerfile_}"
if [[ ! "${version}" =~ ^v[0-9]+\.[0-9]+$ ]]; then
    echo "Dockerfile name must look like Dockerfile_v<major>.<minor>" >&2
    exit 2
fi
tag="${repo}:${version}"

build_args=(
    --build-arg "IMAGE_VERSION=${version}"
    --build-arg "VCS_REF=$(git -C "${context}" rev-parse --short HEAD 2>/dev/null || echo unknown)"
)
# Incremental versions start from the previous published image, pinned by digest
# as the BASE_IMAGE default; that default is the only base configuration.
base_line="$(grep -m1 '^ARG BASE_IMAGE' "${context}/${file_name}" || true)"
if [ -n "${base_line}" ] &&
    [[ ! "${base_line}" =~ ^ARG\ BASE_IMAGE=[^[:space:]]+@sha256:[0-9a-f]{64}$ ]]; then
    echo "${file_name} is incremental; pin the base as ARG BASE_IMAGE=<image>@sha256:<digest>" >&2
    exit 2
fi

output_args=()
metadata=""
if [ "${push}" -eq 1 ]; then
    platform="${platform:-linux/amd64,linux/arm64}"
    metadata="$(mktemp)"
    trap 'rm -f "${metadata}"' EXIT
    output_args=(--push --metadata-file "${metadata}")
else
    if [[ "${platform}" == *,* ]]; then
        echo "multi-platform builds need --push; build one platform at a time locally" >&2
        exit 2
    fi
    output_args=(--load)
fi
platform_args=()
[ -z "${platform}" ] || platform_args=(--platform "${platform}")

echo "==> building ${tag} ${platform:+(${platform})}"
docker buildx build \
    --file "${context}/${file_name}" \
    --tag "${tag}" \
    "${platform_args[@]}" \
    "${build_args[@]}" \
    "${output_args[@]}" \
    "${context}"

if [ -n "${metadata}" ]; then
    digest="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["containerimage.digest"])' "${metadata}")"
    echo "==> pushed ${tag}@${digest}"
fi
