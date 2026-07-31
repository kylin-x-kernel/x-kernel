# kernel-image-metadata — 设计文档

## 定位

`kernel-image-metadata` 拥有 X-Kernel 镜像元数据 ABI。XKMake 使用它编码
ELF note，运行中的内核使用它定位、校验并读取同一份元数据。

## 背景

构建来源信息在 Cargo 链接完成后写入 bundle ELF，以免机器名和构建时间等
诊断字段触发 kernel crate 重编译。该数据同时属于 host 打包器和 kernel
镜像，因此放在 `boot/` 下的共享 ABI crate，而不是通用 `util` crate。

## 范围

- `src/lib.rs`：section 名称、ELF note 常量、编码、校验及 kernel linker
  symbol 访问。
- `docs/design.md`：镜像 ABI、调用约束和设计决策。
- `docs/security.md`：ELF 字节与 linker range 的信任边界。

## 架构

```text
ResolvedKernelConfig
        |
        v
      XKMake ---- encode/finalize ----> kernel ELF notes
                                           |
                                           v
                              kernel-image-metadata
                                           |
                                           v
                                     early boot log
```

`.note.xkernel.build-info` 使用标准 ELF note 外壳，其固定大小 descriptor
包含版本化头部和 UTF-8 payload。`.note.gnu.build-id` 使用 SHA-256 descriptor。
两个 descriptor 都由 linker 预留并进入 load segment，XKMake 只原地写入。
finalize 后 section header 只暴露实际编码长度，但 linker symbol 所界定的固定
reserve 仍完整保留在同一 `PT_LOAD` 中，供早期内核执行有界解析。

## 调用约束 / 执行上下文

host 侧编码 API 只操作调用者提供的 slice。启用 `kernel` feature 后，读取 API
可在 allocator、调度器和中断系统初始化前调用，不分配、不阻塞，也不依赖
当前进程。它依赖最终 linker script 提供的四个边界 symbol，且对应内存必须
已经随内核只读段完成映射。

## 算法流程

1. linker 在最终地址布局中预留两个 allocatable ELF note descriptor。
2. XKMake 在 bundle 临时 ELF 中校验 section、note header 和 load segment。
3. build-info descriptor 被清零并写入小端头部、CRC32 和 UTF-8 payload。
4. Build ID descriptor 清零后，对最终 `PT_LOAD` 字节计算 SHA-256 并写回。
5. kernel 通过 linker symbol 取得 descriptor slice，校验后读取。

## 并发模型

crate 不持有可变全局状态。host 编码需要独占 slice；kernel 读取只借用最终
只读镜像，API 可重入且无需同步。

## 设计决策

- 使用 ELF note 表达镜像元数据，section 名和 owner 区分 X-Kernel 与 GNU。
- 固定 descriptor 大小保持程序段布局稳定，避免链接后增加 section 导致
  `PT_LOAD` 与 raw image 不一致。
- payload 采用文本，kernel 只需验证后输出；语义字段模型仍由 XKMake 持有。
- Build ID 覆盖最终 loadable image，而不是非运行时 section header 噪声。

## Drop / 资源释放

crate 不拥有资源。返回值均借用调用者 slice 或静态 kernel image。
