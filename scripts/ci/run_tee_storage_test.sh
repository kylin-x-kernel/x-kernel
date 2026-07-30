#!/usr/bin/env bash
set -euo pipefail

arch="${1:?usage: run_tee_storage_test.sh <arch>}"
musl_target="${arch}-unknown-linux-musl"
musl_linker="${arch}-linux-musl-gcc"
target_upper="${musl_target^^}"
target_upper="${target_upper//-/_}"
read -r -a tee_test_bins <<< "${TEE_TEST_BINS:-storage_test cryp_test}"

: "${AUX_RUST_TOOLCHAIN:?AUX_RUST_TOOLCHAIN is required}"
: "${LIBUTEE_REPO:?LIBUTEE_REPO is required}"
: "${TARGET_DIR:?TARGET_DIR is required}"
: "${HOSTFWD_PORT:?HOSTFWD_PORT is required}"
: "${VSOCK_CID:?VSOCK_CID is required}"

if [ "${#tee_test_bins[@]}" -eq 0 ]; then
    echo "TEE_TEST_BINS must contain at least one test binary" >&2
    exit 2
fi

libutee_dir="/xkernel-target/libutee-${arch}"
mkdir -p "${libutee_dir}"

echo "==> Syncing rust-libutee..."
if [ -d "${libutee_dir}/.git" ]; then
    git -C "${libutee_dir}" fetch --depth 1 origin HEAD
    git -C "${libutee_dir}" reset --hard FETCH_HEAD
else
    git clone --depth 1 "${LIBUTEE_REPO}" "${libutee_dir}"
fi

echo "==> Building TEE tests for ${musl_target}: ${tee_test_bins[*]}"
(
    cd "${libutee_dir}"
    for bin in "${tee_test_bins[@]}"; do
        CC="${musl_linker}" cargo +"${AUX_RUST_TOOLCHAIN}" build \
            --bin "${bin}" --release --target "${musl_target}"
    done
)

tee_init_apps=""
copy_args=(--copy "${TARGET_DIR}/tee-apps/${musl_target}/release/sh":/bin/sh)
for bin in "${tee_test_bins[@]}"; do
    app_path="/tee/${bin}"
    if [ -n "${tee_init_apps}" ]; then
        tee_init_apps+=","
    fi
    tee_init_apps+="${app_path}"
    copy_args+=(--copy "${libutee_dir}/target/${musl_target}/release/${bin}:${app_path}")
done

echo "==> Building tee_apps/sh with TEE_INIT_APPS=${tee_init_apps}..."
env \
  TEE_INIT_APPS="${tee_init_apps}" \
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
  "${copy_args[@]}"

if [ "${SKIP_KERNEL_BUILD:-0}" != "1" ]; then
    echo "==> Building kernel..."
    cp "platforms/kplat-${arch}/qemu_defconfig" .config
    make build
else
    echo "==> Reusing prebuilt kernel artifact..."
fi

echo "==> Running TEE tests..."
printf 'TEE_TEST_APPS: %s\n' "${tee_test_bins[*]}" > tee-test-output.log
set +e
timeout 1200 stdbuf -oL -eL make HOSTFWD_PORT="${HOSTFWD_PORT}" VSOCK_CID="${VSOCK_CID}" justrun 2>&1 | tee -a tee-test-output.log
qemu_status=${PIPESTATUS[0]}
set -e

if [ "${qemu_status}" -eq 124 ]; then
    echo "TEE_RESULT: TIMEOUT" | tee -a tee-test-output.log
elif [ "${qemu_status}" -ne 0 ]; then
    echo "TEE_RESULT: QEMU_ERROR(${qemu_status})" | tee -a tee-test-output.log
fi
