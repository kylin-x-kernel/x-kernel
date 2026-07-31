# x86_64-uefi-loader

UEFI 启动加载器（x86_64），用于加载并启动内核镜像（ELF）。

## 目录结构

- `x86_64-uefi-loader/`：UEFI 应用工程
- `target/x86_64-unknown-uefi/release/x86_64-uefi-loader.efi`

## 构建

在工作区根目录执行：

- `cargo build -p x86_64-uefi-loader --target x86_64-unknown-uefi --release`
- 或者在 x86_64 平台配置下直接执行 `make build`，同时生成 LinuxBoot `bzImage` 和 UEFI FAT 启动盘

生成的 EFI 文件在：

- `target/x86_64-unknown-uefi/release/x86_64-uefi-loader.efi`
- 对应的 UEFI 启动盘产物为：
  `target/xkmake/kplat-x86_64/<profile>/kernel.uefi.img`

## 运行要求

1. `make build` 生成的 UEFI FAT 启动盘会自动放置：
   - `EFI/BOOT/BOOTX64.EFI`
   - 根目录下的 `axboot.toml`
   - 根目录下的 `kernel.elf`
2. `make run UEFI=y` 会自动使用 OVMF 从该 FAT 启动盘启动。

## 内核加载策略

- ELF：按链接虚拟地址布局解析 `PT_LOAD` 段，但在运行时为整幅镜像选择实际物理装载地址，再建立
  `identity map + linear map + higher-half KIMAGE map` 后跳入 `kernel_boot` stub。

## SEV 内存加密

启动器会通过 CPUID 0x8000_001F 检测 SEV，并在页表条目中设置 C-bit 以确保内核内存保持加密。

## 注意事项

- UEFI 路径现在通过共享 `BootInfo` 协议把启动信息传给 `kernel_boot`/内核，而不是伪造 Multiboot1。
- 线性映射偏移现在直接使用共享的 `kaddr_layout::PAGE_OFFSET`；内核仅支持 ELF 装载，且不再依赖固定物理加载地址。
