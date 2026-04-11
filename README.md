# X-Kernel

This project is inspired by [StarryOS](https://github.com/Starry-OS/StarryOS), an experimental monolithic OS based on [ArceOS](https://github.com/arceos-org/arceos), developed by Tsinghua University.

## Supported Architectures

- [x] RISC-V 64
- [x] LoongArch64
- [x] AArch64
- [ ] x86_64 (work in progress)

## Supported Platforms
- [x] QEMU
- [x] [海光CSV环境](https://docs.opencloudos.org/OCS/Virtualization_and_Containers_Guide/CCP_Hygon_UserGuide/)
- [x] Linux kylin-x Pkvm 虚拟机环境

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

# Musl toolchain
| Architecture | GCC Version | Musl Version | Origin Link |
|--------------|-------------|--------------|-------------|
| x86_64     | 11.2.1      | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/x86_64-linux-musl-cross.tgz) |
| aarch64    | 11.2.1      | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/aarch64-linux-musl-cross.tgz) |
| riscv64    | 11.2.1      | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/riscv64-linux-musl-cross.tgz) |
| loongarch64 | 13.2.0      | 1.2.5 | [LoongsonLab](https://github.com/LoongsonLab/oscomp-toolchains-for-oskernel/releases/download/loongarch64-linux-musl-cross-gcc-13.2.0/loongarch64-linux-musl-cross.tgz) |

### 2. Config kernel

#### start from a default configuration
```bash
cp platforms/aarch64-qemu-virt/defconfig .config
```

#### change configuration
if you want to change the kernel configuration, you can use the following command to open the menuconfig interface:
```bash
make menuconfig
```

this will generate a .config file in the root directory of the project, which will be used for building the kernel.

### 3. Prepare rootfs
Directly run the following commands to build the root filesystem image for the desired architecture:
```bash
make rootfs
```
Or you can build your own root filesystem image(only supported ext4 and musl for now)

### 4. Build
You can build the kernel
```bash
make build
```
this will create a kernel image from .config

For `x86_64`, `make build` also creates boot media artifacts in the project root:

- `xkernel_<platform>.bzimg`: LinuxBoot/direct boot image
- `xkernel_<platform>.uefi.img`: UEFI FAT boot disk containing `BOOTX64.EFI`, `axboot.toml`, and the kernel ELF

### 5. Build and run on QEMU
we support directly running the kernel on QEMU

```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make run

cp platforms/x86_64-qemu-virt/defconfig .config
# Default x86_64 QEMU flow: LinuxBoot/direct boot
make run

# Optional x86_64 UEFI flow: OVMF + generated UEFI boot disk
make run UEFI=y

# x86-csv UEFI + SEV launch helper
bash scripts/start.sh
```

For the x86_64 UEFI flow, the host needs OVMF firmware files (for example `/usr/share/OVMF/OVMF_CODE_4M.fd` and `OVMF_VARS_4M.fd`).

## License
This project is now released under the Apache License 2.0. See the [LICENSE](./LICENSE) and [NOTICE](./NOTICE) files for details.
1
