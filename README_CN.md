# X-Kernel

X-Kernel 是一个使用 Rust 编写的多架构单体内核。它提供模块化的子系统设计，涵盖内存管理、进程调度、文件系统、网络、设备驱动以及 TEE（可信执行环境）支持。内核面向多种硬件架构（AArch64、x86_64、RISC-V 64、LoongArch64），可在虚拟和物理平台上运行。

本项目受 [StarryOS](https://github.com/Starry-OS/StarryOS) 启发，StarryOS 是清华大学基于 [ArceOS](https://github.com/arceos-org/arceos) 开发的实验性单体操作系统。

## 支持的架构

- [x] RISC-V 64
- [x] LoongArch64
- [x] AArch64
- [x] x86_64

## 支持的平台

- [x] QEMU
- [x] [海光 CSV 环境](https://docs.opencloudos.org/OCS/Virtualization_and_Containers_Guide/CCP_Hygon_UserGuide/)
- [x] Linux kylin-x Pkvm 虚拟机环境

## 特性

- [x] TEE 支持

## 快速开始

### 1. 安装依赖

```bash
# Rust 工具链
rustup target add aarch64-unknown-none-softfloat

# QEMU (Debian/Ubuntu)
sudo apt install qemu-system
```

### Musl 工具链

| 架构 | GCC 版本 | Musl 版本 | 下载链接 |
|------|----------|-----------|----------|
| x86_64     | 11.2.1      | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/x86_64-linux-musl-cross.tgz) |
| aarch64    | 11.2.1      | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/aarch64-linux-musl-cross.tgz) |
| riscv64    | 11.2.1      | git-b76f37f (2021-09-23) | [musl.cc](https://musl.cc/riscv64-linux-musl-cross.tgz) |
| loongarch64 | 13.2.0      | 1.2.5 | [LoongsonLab](https://github.com/LoongsonLab/oscomp-toolchains-for-oskernel/releases/download/loongarch64-linux-musl-cross-gcc-13.2.0/loongarch64-linux-musl-cross.tgz) |

### 2. 配置内核

#### 从平台 defconfig 开始

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
```

`make defconfig` 会将复制的最小 `defconfig` 展开为完整的 `.config`。

#### 修改配置

如需修改内核配置，可使用以下命令打开 menuconfig 界面：

```bash
make menuconfig
```

此操作会更新项目根目录下的 `.config` 文件，构建时将使用该文件。

#### Kconfig 变更后刷新已有配置

```bash
make oldconfig
```

这是 Linux 风格的交互式刷新流程：重新加载当前 `.config`，并要求确认新增符号的值。

```bash
make olddefconfig
```

这是非交互式的 Linux 风格刷新流程：重新加载当前 `.config`，并自动使用 Kconfig 默认值填充新增符号。

#### 将当前配置保存回最小 defconfig

```bash
make savedefconfig
```

此命令会写入一个最小化的 `./defconfig`，仅包含与 Kconfig 默认值不同的条目。适用于 menuconfig 修改后更新平台 defconfig。

### 3. 准备 rootfs

下载预构建的根文件系统镜像：

```bash
make rootfs                        # alpine-busybox (musl, 默认)
make rootfs ROOTFS_VARIANT=debian-busybox # debian-busybox (glibc)
make rootfs ROOTFS_VARIANT=debian-systemd # debian-systemd (glibc)
```

也可以自行构建根文件系统镜像（目前仅支持 ext4 和 musl）。

### 4. 构建

构建内核：

```bash
make build
```

此命令会根据 .config 创建内核镜像。

对于 `x86_64`，`make build` 还会在项目根目录生成引导介质文件：

- `xkernel_<arch>-<machine>.bzimg`：LinuxBoot/直接引导镜像
- `xkernel_<arch>-<machine>.uefi.img`：UEFI FAT 引导盘，包含 `BOOTX64.EFI`、`axboot.toml` 和内核 ELF

### 5. 在 QEMU 上构建并运行

支持直接在 QEMU 上运行内核：

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make run

cp platforms/kplat-x86_64/qemu_defconfig .config
# 默认 x86_64 QEMU 流程：LinuxBoot/直接引导
make defconfig
make run

# 可选 x86_64 UEFI 流程：OVMF + 生成的 UEFI 引导盘
make run UEFI=y

# x86_64 CSV (SEV) UEFI 启动辅助
bash scripts/start.sh
```

x86_64 UEFI 流程需要主机上安装 OVMF 固件文件（例如 `/usr/share/OVMF/OVMF_CODE_4M.fd` 和 `OVMF_VARS_4M.fd`）。

## 许可证

本项目基于 Apache License 2.0 发布。详见 [LICENSE](./LICENSE) 和 [NOTICE](./NOTICE) 文件。
