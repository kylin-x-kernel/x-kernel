# kcpu_slot 设计

unsafe 边界的现状、目标能力模型和分阶段收敛方案见
[`unsafe-convergence.md`](unsafe-convergence.md)。

`kcpu_slot` 是独立于旧 `percpu` 的架构层 crate。它把 per-CPU 数据表示为
`.cpu_slot.template` 中的隐藏模板，并由链接脚本为每个 possible CPU 预留等距副本。CPU
启动时复制模板、安装架构 base register；热路径通过 `base + offset` 直接访问，
不需要哈希表或锁。

## API 约束

- `cpu_slot!` 用于按位可复制的静态模板；复杂初始化应由上层为每个 CPU 单独构造。
- `cpu_slot_cell!` 提供 `UnsafeCell<T>`，只允许在调用者已建立排他性或本地 CPU
  访问协议时使用；`CpuSlotCell::read`/`write` 提供单字读写的便捷入口，读改写
  与原子语义仍由具体架构实现补齐，不能把普通读改写误当成原子操作。
- `CpuPin` 是执行上下文契约，由上层 guard 实现：`kspin::IrqSave`、
  `kspin::NoPreemptIrqSave`、`kspin::NoPreempt` 均已实现（关闭 `preempt`、仅
  协作式调度时 `NoPreempt` 同样固定当前 CPU）。它只表达“CPU 固定”，不会自行
  关闭抢占；调度器/中断层负责创建 guard。`kspin` guard 经 `kspin::CurrentCpuIf`
  （由 `kplat` 提供）取得当前逻辑 CPU——这些是 `kspin`/`kplat` 之间的上层契约，
  `kcpu_slot` 只定义 `CpuPin`/`CpuLocalGuard` trait，自身不依赖 `kspin`/`kplat`。
- `CpuLocalGuard` capability trait：要求不迁移且本地 IRQ 不重入，当前唯一实现是
  `kspin::NoPreemptIrqSave`。安全访问器 `CpuSlot::get`、`CpuSlotCell::get`、
  `read`/`write`、`current_shared`/`shared_current` 等只对它或 `T: Sync` 开放；
  通用可变引用只保留显式 unsafe 的 `get_mut_unchecked`。`CpuLocalGuard` 只证明
  执行上下文互斥，不能证明不同嵌套 guard 之间的对象唯一借用。
- 底层 `*_unchecked` 入口保留 `unsafe` 与更宽的 `CpuPin` 约束，只留给
  初始化和特殊架构路径。
- `CpuSlot::at`、`CpuSlotCell::shared_at`、`CpuSlot::current_shared` 与
  `DynamicCpuSlot::get_remote` 在 `T: Sync` 下开放共享读取；远端写入不提供
  无条件 API。
- `begin_cpu_init` 复制模板并设置寄存器，`BootCpuToken::publish` 在 CPU-local
  初始化结束后发布可供远端访问的 checked `CpuArea`。
- `CpuSlotChunk` 使用平台提供的 backing memory；`DynamicCpuSlot` 在每个 CPU 上分别
  初始化，初始化失败会回滚已构造对象，handle drop 时逐 CPU 析构。chunk 使用共享
  bump 元数据，允许多个 dynamic handle 同时存活。backing 或 stride 对齐不满足时返回
  `SlotInitError::Misaligned`，与空间耗尽 `NoSpace` 区分。除类型对齐外，backing 基址
  与 stride 还必须是 `CPU_SLOT_AREA_ALIGN` 的倍数，否则不同 CPU 的对象会共享 cache
  line。
- `DynamicCpuSlot` handle 在 `T: Send` 下可跨线程共享（`Sync`）：handle 构造后字段
  不可变，各线程经 guard 只观察各自 CPU 的实例；唯一跨线程共享单值的
  `get_remote` 另行要求 `T: Sync`。chunk 本身因 bump cursor 保持非 `Sync`，
  分配结束后通过泄漏的 `&'static CpuSlotChunk` 固定 handle 生命周期。

## 链接布局

根链接脚本定义模板 LMA、模板 size 和 `.cpu_slot.template`。模板位于镜像加载地址，
模板 VMA 为零；公开 descriptor 位于正常数据区。模板镜像与 CPU area 不重叠。

area stride 为模板实际大小向上对齐到 `CPU_SLOT_AREA_ALIGN`，area 起始地址也按同一
值对齐；该常量由 `kaddr_layout` 提供、链接脚本经 `%CPU_SLOT_AREA_ALIGN%` 替换，Rust
侧 `stride()` 与链接器不会漂移。它取受支持平台最大数据 cache line 的上界（当前
128B），因此**相邻 CPU area 永不共享一条 cache line**——某个 CPU area 里的锁字不会
与另一 CPU 的数据产生伪共享。改大模板时这条不变量仍成立，因为对齐基于编译期常量
而非模板布局。

该命名空间与旧 `.percpu` 完全分离；迁移已完成，目标镜像只保留
`.cpu_slot.template`，不再包含旧 `.percpu` 区域，也不再依赖 crates.io 的
`percpu` crate 及其 `non-zero-vma` feature。

不支持架构（非 x86_64/aarch64/riscv/loongarch64）在非测试构建下通过
`compile_error!` 拒绝，避免 base register 访问静默失效。

RISC-V 使用 `gp` 作为 per-CPU base register；`tp` 已被内核 TLS 使用，不能同时承担
per-CPU base。

## 后续扩展

动态 slot 通过 `CpuSlotChunk::alloc` 实现按 CPU 初始化和失败回滚；远端读取使用
`DynamicCpuSlot::get_remote`，只对 `T: Sync` 开放，不提供远端写入 API。

安全访问 API 第一阶段已完成：`kspin` 的真实 guard 实现 `CpuPin`，
`NoPreemptIrqSave` 实现 `CpuLocalGuard`，普通共享与按值 slot 访问已提供 guard 约束的
安全方法；通用可变引用只保留 `*_unchecked` unsafe 边界。

平台标识与调度器状态迁移已完成：`current_task_ptr`、BSP 标记、`KPCB_ID`
（当前 CPU 身份，是一切 guard `current_cpu()` 的来源）以及 ktask/kirq/kalloc 等
调用点都已落在寄存器寻址的静态 slot 或 `kplat` 托管的动态槽上，旧 `percpu`
依赖、`.percpu` 区域与 `non-zero-vma` feature 均已删除。

剩余扩展：

- IRQ/trap 上下文与调度器 pinned context 按 `CpuLocalGuard` 契约补齐实现。
- `CpuSlotCell` 的原子 `load/store/add/bit*` 快路径由具体架构模块补齐。
