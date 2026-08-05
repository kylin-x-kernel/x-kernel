# khal::irq — 设计文档

> **状态说明**：本文档描述 IRQ 解析/分发重构（提交 B）的架构，该重构已实现：
> 全局 `IRQ_STATE` + `try_lock` + raw 回退已替换为 per-domain 不可变快照 +
> `PendingIrq` claim 协议。aarch64/riscv64/loongarch64 全量构建、aarch64
> 全量 unittest（1405 用例）均通过。

## 定位

`khal::irq` 是 X-Kernel 的 OS 可见中断管理层，位于架构 HAL（`khal`）内，
向下承接各架构中断控制器驱动（GIC / IOAPIC / PLIC / LoongArch EXTIOI），
向上提供统一的 IRQ 描述、映射、注册和分发接口。

依赖本模块的主要子系统：

- `core/kruntime`：`init_interrupt()` 注册 timer / IPI / PMU 处理器；
- `drivers/irq`：GIC、x86 APIC、RISC-V PLIC 的 `IntrManagerIf` 实现；
- `platforms/kplat-loongarch64`：EXTIOI 分发；
- `drivers/console`：串口输入 IRQ（x86 COM1、AArch64 PL011/DT）；
- `drivers/kdriver`：`Irq::request` / `map_irq` 设备中断资源；
- `task/ktask`：poll 路径的 wakeup 订阅。

## 背景

### 回归根因（0bd7e105）

提交 0bd7e105 为修复 AArch64 PMU pseudo-NMI 自死锁，把公共函数
`translate_hwirq` 从阻塞的 `IRQ_STATE.lock()` 改为 `IRQ_STATE.try_lock()?`，
配合 `resolve_hwirq` 的 `unwrap_or(hwirq)` 回退。该改动引入了语义混叠：

```text
Option<Virq> 同时表达三种含义：
  1. 该 domain 没有此映射
  2. 锁竞争失败（try_lock 返回 None）
  3. 隐式 identity 回退（unwrap_or(hwirq)）
```

当另一个 CPU 短暂持有全局 `IRQ_STATE`（控制面操作，或任何一次并发的
`dispatch_subscribers`）时，数据面解析失败并把 raw hwirq 当作 virq 分发。

- x86：COM1（hwirq 4，IO_APIC_DOMAIN）已映射到动态 virq；解析失败后
  `dispatch_subscribers(4)` 查无 desc，打印 `Unhandled IRQ 4`。COM1 是
  edge-triggered 且 NS16550 无独立 ack，第一次边沿丢失后 RX 不再服务，
  shell 卡死。
- AArch64：同样的 `try_lock` 竞态存在于 GIC irqson 路径。PMU NMI 在开中断
  时走 IRQ 路径（`register_nmi` 用带 GIC domain 的 desc 映射到动态 virq，
  解析失败同样 misroute）；edge-triggered GIC 线（console DT 路径）与 x86
  一样致命；PL011 level 线靠重新触发自愈，掩盖了同一缺陷。

因此这是共享代码的架构级缺陷，不是 x86 平台特例，修复必须在 `khal::irq`
内架构无关地完成。

### 为什么不用 RCU

X-Kernel 没有 RCU 基础设施。本设计不引入 RCU、seqlock 或重试循环，而是利用
以下三个可论证的性质：

1. **反向映射只增不改**：`(domain, hwirq) → virq` 条目创建后从不改写、从不删除；
2. **快照不可变**：发布后的 `ReverseMap` 不再原地修改；
3. **旧快照永不回收**：发布新快照时泄漏旧快照（有意为之）。

由 1 可得读者容忍 stale：读到旧快照依然正确（只是少看到后来的条目），因此
不需要版本校验或重试；由 2、3 可得读者持有的引用在程序生命周期内永远有效，
因此不需要宽限期回收。

## 范围

涉及源文件：

```text
arch/khal/src/irq/
├── mod.rs        # 模块入口与 re-export
├── desc.rs       # IrqDesc / domain 常量（不变）
├── manager.rs    # 控制面 + 分发 + NMI 表（重构：去除数据面对全局锁的依赖）
└── domain.rs     # 新增：IrqDomain / ReverseMap / Published / PendingIrq
```

需同步修改的调用方：

```text
drivers/irq/src/x86.rs        # dispatch_irq 返回 PendingIrq
drivers/irq/src/gic.rs        # dispatch_irq 返回 PendingIrq（NMI window 逻辑不变）
drivers/irq/src/riscv.rs      # dispatch_irq 返回 PendingIrq
platforms/kplat-loongarch64/src/irq.rs  # dispatch_irq 返回 PendingIrq
```

## 架构

### 组件关系

```text
               控制面（任务 / 启动上下文）                数据面（hardirq / irqson NMI）
┌──────────────────────────────────────┐   ┌────────────────────────────────────────────┐
│ map / register / enable / wakeup     │   │ irq_handler:                               │
│        │                             │   │   platform_dispatch_irq -> PendingIrq      │
│        v                             │   │        │                                  │
│  IRQ_CTL（SpinNoIrq，唯一全局锁）     │   │        v                                  │
│   descs   mappings（构建表） next_virq│   │   dispatch_subscribers(&pending)          │
│        │                             │   │   一个 IRQ_CTL 短临界区内：                 │
│        v                             │   │   resolve + 查 descs（handler 锁外执行）    │
│  publish_snapshot(domain)            │   └────────────────────────────────────────────┘
│   重建 -> Published 原子发布          │
└──────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────────┐
│ irqsoff NMI（PMR <= NMI_ONLY 或 DAIF.I=1）：nmi_handler -> dispatch_nmi      │
│   -> NMI_TABLE（SpinRaw，无锁；boot 期写入）。本路径不在重构范围内。           │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 核心类型

```rust
// arch/khal/src/irq/domain.rs（新增）

/// 冻结的 reverse map，发布后只读。
enum ReverseMap {
    /// 稠密 domain（IOAPIC 24 线）：O(1) 数组查找。
    Linear(Box<[Option<Virq>]>),
    /// 稀疏 domain（GIC INTID、PLIC）：按 Hwirq 升序的有序切片，二分查找。
    /// 发布后完全只读，选择有序切片而非 BTreeMap：单块连续内存、缓存友好、
    /// 无节点指针追溯，构建期在控制锁下排序一次。
    Sparse(Box<[(Hwirq, Virq)]>),
}

/// 原子发布的不可变快照；旧快照永不回收。
/// 读者：一次 Acquire load。写者：控制锁下重建 -> Release store -> 泄漏旧值。
/// 本类型**没有 Drop 实现**，只允许挂在 static 上，禁止 drop、禁止回收。
struct Published<T> {
    ptr: AtomicPtr<T>,
}

struct IrqDomain {
    id: IrqDomainId,
    revmap: Published<ReverseMap>,
}

/// claim 协议：平台只上报原始 claim，核心负责解析。
pub enum IrqRef {
    /// 需要经 domain reverse map 解析。
    Domain(IrqDomainId, Hwirq),
    /// 显式身份 / 已解析逻辑号（LAPIC timer、MSI-X、RISC-V timer/IPI、
    /// LoongArch EXTIOI）。对应 Linux 的 IRQ_DOMAIN_FLAG_NO_MAP 语义。
    Virq(Virq),
}

pub struct PendingIrq {
    source: IrqRef,
    completion_cookie: usize,
    completed: bool,
}
```

### 接口变更摘要

| 现状 | 变更 |
|------|------|
| `resolve_hwirq(domain, hwirq) -> Virq`（`unwrap_or(hwirq)` 回退） | **删除** |
| `translate_hwirq(domain, hwirq) -> Option<Virq>`（`try_lock()`） | **删除**，由 `IrqDomain::resolve`（零锁）取代 |
| `IntrManagerIf::dispatch_irq -> Option<DispatchedIrq>` | 返回 `Option<PendingIrq>`（raw claim），解析移入核心 |
| `map/register/unregister/enable/descriptor/subscribe_wakeup` | 签名不变，内部走控制锁 + 快照发布 |
| 未映射线的语义 | `None` 只表示"无映射且无身份策略"；GIC 域按显式策略做身份分发，IO-APIC/PLIC 严格报未处理 |
| `NMI_TABLE`、`dispatch_nmi`、`nmi_handler` | 不动 |

### 可观察行为变更（相对旧代码，提交说明应显式列出）

| 场景 | 旧行为（0bd7e105 之后） | 新行为 |
|------|--------------------------|--------|
| 严格域（IO-APIC/PLIC）未映射线触发 | `unwrap_or(hwirq)` 回退，按 raw hwirq 查 handler 表（可能误命中其它 virq 或报 Unhandled） | `warn + EOI`，显式丢弃 |
| GIC 未映射线触发 | raw hwirq 身份分发 | 不变（显式 identity 策略） |
| 已映射设备的正常分发 | 无变化 | 无变化 |

严格域的丢弃是本次修复 0bd7e105 回归的核心：解析失败不可能再伪装成其它
virq。x86 上「未映射就触发」的中断（虚假中断、注册顺序问题）从"按 raw
hwirq 撞运气"变为明确丢弃，这是有意的行为收紧；对已映射设备无影响。

## 调用约束 / 执行上下文

### 数据面

- **`IrqDomain::resolve(hwirq)`**：可在 hardirq、irqson NMI 上下文调用。
  零锁、零分配、确定性有界（一次原子 load + 一次查找）；不依赖调度器、
  进程线程或 CPU 本地状态。
- **`PendingIrq::resolve()`**：同上；`IrqRef::Virq` 直通，`IrqRef::Domain`
  委托 `IrqDomain::resolve`。
- **`dispatch_subscribers(&pending)`**：hardirq 上下文；在同一个短
  `SpinNoIrq` 临界区内先 `pending.resolve()` 再查 handler 表，克隆 handler
  并处理 one-shot wakeup；handler 本体在锁外执行。
- **`PendingIrq::complete()`**：必须在 claim 该中断的 CPU 上调用；幂等。
- **irqsoff NMI 路径**：只访问 `NMI_TABLE`（SpinRaw），**不得**获取控制锁
  或 `Published` 之外的任何共享状态。

### 控制面

- **`map/register/unregister/descriptor/subscribe_wakeup/unsubscribe_wakeup`**：
  任务上下文或早期启动上下文；允许自旋等待；**不得**在 NMI 上下文调用。
- **`enable`**：通常也是控制面入口；唯一的数据面触达点是
  `dispatch_subscribers` 的 one-shot wakeup 关闭路径（hardirq 上下文调用
  `enable(desc, false)`）。该路径传入的 desc 映射必然已存在，`resolve_desc`
  只会复用既有映射，不插入新映射、不重建/发布快照、不分配；不得在该路径
  引入新的映射或发布逻辑。
- **`publish_snapshot(domain)`**：控制锁内；可分配内存（快照重建）。前提：
  kalloc 为自旋锁实现、不睡眠、无阻塞原语；锁序 `IRQ_CTL` → kalloc 内部锁
  （禁止反向）。IO-APIC 线性表有上界断言（`MAX_IO_APIC_LINEAR_ENTRIES`），
  畸形 hwirq 直接 abort 而非持锁巨量分配。

### 入口分类与 NMI 安全性（AArch64）

异常入口按以下条件分流（`arch/kcpu/src/aarch64/excp.rs`）：

```text
spsr.I != 0（被中断上下文 DAIF.I 置位）
  或 saved PMR <= NMI_ONLY
        -> irqsoff NMI 路径（nmi_handler，无锁）
否则   -> irqson IRQ 路径（irq_handler）
```

关键不变量：`IRQ_CTL` 是 `SpinNoIrq`。在 PMR 模式下持有它意味着 PMR 已降到
`NMI_ONLY`（`arch/karch/src/aarch64/irq.rs` 的 `save_irq_and_disable`），
非 PMR 模式则 DAIF.I 置位。因此：

- 任何 NMI 抢占"持有 `IRQ_CTL` 的临界区"时，入口分类必然命中 irqsoff 路径，
  不会进入 irqson；
- 能进入 irqson 的 NMI，其被中断上下文必然 PMR > NMI_ONLY 且 DAIF.I=0，
  同 CPU 不可能持有 `IRQ_CTL`，因此数据面即使取锁也不会同 CPU 自死锁。

重构后数据面解析零锁，irqson NMI 路径的取锁问题被直接消解；irqsoff NMI
路径保持无锁，两个方向都不依赖上述不变量兜底。

## 状态机

### Domain 快照

```text
 未发布(null) ──首次 publish──> 快照0 ──再次 publish──> 快照1 ──> …

读路径：任何时刻读到的是"某个完整快照"；
旧快照永久有效（内容只增不改），stale 读无害。
```

### desc 条目（现状保留，行为不变）

```text
未注册 ──register──> 有 handler ──unregister──> 无 handler
   └──── remove_if_unused（无 handler 且无 wakeup）──> 删除
```

### NMI 表（现状保留）

```text
boot 期 register_nmi ──> 已注册 ──（可选）unregister_nmi──> 移除
写者：boot 期或 NMI 处理器内；读者：NMI 路径。
```

## 算法流程

### irqson 分发（hardirq / 开中断 NMI）

```text
1. trap 进入 irq_handler(vector)
2. platform_dispatch_irq(vector) -> Option<PendingIrq>（平台侧 ack、EOI 决策、
   GIC NMI window 开关；不再自行解析）
3. dispatch_subscribers(&pending)（一个 IRQ_CTL 临界区）：
   a. pending.resolve()：IrqRef::Virq(v) -> Some(v)；IrqRef::Domain(d, h)
      走 IrqDomain::resolve(d, h)（Acquire load 快照 + 查找，零锁函数）
   b. Some(virq) -> 同一临界区内查 descs、克隆 handler / 处理 one-shot wakeup
      None       -> warn!("Unhandled IRQ {:?}", pending.source)
                    （仅限无身份策略的域）
4. pending.complete()：EOI / GIC deactivate，与 claim 配对
```

注意：GIC 域带有显式 identity-unmapped 策略（见 D-10），因此 GIC 上
`resolve()` 不会返回 `None`——未映射线按原始 hwirq 身份分发，与旧代码
`unwrap_or(hwirq)` 的可观察行为一致，但触发条件完全不同：解析路径零锁，
身份分发只可能源于"线确实未映射"，绝不可能是锁竞争失败。

resolve 与 handler 表查找在同一 `IRQ_CTL` 临界区内完成：并发 `unregister`
无法插在两者之间，分发要么看到 handler 存在，要么看到线已注销（报
Unhandled），不存在「resolve 成功但 desc 已被删」的中间态。残余场景：IRQ
在 unregister 完成之后才进入分发——此时线应已被平台禁用，报 Unhandled 是
正确行为（旧代码同样如此，非回归）。resolve 函数本身仍零锁，可在
NMI/测试路径独立调用；分发路径的取锁次数与重构前 descs 查找相同。

### irqsoff NMI 分发（不变）

```text
1. trap 进入 nmi_handler(vector)
2. platform_dispatch_nmi(vector) -> Option<DispatchedIrq>（raw hwirq）
3. dispatch_nmi_handler(hwirq)：NMI_TABLE 查找并调用，无锁
4. dispatched_irq.complete()
```

### 控制面 map() 与快照发布

```text
1. map(desc)：
   a. IRQ_CTL.lock()
   b. resolve_desc(desc)：查/建 mappings 条目，分配/复用 virq，写 descs
   c. 若 desc 带 domain：publish_snapshot(domain)（仅重建该 domain）
   d. 释放锁
2. publish_snapshot(domain)：
   a. 从 ctl.mappings 过滤该 domain 的 (hwirq, virq) 条目
   b. 稠密 domain（当前为 IO-APIC：GSI 小且连续）构建 Box<[Option<Virq>]>
      （按 max(hwirq)+1 定长，超过 4096 视为配置错误直接 abort，防畸形
      hwirq 在持锁下巨量分配）；稀疏 domain（GIC/PLIC：INTID/源号空间大且
      稀疏）排序后构建 Box<[(Hwirq, Virq)]>
   c. Published::publish()：Release store 新指针，泄漏旧指针
   d. 打印 debug 日志（domain id + snapshot 序号），便于发现异常频繁的运行期 map
```

## 并发模型

- **唯一全局锁**：`IRQ_CTL: SpinNoIrq<IrqState>`，保护 `descs`、`mappings`
  （构建表）和 `next_virq`。控制面操作与 `dispatch_subscribers` 共用此锁，
  与现状一致；解析函数本身不再依赖锁（分发路径在已持有的临界区内调用它）。
- **解析零锁**：一次 `Acquire` 原子 load + 一次查找。内存序要求：
  - 写者：先完成快照内容写入，再 `ptr.store(new, Release)`；
  - 读者：`ptr.load(Acquire)` 后读内容；
  - 快照构建在控制锁内串行化，不存在并发写者。
- **resolve 与 descs 查找原子一致**：`dispatch_subscribers` 在一个 `IRQ_CTL`
  临界区内先 `pending.resolve()` 再查 handler 表（见「算法流程」），并发
  `unregister` 无法插在两者之间，不存在「resolve 成功但 desc 已删」的中间态。
  残余场景：IRQ 在 unregister 完成之后才进入分发，此时线应已被平台禁用，
  报 Unhandled 是正确行为（与旧代码一致，非回归）。
- **无 RCU / seqlock / 重试**：见"背景"；stale 读容忍 + 永不回收使其成立。
- **运行期 map 支持**：virtio late-init 在 `enable_local_irq()` 之后通过
  `map_irq` 申请中断线，因此**不设 seal 点**；同一套快照重建机制覆盖
  boot 期与运行期 map。
- **irqson NMI**：解析零锁；`dispatch_subscribers` 仍取短锁，与今天一致，
  且由入口分类不变量保证同 CPU 不会重入。
- **irqsoff NMI**：不碰任何锁。

## 设计决策

| 编号 | 决策 | 理由 / 被否方案 |
|------|------|------------------|
| D-1 | per-domain 不可变快照承担数据面解析 | 被否：全局锁解析（回归根因）；per-domain `SpinRwLock` 读锁（NMI 重入 + 争用） |
| D-2 | 原子发布 + 永不回收 | 被否：RCU（无基础设施）、seqlock/重试（快照只增不改使版本校验无必要） |
| D-3 | `IrqRef::Virq` 显式表达身份 | 被否：`unwrap_or(hwirq)` 回退（None 三重语义混叠） |
| D-10 | GIC 域显式 identity-unmapped 策略 | aarch64 的 arch timer / IPI 以裸数字注册并经由 GIC 域分发，历史上依赖回退；策略化后身份是域的声明属性而非失败回退。IO-APIC/PLIC 为严格域（未映射 -> None）。边界：identity 返回 raw hwirq 作 virq，与动态 virq 空间（≥ 4096）无隔离；GIC 现实线号（SGI/PPI/SPI < 1024）不会触发，LPI 等大号线必须显式映射（与旧行为一致，非回归） |
| D-11 | 快照布局按域选择（IO-APIC 线性 / GIC、PLIC 稀疏） | 布局由 hwirq 空间形态决定而非平台：IO-APIC 的 GSI 小且连续（单控制器 0~23），定长数组 O(1)、缓存友好；GIC INTID（LPI 可达 0x3FFFF）与 PLIC 源号空间稀疏，按 max(hwirq)+1 做线性表会巨量膨胀，故用有序切片二分。当前在 `publish_snapshot` 按域 ID 分支选择，可演进为 `IrqDomain` 注册属性（与 `UnmappedPolicy` 同构） |
| D-4 | 无 seal 点，运行期 map 走同一条快照重建路径 | 被否：boot 冻结（virtio late-init 在 IRQ 使能后 map，会踩断） |
| D-5 | 稀疏域用有序扁平切片而非 BTreeMap | 只读快照下切片更小、缓存友好；`BTreeMap::get` 虽零分配但节点指针追溯更多 |
| D-6 | handler 表留在锁后，不塞进快照 | 见"未来工作"：handler/wakeup 是高频可变状态，塞进不可变快照会把泄漏从个位数变成运行期频率 |
| D-7 | 先保留唯一控制锁，不拆 per-domain 锁 | 控制面非热路径；有 `lock_stat` 数据再拆 |
| D-8 | NMI 路径零改动 | irqsoff NMI 已无锁；irqson NMI 由 D-1 受益 |
| D-9 | 提交拆分：A 止血 / B 重构 / C 性能 | AGENTS.md 要求 refactor 与 behavior 分离 |

### 被否方案补充：klazy::Once

`klazy::Once<T>` 提供初始化后单次原子 load 的安全读取，但**不支持重新发布**
（运行期追加映射需要更新快照），因此弃用，改用约 20 行的 `Published<T>`。
代价是引入一处有界 unsafe（清单与 SAFETY 论证随提交 B 一并写入
`arch/khal/docs/security.md`）。

### 未来工作：hwirq -> desc 一步到位

参考 Linux `irq_data` 内嵌于 `irq_desc` 的结构，可让快照直接携带稳定 desc
引用，省去 virq -> descs 的二次索引。**前置条件**：

1. desc 改为稳定的 per-virq 对象（不再 `remove_if_unused` 即删）；
2. handler/wakeup 高频可变状态仍留在 per-desc 锁后（Linux 亦锁 `irq_desc`）；
3. 需要为"快照引用的对象生命周期"提供回收或永久存活保证。

在出现可测量的性能需求前不做；快照条目先定义为 struct 而非裸 `Virq`，
为将来扩展保留协议兼容性。

## Drop / 资源释放

- **`Published<T>`**：无 `Drop` 实现，只允许挂 static；泄漏是设计意图而非
  副作用。禁止任何回收路径，否则 racing 读者会得到悬垂引用。`get()` 返回
  `&'static T`，把「pointee 永久存活」直接编码进类型，而非仅靠注释约束。
- **快照泄漏上界**：每次运行期 map 泄漏一份旧快照。boot/late-init 的 map
  调用次数为几十量级，泄漏总量为 KB 级；由 debug 计数器监控是否出现异常
  频率，出现后再引入 epoch / hazard pointer 回收（接口不变）。
- **`PendingIrq`**：与 `DispatchedIrq` 相同的幂等 `Drop` 补全（`completed`
  标志 + `complete_inner`），保证异常/早退路径不漏 EOI；completion 必须
  发生在 claim 的 CPU。
- **desc 条目**：`remove_if_unused` 行为不变。

## 验证与回归

### 单元测试（khal）

1. `map()` 同 `(domain, hwirq)` 复用 virq，不同 hwirq 分配不同 virq，
   virq >= `DYNAMIC_VIRQ_BASE`；
2. 发布后 `resolve`：命中 / 未命中返回 None / `IrqRef::Virq` 直通；
3. **回归不变量测试**：持有 `IRQ_CTL` 的同时调用 `resolve()` 必须成功——
   证明解析不依赖控制锁（若回到 blocking `translate_hwirq`，该测试死锁）；
4. 追加映射并重新发布后，旧线、新线都能解析；
5. 现有 `dispatch_subscribers` / wakeup 用例迁移到新状态结构。

### 平台回归（harness，按 test-harness skill 注册）

- x86：COM1 在并发中断负载下多次读命令（原始 stall 场景）；
- AArch64：PMU irqson 路径 + GIC edge 线（console DT）；
- 两端都要进矩阵：共享代码，缺陷非 x86 独有。

### 构建与检查（build-workflow）

```text
cp platforms/<plat>/qemu_defconfig .config && make defconfig
make build
make clippy
make UNITTEST=y run
cargo +nightly-2026-03-08 fmt --all
```

### 落地顺序

1. **提交 B（重构，root fix，本次改动）**：domain 快照 + `PendingIrq` 协议
   + 4 平台改动 + GIC 身份策略 + IO-APIC 稠密线性表（含上界断言）+
   删除 `resolve_hwirq`/`translate_hwirq` + 单测 + 本文档与 security.md。
2. **提交 C（可选，性能）**：`lock_stat` 对比与后续按需调优；稠密线性化已
   随提交 B 落地（见 D-11），不再属于 C。

> 原计划的提交 A（`translate_hwirq` 恢复阻塞锁）已按评审决定放弃：重构落地
> 后该函数被删除，止血改动不再有存在价值。
