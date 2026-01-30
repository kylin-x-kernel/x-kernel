# X-Kernel
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

### 2. Prepare rootfs

Directly run the following commands to build the root filesystem image for the desired architecture:
```bash
# Default target: riscv64
make rootfs
# Explicit target
make ARCH=riscv64 rootfs
make ARCH=loongarch64 rootfs
```
Or you can build your own root filesystem image(only supported ext4 and musl for now)

### 3. Build and run on QEMU
You can build and run the kernel on QEMU with the following commands:

```bash
# Default target: riscv64
make build
# Explicit target
make ARCH=riscv64 build
make ARCH=loongarch64 build

# Run on QEMU (also rebuilds if necessary)
make ARCH=riscv64 run
make ARCH=loongarch64 run
```

### 4. Build for other platforms
You can build the kernel for other supported platforms with the following commands:
```bash
make ARCH=xxx PLAT=[platforms/xxxxx] build
```

## License
This project is now released under the Apache License 2.0. See the [LICENSE](./LICENSE) and [NOTICE](./NOTICE) files for details.
