#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

OVMF_CODE="${OVMF_CODE:-}"
OVMF_VARS_TEMPLATE="${OVMF_VARS_TEMPLATE:-}"
if [[ -z "${OVMF_CODE}" ]]; then
    for candidate in \
        /usr/share/OVMF/OVMF_CODE_4M.fd \
        /usr/share/OVMF/OVMF_CODE.fd \
        /usr/share/edk2/x64/OVMF_CODE.fd; do
        if [[ -f "${candidate}" ]]; then
            OVMF_CODE="${candidate}"
            break
        fi
    done
fi
if [[ -z "${OVMF_VARS_TEMPLATE}" ]]; then
    for candidate in \
        /usr/share/OVMF/OVMF_VARS_4M.fd \
        /usr/share/OVMF/OVMF_VARS.fd \
        /usr/share/edk2/x64/OVMF_VARS.fd; do
        if [[ -f "${candidate}" ]]; then
            OVMF_VARS_TEMPLATE="${candidate}"
            break
        fi
    done
fi

if [[ -z "${OVMF_CODE}" || -z "${OVMF_VARS_TEMPLATE}" ]]; then
    echo "OVMF firmware not found. Install OVMF or set OVMF_CODE/OVMF_VARS_TEMPLATE." >&2
    exit 1
fi

cp platforms/x86-csv/defconfig .config
make build UEFI=y

UEFI_IMG="${ROOT_DIR}/xkernel_x86-csv.uefi.img"
OVMF_VARS="${ROOT_DIR}/target/OVMF_VARS_x86-csv.fd"
cp "${OVMF_VARS_TEMPLATE}" "${OVMF_VARS}"

SEV_SESSION_FILE="${SEV_SESSION_FILE:-cvm_1_launch_blob.bin}"
SEV_DH_CERT_FILE="${SEV_DH_CERT_FILE:-cvm_1_guest_owner_dh.cert}"
DISK_IMG="${DISK_IMG:-${ROOT_DIR}/disk.img}"
QMP_ADDR="${QMP_ADDR:-tcp:127.0.0.1:2223,server,nowait}"
VSOCK_CID="${VSOCK_CID:-104}"
HOSTFWD_PORT="${HOSTFWD_PORT:-5556}"

echo "启动 QEMU CSV + UEFI..."
exec qemu-system-x86_64 -m 1G -smp 1 -machine q35 \
    -device virtio-net-pci,netdev=net0 \
    -netdev user,id=net0,hostfwd=tcp::${HOSTFWD_PORT}-:5555,hostfwd=udp::${HOSTFWD_PORT}-:5555 \
    -nographic \
    -device vhost-vsock-pci,id=virtiosocket0,guest-cid=${VSOCK_CID} \
    -device virtio-blk-pci,drive=disk0 \
    -drive id=disk0,if=none,format=raw,file="${DISK_IMG}" \
    -cpu host -accel kvm \
    -drive if=ide,format=raw,index=0,file="${UEFI_IMG}" \
    -object sev-guest,id=sev0,policy=0x1,cbitpos=47,reduced-phys-bits=5,session-file="${SEV_SESSION_FILE}",dh-cert-file="${SEV_DH_CERT_FILE}" \
    -machine memory-encryption=sev0 \
    -drive if=pflash,format=raw,unit=0,file="${OVMF_CODE}",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="${OVMF_VARS}" \
    -qmp "${QMP_ADDR}"
