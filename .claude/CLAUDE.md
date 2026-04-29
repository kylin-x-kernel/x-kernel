# x-kernel Project Guide

## Prerequisites

- Rust toolchain `1.93.1` (see `rust-toolchain.toml`)
- Targets: `x86_64-unknown-none`, `aarch64-unknown-none-softfloat`, `riscv64gc-unknown-none-elf`, `loongarch64-unknown-none-softfloat`
- `cargo-binutils` (auto-installed by build system)
- Musl cross-compile toolchain for the target arch
- QEMU for running

## Architecture-Target Mapping

| .config ARCH | Rust Target | CROSS_COMPILE |
|---|---|---|
| `aarch64` | `aarch64-unknown-none-softfloat` | `aarch64-linux-musl-` |
| `riscv64` | `riscv64gc-unknown-none-elf` | `riscv64-linux-musl-` |
| `x86_64` | `x86_64-unknown-none` | `x86_64-linux-musl-` |
| `loongarch64` | `loongarch64-unknown-none-softfloat` | `loongarch64-linux-musl-` |

## Supported Platforms (with defconfig)

| Platform | Description |
|---|---|
| `aarch64-qemu-virt` | QEMU ARM64 Virtual Machine |
| `aarch64-crosvm-virt` | CROSVM ARM64 Virtual Machine |
| `riscv64-qemu-virt` | QEMU RISC-V 64 Virtual Machine |
| `x86_64-qemu-virt` | QEMU x86_64 (q35) |
| `x86-csv` | Hygon CSV (SEV) Platform |

`loongarch64-qemu-virt` has no defconfig — use `make menuconfig`.

## Configuration

```bash
# Copy platform default config
cp platforms/<platform>/defconfig .config

# Interactive TUI menu
make menuconfig

# Generate default config
make defconfig

# Update config after Kconfig changes
make oldconfig
```

## Build

```bash
# Build kernel (requires .config)
make build

# Verbose output
make build V=1      # -v
make build V=2      # -vv
```

### Build Outputs

| File | Description |
|---|---|
| `xkernel_<platform>.elf` | ELF binary (with debug info if DWARF=y) |
| `xkernel_<platform>.bin` | Raw binary image |
| `xkernel_<platform>.uimg` | U-Boot image (`UIMAGE=y`) |
| `xkernel_x86_64-*.bzimg` | x86_64 LinuxBoot image |
| `xkernel_x86_64-*.uefi.img` | x86_64 UEFI boot disk (`UEFI=y`) |

## Run on QEMU

> **macOS users:** Add `VSOCK=n` to disable vsock (not supported on macOS).
> e.g. `make run VSOCK=n`

```bash
# Build + run
make run

# Run without rebuilding
make justrun

# Per-platform quick start
cp platforms/aarch64-qemu-virt/defconfig .config && make run
cp platforms/riscv64-qemu-virt/defconfig .config && make run
cp platforms/x86_64-qemu-virt/defconfig .config && make run

# x86_64 UEFI boot (requires OVMF)
make run UEFI=y

# x86-csv
bash scripts/start.sh
```

### QEMU Variables

| Variable | Default | Description |
|---|---|---|
| `BLK` | `y` | Enable virtio-blk storage |
| `NET` | `y` | Enable virtio-net networking |
| `GRAPHIC` | `n` | Enable virtio-gpu display |
| `BUS` | `pci` | Device bus: `pci` or `mmio` |
| `MEM` | `1g` | Memory size |
| `ACCEL` | auto | Hardware acceleration: `y` or `n` |
| `DISK_IMG` | `$(PWD)/disk.img` | Virtual disk image path |
| `UEFI` | `n` | x86_64 UEFI boot |
| `NET_DEV` | `user` | Network backend: `user`, `tap`, `bridge` |
| `IP` | `10.0.2.15` | Guest IPv4 |
| `GW` | `10.0.2.2` | Gateway IPv4 |
| `QEMU_LOG` | `n` | QEMU log to `qemu.log` |
| `NET_DUMP` | `n` | Packet capture to `netdump.pcap` |

```bash
# Examples
make run MEM=2g NET=n GRAPHIC=y
make run NET_DEV=tap
make run QEMU_LOG=y NET_DUMP=y
```

## Test

```bash
# Host-side unit tests
make unittest

# Without fail-fast
make unittest_no_fail_fast

# Filter by crate
make unittest UNITTEST_CRATE=kfs
make unittest UNITTEST_CRATE=kfs,kprocess

# Kernel-mode tests on QEMU (with coverage)
make run UNITTEST=y
make run UNITTEST=y UNITTEST_CRATE=kfs
```

## Format, Lint & Docs

```bash
# Format (requires nightly-2026-03-08)
make fmt

# Clippy (requires .config)
make clippy

# Generate docs
make doc

# Docs + check missing docs
make doc_check_missing
```

## Debug

```bash
# Build + QEMU debug + GDB attach (breakpoint at __kplat_main)
make debug

# Disassemble kernel ELF
make disasm

# Manual: start QEMU paused
make justrun QEMU_ARGS="-s -S"
# Then attach: gdb <elf> -ex 'target remote localhost:1234'
```

## Clean

```bash
make clean       # Remove build artifacts + cargo clean
make distclean   # clean + remove .config files
make clean_c     # C object files only
```

## Rootfs

```bash
make rootfs      # Download rootfs image
make disk_img    # Create empty FAT32 disk image
```

### Add Autostart via Rootfs Only

If you want to add autostart behavior without changing kernel source files, you can inject a script into `disk.img` only.

This rootfs uses `/etc/profile` to source `/etc/profile.d/*.sh`, so a script in `/etc/profile.d/` runs on login shell startup.

```bash
# 1) Build or refresh rootfs image
make rootfs ARCH=aarch64

# 2) Prepare an autostart hook script locally
cat > /tmp/99-xkernel-autostart.sh << 'EOF'
#!/bin/sh
[ -n "${XKERNEL_AUTOSTART_DONE:-}" ] && return 0
export XKERNEL_AUTOSTART_DONE=1

echo "[autostart] profile.d startup hook triggered"
date

# Put your app startup command here
# /path/to/your/app --arg1 --arg2 &
EOF

# 3) Inject into rootfs image (ext4)
debugfs -w -R "write /tmp/99-xkernel-autostart.sh /etc/profile.d/99-xkernel-autostart.sh" disk.img

# 4) Verify file content in image
debugfs -R "cat /etc/profile.d/99-xkernel-autostart.sh" disk.img

# 5) Boot and verify
make run VSOCK=n
```

Expected runtime log contains:

```text
[autostart] profile.d startup hook triggered
```

Rollback:

```bash
debugfs -w -R "rm /etc/profile.d/99-xkernel-autostart.sh" disk.img
```

Note for fish users: if heredoc handling is unstable in your shell integration, use `printf`/`echo` pipeline to create the temporary script file.
