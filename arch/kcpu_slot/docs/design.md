# kcpu_slot 设计

`kcpu_slot` 是独立于旧 `percpu` 的架构层 crate。它把 per-CPU 数据表示为
`.cpu_slot.template` 中的隐藏模板，并由链接脚本为每个 possible CPU 预留等距副本。CPU
启动时复制模板、安装架构 base register；热路径通过 `base + offset` 直接访问，
不需要哈希表或锁。

## API 约束

- `cpu_slot!` 用于按位可复制的静态模板；复杂初始化应由上层为每个 CPU 单独构造。
- `cpu_slot_cell!` 提供 `UnsafeCell<T>`，只允许在调用者已建立排他性或本地 CPU
  访问协议时使用；`CpuSlotCell::read`/`write` 提供单字读写的便捷入口，读改写
  与原子语义仍由具体架构实现补齐，不能把普通读改写误当成原子操作。
- `PinCurrentCpu` 是执行上下文契约，由 `kspin` 等真实 guard 实现；它只表达
  “CPU 固定”，不会自行关闭抢占，调度器/中断层负责创建它。
- 后续增加更强的 `CpuLocalGuard` capability trait：只有同时保证不迁移且不会发生
  相关 IRQ 重入的 guard，才能调用安全的 `CpuSlot::get`、`get_mut`、
  `CpuSlotCell::read/write` 和动态 slot 本地访问器；底层 `*_unchecked` 入口只留给
  初始化和特殊架构路径。
- `CpuSlot::get_at` 在 `T: Sync` 下开放共享远端读取；远端写入不提供无条件 API。
- `initialize_cpu` 只负责复制模板和设置寄存器，CPU online 生命周期由平台层管理。
- `CpuSlotChunk` 使用平台提供的 backing memory；`DynamicCpuSlot` 在每个 CPU 上分别
  初始化，初始化失败会回滚已构造对象，handle drop 时逐 CPU 析构。chunk 使用共享
  bump 元数据，允许多个 dynamic handle 同时存活。backing 或 stride 对齐不满足时返回
  `SlotInitError::Misaligned`，与空间耗尽 `NoSpace` 区分。

## 链接布局

根链接脚本定义模板 LMA、模板 size 和 `.cpu_slot.template`。模板位于镜像加载地址，
模板 VMA 为零；公开 descriptor 位于正常数据区。模板镜像与 CPU area 不重叠，
每个 area 按模板实际大小向上对齐到 64 字节。该命名空间与旧
`.percpu` 完全分离；调用点迁移完成后统一切换到这一命名空间，目标镜像不保留两套机制。

不支持架构（非 x86_64/aarch64/riscv/loongarch64）在非测试构建下通过
`compile_error!` 拒绝，避免 base register 访问静默失效。

RISC-V 使用 `gp` 作为 per-CPU base register，与仓库现有 `percpu 0.4` 集成一致；
`tp` 已被内核 TLS 使用，不能同时承担 per-CPU base。

## 后续扩展

动态 slot 通过 `CpuSlotChunk::alloc` 实现按 CPU 初始化和失败回滚；远端读取使用
`DynamicCpuSlot::get_remote`，只对 `T: Sync` 开放，不提供远端写入 API。

安全访问 API 的实施顺序是：先由 `kspin` 的真实 guard 实现 `PinCurrentCpu`，再为
`NoPreemptIrqSave` 实现 `CpuLocalGuard`，最后把普通 slot 访问从 `unsafe` 迁移到
guard 约束的安全方法；迁移完成后仅保留少量底层 unsafe 边界。
