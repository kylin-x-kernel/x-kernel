# khal::irq — 安全与可靠性分析

## 概述

本文档覆盖 `arch/khal/src/irq/` 模块（`desc.rs`、`domain.rs`、`manager.rs`）。
该模块是中断控制面与数据面的核心，包含一处有界 unsafe（`Published<T>`
原子快照发布）。安全论证的核心是两个不变量：**快照发布后不可变、永不回收**，
以及 **数据面解析零锁**（因此不存在锁竞争导致的错误分发）。
`dispatch_subscribers` 在单一 `IRQ_CTL` 临界区内原子完成解析与 handler 表
查找（resolve 函数本身零锁），并发 `unregister` 无法插在两者之间。

## 信任模型

```text
平台中断控制器驱动 / 内核子系统（控制面：map、register、enable、wakeup；
  enable 也会被 dispatch 的 one-shot wakeup 关闭路径调用，只复用既有映射）
   │  safe API
   ▼
┌──────────────────────────────────────────────────┐
│ khal::irq                                        │
│  IRQ_CTL（SpinNoIrq）：descs / mappings / virq 分配 │
│  IRQ 域注册表：Published<ReverseMap>（不可变快照） │
│  NMI_TABLE（SpinRaw，boot 期写入）                │
└──────────────────────────────────────────────────┘
   ▲  safe API（resolve / dispatch / complete）
   │
硬件中断（hardirq / irqson NMI / irqsoff NMI 入口）
```

- 控制面调用者信任本模块正确维护映射与 handler 状态。
- 数据面调用者（异常入口）信任解析零锁、确定性、有界。
- `enable` 在数据面（one-shot wakeup 关闭）仅复用既有映射：不插入新映射、
  不发布快照、不分配。
- 硬件（中断控制器状态）由平台驱动管理；本模块只在 `complete_irq` /
  `dispatch_irq` 接口层与其交互，不直接访问 MMIO/PIO。

## 外部边界 / 攻击面

- **固件/设备树/ACPI 中断描述**：经 `IrqDesc`（hwirq、trigger、domain）进入
  控制面。错误或恶意描述只会映射到本域命名空间，不会越界写。
- **设备中断信号**：数据面入口。未映射的严格域线会走 `warn + EOI`，
  不会伪装成其它 virq。
- **用户态**：不能直接调用本模块；只能经设备驱动间接注册/映射。
- **NMI 上下文**：irqsoff NMI 路径只读 `NMI_TABLE`；任何在该路径引入锁或
  分配的行为都违反模块契约。

本模块**不直接访问**用户内存、DMA 缓冲、MMIO/PIO 寄存器或汇编，
硬件寄存器访问全部封装在平台驱动层。

## unsafe 代码清单

### `Published<T>::get()`（`arch/khal/src/irq/domain.rs`）

- **操作**：`AtomicPtr::load(Acquire)` 后 `ptr.as_ref()`。
- **依赖不变量**：
  1. 非空指针只能来自 `publish()` 的 `Box::into_raw`，指向已完全初始化的
     `T`，对齐且类型正确；
  2. 快照发布后**永不修改、永不释放**（因此 `get()` 返回 `&'static T`）；
  3. `Acquire` 与 `publish()` 的 `AcqRel` 配对，保证内容可见。
- **谁保证**：`publish()` 只在控制锁内调用且只发布构建完成的快照；
  `Published<T>` 无 `Drop`、只允许挂 static；代码评审禁止添加任何回收路径。

### `Published<T>::publish()`（同文件）

- **操作**：`Box::into_raw` + `AtomicPtr::swap(AcqRel)` + 丢弃旧裸指针。
- **依赖不变量**：旧快照可能仍被 racing 读者引用，因此**故意泄漏**；
  丢弃裸指针不触发释放。泄漏上界 = 运行期 remap 次数 × 快照大小。

### `unsafe impl Send/Sync for Published<T>`（同文件）

- **不变量**：`get()` 返回的引用在程序生命周期内有效且只读，
  跨 CPU 共享仅在 `T: Send + Sync` 时成立；由 `PhantomData<*mut T>` 先否定
  自动 trait 再用有界 impl 恢复。

## 内存安全不变量

1. `ReverseMap` 发布后不可变（`Linear`/`Sparse` 均为只读查找）。
2. 快照永不回收；`Published<T>` 无 `Drop`。
3. `mappings`（构建表）只增不改不删；旧快照永远是对已存在条目的正确视图。
4. 非空快照指针一定指向有效、初始化、对齐的 `ReverseMap`。
5. `PendingIrq`/`DispatchedIrq` 完成状态幂等，防重复 EOI。
6. `dispatch_subscribers` 在同一 `IRQ_CTL` 临界区内完成 resolve 与 descs
   查找，不存在「解析成功但 desc 已删」的 TOCTOU 中间态。

## 线程安全

- `IRQ_CTL`（`SpinNoIrq`）串行化控制面与 `dispatch_subscribers`；临界区短，
  无跨 CPU 等待环。
- 锁序：`IRQ_CTL` → kalloc 内部锁（`balloc`/`palloc`/`usages`）。kalloc 全为
  自旋锁、无睡眠/阻塞原语，控制锁内快照重建不会阻塞；禁止任何路径在持有
  kalloc 锁时获取 `IRQ_CTL`。
- `dispatch_subscribers` 在一个 `IRQ_CTL` 临界区内原子完成 resolve + descs
  查找，并发 `unregister` 无法插在两者之间（残余场景：IRQ 在 unregister
  完成之后才进入分发，线应已被平台禁用，报 Unhandled 属正确行为）。
- 解析（`IrqDomain::resolve`）零锁：一次原子 load + 一次查找。
- irqson NMI：由异常入口分类不变量保证同 CPU 不重入控制锁（持锁即
  PMR ≤ NMI_ONLY 或 DAIF.I 置位，入口必然分流到 irqsoff）。
- irqsoff NMI：只读 `NMI_TABLE`（`SpinRaw`，boot 期写入）。
- 中断完成必须发生在 claim 的 CPU：`PendingIrq`/`DispatchedIrq` 通过
  `PhantomData<*mut ()>` 为 `!Send` 强制该约束。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 固件/DT 提供错误 hwirq，严格域（IO-APIC/PLIC）解析失败 | 低 | 描述错误或设备未枚举 | 显式 `warn + EOI`；不冒充 virq，不阻塞后续中断 |
| T-02 | 数据面因锁竞争错误分发（0bd7e105 回归类） | 高（x86 edge 丢中断） | 并发 CPU 持全局锁 | 解析零锁 + 快照原子发布，锁竞争不可表示；`dispatch_subscribers` 仍取一次控制锁查 handler 表（与重构前一致），解析本身零锁 |
| T-03 | 控制面 bug 改写已发布映射 | 高（UB） | 违反"只增不改"不变量 | 不变量入类型/注释；评审清单；`Published<T>` 无写接口 |
| T-04 | 运行期异常频繁 remap 导致无界泄漏 | 中 | 驱动热插拔风暴 | 每域 publish 计数器 + debug 日志；出现后再引入回收 |
| T-05 | 未来改动在 irqsoff NMI 路径取锁/分配 | 高（死锁/挂起） | 代码回归 | NMI 路径契约 + 审查清单；现有实现只读 NMI_TABLE |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 严格域未映射线触发 | 虚假/未枚举中断 | 一条 warn 日志 | 无 | 4 | warn + EOI |
| F-02 | GIC 身份线（timer/IPI）未映射 | 注册顺序/漏映射 | 身份分发到 raw hwirq | 若 handler 存在则正常服务 | 3 | 显式域策略；与旧行为一致 |
| F-03 | handler 早退漏 EOI | 代码早退路径 | In-Service 卡住 | 阻塞同级/低优先级中断 | 2 | `PendingIrq`/`DispatchedIrq` 幂等 Drop 补全 |
| F-04 | 快照重建分配失败 | 内存耗尽 | map() 失败/panic | 视调用方而定 | 2 | 内核 alloc（自旋锁、无睡眠、无阻塞）；锁序 IRQ_CTL → kalloc；IO-APIC 线性表上界断言，畸形 hwirq 直接 abort |

## 故障管理

- 解析 `None`：`warn!("Unhandled IRQ {:?}")` 后仍 `complete()`（EOI），
  维护硬件状态机。
- 重复注册/注销：返回失败并 warn，不破坏状态一致性。
- panic：内核 `panic=abort` 语义；`Drop` 补全仍尽力执行。
- 重试：无；快照发布失败即控制面错误，不进入数据面。

## 隐私分析

本模块不处理用户数据、不读取用户内存；中断号与 handler 引用均为内核内部
状态，无隐私面。

## 已知限制

- 运行期 remap 泄漏旧快照：当前调用全部在 boot / late-init，泄漏为 KB 级；
  若未来出现高频 remap 需引入 epoch/hazard pointer 回收。
- GIC 身份策略会掩盖"应当映射但漏映射"的注册错误（分发到 raw hwirq 后由
  handler 表兜底或报 Unhandled），与 0bd7e105 之前的行为一致，属有意取舍。
- GIC identity 返回 raw hwirq 作 virq，与动态 virq 空间（≥ 4096）无隔离：
  现实 GIC 线号（SGI/PPI/SPI）< 1024 不会触发；LPI 等大号线必须显式映射。
  与旧行为一致，非回归。
- 严格域（IO-APIC/PLIC）未映射线从旧 `unwrap_or(hwirq)` 回退改为显式
  `warn + EOI` 丢弃：可观察行为收紧，是修复 0bd7e105 回归的核心；对已映射
  设备无影响。
- x86_64 全量构建当前被依赖 `curve25519-dalek` 与 pinned nightly 的既有
  兼容性问题阻塞（与本次改动无关，已通过 stash 对照确认）；x86 控制器改动
  与其它三个平台同构。

## 审计清单

- [ ] `Published<T>` 无 `Drop`，只挂 static；无任何回收路径。
- [ ] `publish()` 仅在 `IRQ_CTL` 内调用；快照内容在 store 前构建完成。
- [ ] `mappings` 无删除/改写操作（新增域/映射不得违反）。
- [ ] `IrqDomain::resolve` 路径无锁、无分配、无日志（数据面热路径）。
- [ ] `irq_handler` 的每条路径（含 None）都执行 `pending.complete()`。
- [ ] irqsoff NMI 路径不获取任何锁、不分配。
- [ ] 新增 domain 必须显式声明 identity-unmapped 策略，禁止默认回退。
- [ ] `PendingIrq`/`DispatchedIrq` 保持 `!Send`（claim CPU 完成约束）。
- [ ] `resolve_and_publish` 校验 `publish_snapshot` 返回值；未知 domain 必须
      报警（warn），不得静默"成功"。
- [ ] IO-APIC 线性表上界断言存在；畸形 hwirq 不得在持锁状态下巨量分配。
- [ ] `dispatch_subscribers` 在单一 `IRQ_CTL` 临界区内 resolve + 查 descs。
