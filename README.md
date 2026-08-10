# X-Kernel

[中文版](README_CN.md)

X-Kernel is a multi-architecture monolithic kernel written in Rust. It provides a modular subsystem design covering memory management, process scheduling, file systems, networking, device drivers, and TEE (Trusted Execution Environment) support. The kernel targets multiple hardware architectures (AArch64, x86_64, RISC-V 64, LoongArch64) and runs on both virtual and physical platforms.

This project is inspired by [StarryOS](https://github.com/Starry-OS/StarryOS), an experimental monolithic OS based on [ArceOS](https://github.com/arceos-org/arceos), developed by Tsinghua University.

## Supported Architectures

- [x] RISC-V 64
- [x] LoongArch64
- [x] AArch64
- [x] x86_64

## Supported Platforms
- [x] QEMU
- [x] [Hygon CSV Environment](https://docs.opencloudos.org/OCS/Virtualization_and_Containers_Guide/CCP_Hygon_UserGuide/)
- [x] Linux kylin-x Pkvm Virtual Machine

## Features
- [x] Tee support

## Quick Start
### 1. Install dependencies
```bash
# Rust toolchain
rustup target add aarch64-unknown-none-softfloat

# QEMU (Debian/Ubuntu)
sudo apt install qemu-system
```

### Musl toolchain
| Architecture | GCC Version | Musl Version | Origin Link |
|--------------|-------------|--------------|-------------|
| x86_64     | 11.2.1      | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/x86_64-linux-musl-cross.tgz) |
| aarch64    | 11.2.1      | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/aarch64-linux-musl-cross.tgz) |
| riscv64    | 11.2.1      | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/riscv64-linux-musl-cross.tgz) |
| loongarch64 | 13.2.0      | 1.2.5 | [LoongsonLab](https://github.com/LoongsonLab/oscomp-toolchains-for-oskernel/releases/download/loongarch64-linux-musl-cross-gcc-13.2.0/loongarch64-linux-musl-cross.tgz) |

### 2. Config kernel

#### start from a platform defconfig
```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make defconfig
```

`make defconfig` expands the copied minimal `defconfig` into a full `.config`.

#### change configuration
If you want to change the kernel configuration, use the following command to open the menuconfig interface:
```bash
make menuconfig
```

This updates the `.config` file in the project root, which is then used for builds.

#### refresh an existing configuration after Kconfig changes
```bash
make oldconfig
```

This is the Linux-style interactive refresh flow: it reloads the current `.config` and asks you to confirm values for newly introduced symbols.

```bash
make olddefconfig
```

This is the non-interactive Linux-style refresh flow: it reloads the current `.config` and automatically fills newly introduced symbols with their Kconfig defaults.

#### save the current configuration back to a minimal defconfig
```bash
make savedefconfig
```

This writes a minimized `./defconfig` containing only values that differ from Kconfig defaults. It is useful when updating a platform defconfig after menuconfig changes.

### 3. Prepare rootfs

Download a pre-built root filesystem image:

```bash
make rootfs
make rootfs ROOTFS_VARIANT=alpine-busybox
make rootfs ROOTFS_VARIANT=debian-busybox
```

Install repository uapps into the image with `make uapps`, or perform both
steps with `make rootfs-uapps`. A custom image can be selected through
`DISK_IMG` or `xkmake run --disk-image`.

### 4. Build
You can build the kernel
```bash
make build
```
This expands `.config` when needed and creates a versioned bundle under
`target/xkmake/<platform>/<profile>/`.

### 5. Build and run on QEMU
we support directly running the kernel on QEMU

```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make defconfig
make run

cp platforms/x86_64-qemu-virt/defconfig .config
make defconfig
make run
make run UEFI=y

# x86-csv UEFI + SEV launch helper
bash scripts/start.sh
```

For x86_64, `make build` creates both `kernel.bzimg` and `kernel.uefi.img` in
the bundle. `make run` uses LinuxBoot by default; `make run UEFI=y` selects
OVMF/UEFI. Custom firmware paths can be supplied with `OVMF_CODE` and
`OVMF_VARS_TEMPLATE`.

Pass run options through `XKMAKE_ARGS`:

```bash
make run XKMAKE_ARGS='--memory 2g --smp 2 --no-net'
make run XKMAKE_ARGS='--no-vsock'
make run XKMAKE_ARGS='-- --d guest_errors'
```

Kernel unit tests use the same build-and-QEMU path:

```bash
make unittest
make unittest UNITTEST_CRATE=kvfs,kprocess
```

Successful unit-test runs generate `coverage.txt`, `coverage.info`, and
`coverage.xml` under `target/<rust-target>/<profile>/`.

Generate workspace API documentation with the configured feature set:

```bash
make doc
make doc_check_missing
```

## License
This project is now released under the Apache License 2.0. See the [LICENSE](./LICENSE) and [NOTICE](./NOTICE) files for details.
