#!/usr/bin/env bash
set -euo pipefail

echo "==> Checking Rust build environment..."
NIGHTLY_TOOLCHAIN="${AUX_RUST_TOOLCHAIN}"
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

mapfile -t DEFAULT_TARGETS < <(dedup_words "${XKERNEL_TARGETS[@]}" "${DEFAULT_EXTRA_TARGETS[@]}")

default_install_args=("${XKERNEL_TOOLCHAIN}" --profile minimal --no-self-update)
for component in "${XKERNEL_COMPONENTS[@]}"; do
    default_install_args+=(--component "${component}")
done
for target in "${DEFAULT_TARGETS[@]}"; do
    default_install_args+=(--target "${target}")
done

nightly_install_args=("${NIGHTLY_TOOLCHAIN}" --profile minimal --component rustfmt --no-self-update)
for target in "${NIGHTLY_TARGETS[@]}"; do
    nightly_install_args+=(--target "${target}")
done

echo "==> Installing x-kernel toolchain: ${XKERNEL_TOOLCHAIN}"
retry 3 rustup toolchain install "${default_install_args[@]}"

echo "==> Installing auxiliary nightly toolchain: ${NIGHTLY_TOOLCHAIN}"
retry 3 rustup toolchain install "${nightly_install_args[@]}"

echo "==> Active default toolchain"
cargo --version
rustc --version
rustup show active-toolchain

# The builder image ships these tools as binaries under /usr/local/cargo/bin
# (on PATH) while CARGO_HOME points at a separate cache volume, so
# `cargo install --list` cannot see them and would recompile from source on
# every run. Detect via the binary version instead of the install registry.
if [ "$(cargo-shear --version 2>/dev/null | awk '{print $NF}')" != "${CARGO_SHEAR_VERSION}" ]; then
    echo "==> Installing cargo-shear ${CARGO_SHEAR_VERSION}"
    retry 3 cargo install cargo-shear \
        --version "${CARGO_SHEAR_VERSION}" --locked --force
fi

echo "==> Dependency analyzer"
cargo-shear --version

if [ "$(licensure --version 2>/dev/null | awk '{print $NF}')" != "${LICENSURE_VERSION}" ]; then
    echo "==> Installing licensure ${LICENSURE_VERSION}"
    retry 3 cargo install licensure \
        --version "${LICENSURE_VERSION}" --locked --force
fi

echo "==> License header analyzer"
licensure --version

echo "==> Installed default targets"
rustup target list --installed

echo "==> Installed nightly targets"
rustup +"${NIGHTLY_TOOLCHAIN}" target list --installed
