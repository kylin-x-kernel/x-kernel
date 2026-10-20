#!/usr/bin/env bash
set -euo pipefail

LEG_ARCH=""
FMT_ONLY=0
while [ $# -gt 0 ]; do
    case "$1" in
        --leg)
            LEG_ARCH="${2:?--leg requires guest arch (x86_64, aarch64, or riscv64)}"
            shift 2
            ;;
        --fmt)
            FMT_ONLY=1
            shift
            ;;
        *)
            echo "Unknown argument: $1 (expected --fmt or --leg <arch>)" >&2
            exit 1
            ;;
    esac
done
if [ "${FMT_ONLY}" = 1 ] && [ -n "${LEG_ARCH}" ]; then
    echo "--fmt and --leg are mutually exclusive" >&2
    exit 1
fi

echo "==> Checking Rust build environment..."
if [ "${FMT_ONLY}" = 1 ]; then
    echo "==> Prepare: rustfmt toolchain only"
elif [ -n "${LEG_ARCH}" ]; then
    echo "==> Arch CI leg (${LEG_ARCH}): prepare the complete project toolchain"
fi
NIGHTLY_TOOLCHAIN="${AUX_RUST_TOOLCHAIN:?AUX_RUST_TOOLCHAIN is required}"
CARGO_SHEAR_VERSION="1.13.2"
LICENSURE_VERSION="0.8.1"

retry() {
    local attempts="$1"
    shift
    local i
    for i in $(seq 1 "${attempts}"); do
        "$@" && return 0
        if [ "${i}" = "${attempts}" ]; then
            return 1
        fi
        echo "Command failed, retrying (${i}/${attempts}): $*" >&2
        sleep 5
    done
}

# rustup downloads many components (especially on a fresh toolchains volume);
# rsproxy.cn often needs longer backoff than the generic retry() sleep.
retry_rustup_install() {
    local raw="${RUSTUP_INSTALL_RETRIES:-}"
    local attempts=6
    case "${raw}" in
        '')
            attempts=6
            ;;
        *[!0-9]*)
            echo "RUSTUP_INSTALL_RETRIES=${raw} is not a positive integer; using 6" >&2
            attempts=6
            ;;
        *)
            attempts="${raw}"
            if [ "${attempts}" -lt 1 ]; then
                echo "RUSTUP_INSTALL_RETRIES=${raw} is not a positive integer; using 1" >&2
                attempts=1
            fi
            ;;
    esac
    local i
    for i in $(seq 1 "${attempts}"); do
        if "$@"; then
            return 0
        fi
        if [ "${i}" = "${attempts}" ]; then
            return 1
        fi
        local delay=$((15 * i))
        echo "rustup install failed, retrying (${i}/${attempts}) in ${delay}s: $*" >&2
        sleep "${delay}"
    done
    return 1
}

if [ "${FMT_ONLY}" = 1 ]; then
    echo "==> Installing rustfmt toolchain: ${NIGHTLY_TOOLCHAIN}"
    retry_rustup_install rustup toolchain install \
        "${NIGHTLY_TOOLCHAIN}" --profile minimal --component rustfmt --no-self-update
    echo "==> rustfmt"
    cargo +"${NIGHTLY_TOOLCHAIN}" fmt --version
    exit 0
fi

eval "$(
python3 <<'PY_TOOLCHAIN'
import shlex
import tomllib

with open("rust-toolchain.toml", "rb") as f:
    toolchain = tomllib.load(f)["toolchain"]

def array(name, values):
    print(f"{name}=(" + " ".join(shlex.quote(v) for v in values) + ")")

print("XKERNEL_TOOLCHAIN=" + shlex.quote(toolchain["channel"]))
array("XKERNEL_COMPONENTS", toolchain.get("components", []))
array("XKERNEL_TARGETS", toolchain.get("targets", []))
PY_TOOLCHAIN
)"

DEFAULT_EXTRA_TARGETS=(
    x86_64-unknown-uefi
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-musl
    riscv64gc-unknown-linux-musl
)
NIGHTLY_TARGETS=(
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-musl
    riscv64gc-unknown-linux-musl
)

dedup_words() {
    printf '%s\n' "$@" | awk 'NF && !seen[$0]++'
}

validate_leg_arch() {
    case "${1}" in
        x86_64 | aarch64 | riscv64) return 0 ;;
        *)
            echo "Unsupported arch CI leg: ${1} (expected x86_64, aarch64, or riscv64; must match Jenkins ciMatrixArchitectures())" >&2
            exit 1
            ;;
    esac
}

leg_host_musl_target() {
    case "${1}" in
        x86_64) printf '%s\n' x86_64-unknown-linux-musl ;;
        aarch64) printf '%s\n' aarch64-unknown-linux-musl ;;
        riscv64) printf '%s\n' riscv64gc-unknown-linux-musl ;;
    esac
}

SKIP_NIGHTLY_INSTALL=""
# Rustup proxies honor every target in rust-toolchain.toml, even on a single
# architecture leg. Install them under the caller's lock so later Cargo calls
# cannot start an implicit installation alongside builds sharing this volume.
mapfile -t DEFAULT_TARGETS < <(dedup_words "${XKERNEL_TARGETS[@]}" "${DEFAULT_EXTRA_TARGETS[@]}")
if [ -n "${LEG_ARCH}" ]; then
    validate_leg_arch "${LEG_ARCH}"
    mapfile -t _leg_musl < <(leg_host_musl_target "${LEG_ARCH}")
    LEG_MUSL_TARGET="${_leg_musl[0]}"
    # riscv64 leg has no TEE stage; x86_64/aarch64 need nightly + host musl for libutee.
    if [ "${LEG_ARCH}" = riscv64 ]; then
        SKIP_NIGHTLY_INSTALL=1
    fi
fi

default_install_args=("${XKERNEL_TOOLCHAIN}" --profile minimal --no-self-update)
for component in "${XKERNEL_COMPONENTS[@]}"; do
    default_install_args+=(--component "${component}")
done
for target in "${DEFAULT_TARGETS[@]}"; do
    default_install_args+=(--target "${target}")
done

nightly_install_args=("${NIGHTLY_TOOLCHAIN}" --profile minimal --component rustfmt --no-self-update)
if [ -n "${LEG_ARCH}" ] && [ -z "${SKIP_NIGHTLY_INSTALL:-}" ]; then
    nightly_install_args+=(--target "${LEG_MUSL_TARGET}")
else
    for target in "${NIGHTLY_TARGETS[@]}"; do
        nightly_install_args+=(--target "${target}")
    done
fi

echo "==> Installing x-kernel toolchain: ${XKERNEL_TOOLCHAIN}"
retry_rustup_install rustup toolchain install "${default_install_args[@]}"

if [ -z "${SKIP_NIGHTLY_INSTALL:-}" ]; then
    echo "==> Installing auxiliary nightly toolchain: ${NIGHTLY_TOOLCHAIN}"
    retry_rustup_install rustup toolchain install "${nightly_install_args[@]}"
else
    echo "==> Skipping auxiliary nightly toolchain on ${LEG_ARCH} leg (no TEE / rustfmt on this leg)"
fi

echo "==> Active default toolchain"
cargo --version
rustc --version
rustup show active-toolchain

# Builder image ships tools under /usr/local/cargo/bin while CARGO_HOME is a
# separate cache volume. cargo-shear 1.13.2 may report `Version: dev`.
# Do not rely on `cargo install --list` alone: it needs a usable rustup
# toolchain and can fail even when /usr/local/cargo/.crates.toml is present.
# Prefer, in order: exact --version, .crates.toml / install-list metadata,
# then the image binary embedding `{package}-{version}`.
IMAGE_CARGO_BIN="/usr/local/cargo/bin"

tool_version_token() {
    local program="$1"
    "$program" --version 2>/dev/null | awk '{print $NF}'
}

# shellcheck source=scripts/ci/cargo_tool_receipts.sh
source "$(dirname "$0")/cargo_tool_receipts.sh"

# cargo install embeds registry paths like .../cargo-shear-1.13.2/... in the bin.
image_bin_embeds_version() {
    local bin="$1"
    local package="$2"
    local version="$3"
    grep -a -F "${package}-${version}" "${bin}" >/dev/null 2>&1
}

cargo_tool_ready() {
    local program="$1"
    local package="$2"
    local version="$3"
    local reported root
    local image_bin="${IMAGE_CARGO_BIN}/${program}"

    reported="$(tool_version_token "${program}" || true)"
    if [ "${reported}" = "${version}" ]; then
        return 0
    fi

    # Image/build quirk: clap reports "dev" while install metadata is correct.
    if [ "${reported}" = "dev" ]; then
        root="$(cargo_install_root_for "${program}" || true)"
        if [ -n "${root}" ] && cargo_root_has_package "${package}" "${version}" "${root}"; then
            return 0
        fi
        if cargo_root_has_package "${package}" "${version}" /usr/local/cargo; then
            return 0
        fi
        if [ -x "${image_bin}" ] && image_bin_embeds_version "${image_bin}" "${package}" "${version}"; then
            return 0
        fi
    fi

    # PATH may miss /usr/local/cargo/bin; still accept a pinned image binary.
    if [ -x "${image_bin}" ]; then
        reported="$("${image_bin}" --version 2>/dev/null | awk '{print $NF}' || true)"
        if [ "${reported}" = "${version}" ]; then
            return 0
        fi
        if [ "${reported}" = "dev" ] && {
            cargo_root_has_package "${package}" "${version}" /usr/local/cargo \
                || image_bin_embeds_version "${image_bin}" "${package}" "${version}"
        }; then
            return 0
        fi
    fi

    return 1
}

ensure_cargo_tool() {
    local program="$1"
    local package="$2"
    local version="$3"

    if cargo_tool_ready "${program}" "${package}" "${version}"; then
        return 0
    fi

    echo "==> Installing ${package} ${version}"
    retry 3 cargo install "${package}" \
        --version "${version}" --locked --force
}

ensure_cargo_tool cargo-shear cargo-shear "${CARGO_SHEAR_VERSION}"

echo "==> Dependency analyzer"
cargo-shear --version

ensure_cargo_tool licensure licensure "${LICENSURE_VERSION}"

echo "==> License header analyzer"
licensure --version

echo "==> Installed default targets"
rustup target list --installed

if [ -z "${SKIP_NIGHTLY_INSTALL:-}" ]; then
    echo "==> Installed nightly targets"
    rustup +"${NIGHTLY_TOOLCHAIN}" target list --installed
fi
