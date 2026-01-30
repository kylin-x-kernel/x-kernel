# bootloader

UEFI 启动加载器（x86_64），用于加载并启动 `irq-kernel`（ELF 或 BIN）。

## 目录结构

- `bootloader/`：UEFI 应用工程
- `target/x86_64-unknown-uefi/release/bootloader.efi`

## 构建

在工作区根目录执行：

- `cargo build -p bootloader --target x86_64-unknown-uefi --release`

生成的 EFI 文件在：

- `target/x86_64-unknown-uefi/release/bootloader.efi`

## 运行要求

1. 将 `bootloader.efi` 和内核文件放在同一个 EFI FAT 分区根目录。
2. 内核文件名支持：
   - `\irq-kernel`
   - `\irq-kernel.bin`

## 内核加载策略

- ELF：按照段的虚拟地址加载；若虚拟地址在高半区，则按 `phys = virt - 0xffff_8000_0000_0000` 转换后加载。
- BIN：直接加载到物理地址 `0x20_0000`，入口地址取 `0xffff_8000_0020_0000`。

## SEV 内存加密

启动器会通过 CPUID 0x8000_001F 检测 SEV，并在页表条目中设置 C-bit 以确保内核内存保持加密。

## 注意事项

- 该加载器会提供 Multiboot v1 的内存映射信息给内核（`magic = 0x2BADB002`）。
- 内核需使用与 `axplat-x86-pc` 配置一致的高半区映射（`phys_virt_offset = 0xffff_8000_0000_0000`）。
