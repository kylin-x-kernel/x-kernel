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
cp platforms/aarch64-qemu-virt/defconfig .config
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
make rootfs
make rootfs ROOTFS_VARIANT=debian-busybox
make rootfs ROOTFS_VARIANT=debian-systemd
```

使用 `make uapps` 将仓库中的 uapps 安装进镜像，或使用
`make rootfs-uapps` 连续完成下载和安装。可通过 `DISK_IMG` 或
`xkmake run --disk-image` 指定其他镜像。

### 4. 构建
构建内核：
```bash
make build
```
该命令会按需展开 `.config`，并在
`target/xkmake/<platform>/<profile>/` 下创建带版本信息的 bundle。

### 5. 在 QEMU 上构建并运行
支持直接在 QEMU 上运行内核：

```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make defconfig
make run

cp platforms/x86_64-qemu-virt/defconfig .config
make defconfig
make run
make run UEFI=y

# x86-csv UEFI + SEV 启动辅助
bash scripts/start.sh
```

x86_64 的 `make build` 会在 bundle 中同时生成 `kernel.bzimg` 和
`kernel.uefi.img`。`make run` 默认使用 LinuxBoot，`make run UEFI=y`
选择 OVMF/UEFI；可通过 `OVMF_CODE` 和 `OVMF_VARS_TEMPLATE` 指定固件。

高级运行参数通过 `XKMAKE_ARGS` 传递：

```bash
make run XKMAKE_ARGS='--memory 2g --smp 2 --no-net'
make run XKMAKE_ARGS='--no-vsock'
make run XKMAKE_ARGS='-- --d guest_errors'
```

内核单元测试复用同一套构建和 QEMU 流程：

```bash
make unittest
make unittest UNITTEST_CRATE=kvfs,kprocess
```

单元测试成功运行后，会在 `target/<rust-target>/<profile>/` 下生成
`coverage.txt`、`coverage.info` 和 `coverage.xml`。

使用当前配置的 feature 集生成 workspace API 文档：

```bash
make doc
make doc_check_missing
```

## 许可证
本项目基于 Apache License 2.0 发布。详见 [LICENSE](./LICENSE) 和 [NOTICE](./NOTICE) 文件。
