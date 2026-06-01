#!/usr/bin/env bash
set -euo pipefail

arch="${1:?usage: run_tee_storage_test.sh <arch>}"
musl_target="${arch}-unknown-linux-musl"
musl_linker="${arch}-linux-musl-gcc"
target_upper="${musl_target^^}"
target_upper="${target_upper//-/_}"

: "${AUX_RUST_TOOLCHAIN:?AUX_RUST_TOOLCHAIN is required}"
: "${LIBUTEE_REPO:?LIBUTEE_REPO is required}"
: "${TARGET_DIR:?TARGET_DIR is required}"
: "${HOSTFWD_PORT:?HOSTFWD_PORT is required}"
: "${VSOCK_CID:?VSOCK_CID is required}"

libutee_dir="/xkernel-target/libutee-${arch}"
mkdir -p "${libutee_dir}"

echo "==> Syncing rust-libutee..."
if [ -d "${libutee_dir}/.git" ]; then
    git -C "${libutee_dir}" fetch --depth 1 origin HEAD
    git -C "${libutee_dir}" reset --hard FETCH_HEAD
else
    git clone --depth 1 "${LIBUTEE_REPO}" "${libutee_dir}"
fi

echo "==> Building storage_test for ${musl_target}..."
(
    cd "${libutee_dir}"
    CC="${musl_linker}" cargo +"${AUX_RUST_TOOLCHAIN}" build \
        --bin storage_test --release --target "${musl_target}"
)

echo "==> Building tee_apps/sh with TEE_INIT_APPS=/tee/storage_test..."
env \
  TEE_INIT_APPS="/tee/storage_test" \
  RUSTFLAGS= \
  CC="${musl_linker}" \
  "CARGO_TARGET_${target_upper}_LINKER=${musl_linker}" \
  cargo build --release --target "${musl_target}" --manifest-path tee_apps/sh/Cargo.toml \
  --target-dir "${TARGET_DIR}/tee-apps"

echo "==> Creating rootfs..."
env -u CARGO_BUILD_TARGET RUSTFLAGS= cargo run --release \
  --manifest-path xtask/crate_rootfs/Cargo.toml \
  --target-dir "${TARGET_DIR}/crate-rootfs" -- \
  --image disk.img --size-bytes 64M \
  --copy "${TARGET_DIR}/tee-apps/${musl_target}/release/sh":/bin/sh \
  --copy "${libutee_dir}/target/${musl_target}/release/storage_test":/tee/storage_test

echo "==> Building kernel..."
cp "platforms/${arch}-qemu-virt/defconfig" .config
make build

echo "==> Running TEE storage test..."
set +e
timeout 1200 stdbuf -oL -eL make HOSTFWD_PORT="${HOSTFWD_PORT}" VSOCK_CID="${VSOCK_CID}" justrun 2>&1 | tee tee-test-output.log
qemu_status=${PIPESTATUS[0]}
set -e

if [ "${qemu_status}" -eq 124 ]; then
    echo "TEE_RESULT: TIMEOUT" | tee -a tee-test-output.log
elif [ "${qemu_status}" -ne 0 ]; then
    echo "TEE_RESULT: QEMU_ERROR(${qemu_status})" | tee -a tee-test-output.log
fi
