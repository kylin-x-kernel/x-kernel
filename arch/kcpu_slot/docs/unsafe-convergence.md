# kcpu_slot unsafe 收敛设计

本文记录 `kcpu_slot` 当前 unsafe 边界、外部 raw 访问的分类，以及后续收敛方向。
目标不是机械减少 `unsafe` 关键字，而是把地址有效性、CPU 固定、IRQ/NMI 重入、
引用排他性和远端同步等义务放进能够审计的类型与子系统边界。

## 当前结论

`kcpu_slot` 已完成 per-CPU 存储迁移、unsafe 边界收敛，并对齐 Asterinas 的访问
模型（见 [`asterinas-style-access.md`](asterinas-style-access.md)）。旧的
`read_raw`/`write_raw`/`current_ref_mut_raw` 以及 base 型 raw 入口
（`get_at`/`read_at`/`write_at`/`ref_at`/`ref_mut_at`）均已删除。

当前访问分为四层：

- **自举原语**：`current_base_raw()`、模板符号，仅 `kcpu_slot` 内部使用；
- **无 guard 的 Sync/Copy 访问**：`current_shared`、`shared_current`、
  `load_current`/`store_current`（架构单指令；无单指令的架构经 `karch` 提供的
  `CpuLocalIrqIf` 关中断兜底）；
- **guard 约束访问**：`get`/`read`/`write`，以及显式 `unsafe` 的 `get_mut_unchecked`；
- **已发布 area 访问**：`at`/`shared_at`。

普通调用点不再出现裸 base；唯一保留的 public raw 边界是
`CpuSlotCell::current_shared_mut`，供 x86 GDT bring-up 使用
（`GlobalDescriptorTable::load` 要求 `'static` 引用）。逐指针内部 helper 为
`pub(crate)`，不对外暴露。

## unsafe 类型清单

### 架构 base register

`src/arch/` 负责读取和安装各架构的 per-CPU base register：x86-64 使用 GS base，
AArch64、RISC-V 和 LoongArch64 使用各自约定的寄存器。这类 unsafe 来自内联汇编和
特权寄存器状态。

必须维持：

- 寄存器只指向当前 CPU 已初始化的 area；
- 写寄存器只发生在 CPU bring-up 或受控切换阶段；
- 安装完成前不得执行普通 slot 访问。

这类 unsafe 应保留并封装在架构模块内。

### 链接器模板与 CPU area 初始化

`src/layout.rs` 和 `src/macros.rs` 使用 linker symbol、
`.cpu_slot.template`、模板 offset 和 `copy_nonoverlapping` 构造每 CPU 副本。

必须维持：

- Rust 产物与 linker script 属于同一镜像；
- template LMA、size、area start 和 stride 正确；
- CPU 编号小于链接时的 `NR_CPUS`；
- 目标 area 可写、对齐、足够大、互不重叠且由对应 CPU 独占；
- 模板中的值允许按位复制。

这类 unsafe 应保留在 `kcpu_slot` 内部；外部不应直接操作 linker base 的裸 `usize`。

### `base + offset` 裸指针寻址

`CpuSlot` 和 `CpuSlotCell` 的 `*_unchecked`、`*_at` 系列把 area base 与链接器
offset 相加并转换为引用或执行按值读写。

必须同时证明：

- base 有效并属于目标 CPU；
- offset 在 area 内且满足 `T` 的对齐；
- 对象已经初始化；
- backing area 的生命周期覆盖返回引用；
- 共享和可变引用不冲突；
- 普通访问不与 IRQ、NMI 或其他 CPU 形成数据竞争；
- 当前上下文在引用存活期间不迁移。

`ref_at` 和 `ref_mut_at` 的风险高于 `read_at` 和 `write_at`，因为前者生成可能逃逸
出调用表达式的 Rust 引用。

### CPU-local 并发能力

`CpuPin` 表达上下文在 guard 生命周期内不会迁移；`CpuLocalGuard` 进一步表达本地
普通 IRQ 不会重入同一 slot。`kspin` 的 guard 通过 unsafe impl 承诺这些性质。

该能力只能证明执行上下文属性，不能自动证明同一个 slot 的引用唯一性，也不能证明
NMI 不会重入。调度器、allocator、IPI 和 NMI 状态还需要各自的所有权或同步契约。

### 静态模板按位复制

`StaticSlotValue` 表达模板值可被复制到所有 CPU area。危险点在于真实条件是
“这个具体初始值可以复制”，而当前 trait 是类型级许可。例如 `Vec::new()` 是安全的
空模板，已经持有 allocation 的 `Vec` 不能安全复制。

审计任何新增实现时必须检查：

- 模板初始值不拥有会被多个 CPU 共同复制的资源；
- 模板不含指向自身或模板地址的引用；
- slot storage 不会隐式析构模板副本；
- 每 CPU 后续初始化、覆盖和清理遵守该类型的所有权规则。

`cpu_slot_cell_with_template!` 已提供显式值级认证；`Vec`、`VecDeque`、`Weak` 和
`LazyInit` 的类型级 blanket impl 已删除。数组、`Option<T>`、外部显式 unsafe impl 和
`impl_static_slot_value!` 的调用点仍需持续审计。

### 动态 slot 原始内存

`CpuSlotChunk` 和 `DynamicCpuSlot` 使用 caller-owned backing、bump allocation、
`MaybeUninit` 原位构造、失败回滚和逐 CPU 析构。

必须维持：

- backing 生命周期覆盖 chunk 和全部 handle；
- 分配区间不重叠，地址与 stride 满足 `T` 的对齐；
- 每个成功初始化的值恰好析构一次；
- 失败路径只析构已经完成初始化的元素；
- remote shared access 要求 `T: Sync`；
- local mutable access 具有真正的排他性。

### `Send` 和 `Sync` 人工证明

`CpuSlotCell`、`DynamicCpuSlot`、模板 wrapper 及部分 payload 具有 unsafe
`Send`/`Sync` 实现。这些实现依赖 guard、每 CPU 分区、内部锁或原子，而不是
`UnsafeCell` 本身提供同步。

修改任何安全访问器、guard 实现、远端 API 或 drop 生命周期时，必须重新审计相关
`Send`/`Sync` 证明。

## 现有安全 API 的限制

原设计中的 `CpuSlot::get_mut`、`CpuSlotCell::get_mut` 和
`DynamicCpuSlot::get_mut` 以 `&mut CpuLocalGuard` 约束同一 guard 上的重叠借用，
但 `NoPreemptIrqSave` 可以嵌套。安全代码可以创建两个 guard，并从同一 slot 获得两个
重叠的 `&mut T`；也可以先从一个 guard 获得 `&T`，再从另一个 guard 获得重叠的
`&mut T`。因此问题会传递到共享引用面，而不只影响可变访问器。

因此，“不同 guard 之间不重叠”不能作为安全调用者义务写在文档中。第一阶段先删除
安全的通用可变引用访问面，只保留显式 unsafe 的 `*_mut_unchecked`。完成下列方案之一
前，不得重新引入安全的通用引用型可变访问：

- 建立不可嵌套的唯一 CPU-local token；
- 引入可靠的 per-slot 运行时 borrow tracking；
- 将通用引用型访问保持为 unsafe；
- 对复杂状态只提供领域操作或领域 owner token，不提供通用 `&mut T`。

共享 `get` 只有在安全代码无法获得同一 slot 的可变引用后才独立成立。按值
`read`/`write` 不让引用逃逸，原子访问依赖原子类型自身的同步语义，这两类可以继续
作为安全面。

closure API 可以限制引用逃逸，但 callback 内仍可创建第二个嵌套 guard，所以 closure
本身不能解决别名问题，也不作为过渡 API 引入。

## 目标访问模型

### Copy 标量

对 bool、整数、CPU ID、deadline 和 epoch 等值只提供短暂的 `read`、`write`、
`replace` 操作，不返回引用。当前 safe `CpuSlotCell::read/write` 要求
`CpuLocalGuard`；只有 unchecked 面接受较弱的 `CpuPin`。未来若提供只要求 `CpuPin` 的
Copy view，必须先证明该 slot 不与 IRQ/NMI 形成非原子数据竞争。`khal::current_task_ptr`
是 guard/CPU 身份解析链上的
递归源头，不能先取得 pin，应作为命名明确的底层 raw 边界保留。

### 原子状态

为 `Atomic*` payload 提供 `current(&CpuPin)` / `at(&CpuArea)` 形式的原子引用，让
调用方保留 `Ordering` 选择权。远端访问不应要求各子系统自行组合
`cpu_area_base(cpu)` 和 `get_at`。

### 复杂 CPU-owned 状态

run queue、allocator cache、IPI queue 和 watchdog registry 等状态应绑定领域所有权：

- scheduler 状态使用 `SchedulerCpuToken`；
- allocator PCP/slab 状态使用 allocator IRQ-off token；
- IPI queue 使用内部锁或 IPI 上下文 token；
- watchdog 注册表使用“初始化期可变、发布后 NMI 只读”的生命周期。

领域 API 优先提供受限操作，不应返回无约束的 `'static mut` 引用。owner token 类型
应进入 slot 的类型或定义，使“哪个子系统有权访问”在声明处可见。

### CPU 初始化

`begin_cpu_init`、`BootCpuToken` 和 `publish` 已集中完成模板复制、base register 安装、
CPU ID/BSP 写入和 area 发布。CPU 身份读取是 guard 自举根，仍保留为命名 raw 边界。

### IRQ 与 NMI

普通任务 IRQ-off 入口继续统一使用 `CpuLocalGuard` 词汇。任务路径使用
`NoPreemptIrqSave`。trap/IRQ 入口不能直接实现通用 `CpuLocalGuard`：普通 IRQ 虽已屏蔽，
硬件或伪 NMI 仍可能嵌套。`kirq` 应持有领域私有的 hard-IRQ owner，只访问声明为不向
NMI 暴露的状态。禁止仅凭“IRQs disabled”公开构造允许任意 `&mut T` 的对象。

NMI 不可屏蔽，因此 NMI-safe 不是通用 token 能单独证明的性质，而是 token 能力与
slot 暴露面的乘积。每个 slot 应在定义处记录暴露面 `{任务, IRQ, NMI}`。NMI 路径只
开放原子、快照或发布后只读操作。watchdog registry 已使用 Release 发布、NMI Acquire
只读协议；`LOCKUP_DETECTION` 的 IRQ/NMI 共享字段已改为原子。

### 远端 CPU

用经过范围和初始化状态校验的 `CpuArea` 或等价对象替代公开的裸 `usize` base。
`BootCpuToken::publish` 发布 initialized 位，`area(cpu)` 只为已经完成 bring-up 的 CPU 返回
对象。远端 API 按同步模型区分共享、原子和内部锁访问，不提供通用的远端 `&mut T`。

## 调用点迁移原则

按以下顺序判断一个 raw 调用是否可收敛：

1. 如果只是当前 CPU 的 Copy 标量读写，改用 token-backed `read/write`。
2. 如果 payload 是原子，改用 current/remote 原子 API。
3. 如果调用点已经持有调度器、allocator 或 IRQ 所有权，把该所有权显式建模为 token。
4. 如果访问发生在初始化阶段，归入统一的 CPU 初始化入口。
5. 如果访问远端 CPU，使用 checked area 和 payload 自身的同步机制。
6. 如果发生在 NMI，限制为原子或发布后只读数据，不能返回任意可变引用。
7. 只有无法由上述模型表达的架构和链接器路径才保留裸 base unsafe。

禁止为了减少 `unsafe` 数量，把同样的调用者义务移动进一个无检查、无 token 的安全
helper。

## 分阶段实施

| 阶段 | 状态 | 结果 |
| --- | --- | --- |
| 1. soundness 修复 | 完成 | 删除 safe `get_mut` 系列，明确 guard 不证明对象唯一借用 |
| 2. 生命周期与 typed access | 基础完成 | Boot token、checked area、共享远端视图已落地；Copy/atomic 继续按暴露面迁移 |
| 3. 领域所有权 | 主体完成 | scheduler、allocator、IPI、watchdog 已使用内部锁、原子或发布协议；PMU 待单独重构 |
| 4. API 收口 | 部分完成 | 已完成首轮清点和值级模板认证；public raw API 尚有跨 crate allowlist 用户 |

## 当前实施状态

- 阶段 1 已完成：安全的通用 `get_mut` 系列已删除；共享和按值安全面保留。
- 阶段 2 已完成基础部分：`begin_cpu_init` / `BootCpuToken::publish` 建立
  `UNINITIALIZED → INITIALIZING → PUBLISHED` 状态机；`CpuArea` 只表示已发布 area；
  `CpuSlot::at` 与 `CpuSlotCell::shared_at` 提供 `T: Sync` 的 checked 共享视图。
- 阶段 3 已完成 allocator、IPI 和 watchdog/NMI：allocator cache raw 引用集中到私有
  `AllocLocalOwner`；IPI 远端队列经 checked area 和内部 `SpinNoIrq` 访问；watchdog
  registry 采用注册、Release 发布、NMI Acquire 只读协议；lockup detection 只使用共享
  原子状态。
- scheduler 的 whole-object 别名问题已完成修复：`CurrentRunQueueRef`、`KRunQueueRef`
  和 registry 只持共享 `&RunQueue`；所有变化经 scheduler `SpinRaw` 或原子字段完成。
  `last_accounted_ns` 使用仅在同一 scheduler 锁内访问的 Relaxed `AtomicU64` 提供内部
  可变性，不承担同步语义。`PREV_TASK` 和退出队列已各自使用 `SpinNoIrq`；boot
  `LazyInit` 保留为命名初始化边界。
- IRQ context 与 periodic callback 容器已使用 per-CPU `SpinNoIrq`，嵌套 guard 不再能
  制造重叠 `&mut`，且 callback 在锁外执行。APIC 热路径复用 `IrqSave` pin，ARM timer
  在同一 guard 内读写，VMX 初始化全程固定 CPU。
- 阶段 4 已完成首轮审计，但 raw API 尚不能全部降为 `pub(crate)`：架构 IRQ、PMU、timer、
  APIC、VMX、scheduler 和 CPU 身份递归链仍有跨 crate 用户。保留的 public raw API
  是显式迁移清单，不代表普通调用点可以继续新增使用。

`StaticSlotValue` 已改为语言级 `unsafe trait`。所有权容器的 blanket impl 已删除，固定
空模板通过 `cpu_slot_cell_with_template!` 做值级认证。`impl_static_slot_value!` 仍可由
安全语法生成 unsafe impl，因此它是治理待办：调用点必须有相邻 `SAFETY` 证明，后续
考虑删除该宏或要求显式 unsafe 语法。

对齐 Asterinas 后已不再需要 raw 的 self-hosting 调用点：`CURRENT_TASK_PTR`、
KCPU_ID/BSP 使用 `load_current`/`store_current`；架构 IRQ readiness/active-exception
原子状态使用 `current_shared`；watchdog、scheduler、PMU、allocator 使用
`shared_current`/`get`/`unsafe get_mut_unchecked`；x86 TSS 使用 `current_shared()`（汇编仍走模板偏移）。
唯一保留的 raw 边界是 x86 GDT bring-up 的 `current_shared_mut`。

## 预期最终边界

```text
架构寄存器 / linker unsafe
            |
            v
kcpu_slot 内部地址计算与初始化
            |
            v
checked CpuArea + execution/owner token
            |
            v
Copy / atomic / domain closure API
            |
            v
普通调用点不含 per-CPU 地址 unsafe
```

最终仍会保留少量 unsafe：架构寄存器、链接器符号、模板复制、动态 backing 构造、
`MaybeUninit` 生命周期和极底层 trap/NMI glue。收敛成功的标准是这些义务集中、可追踪、
可由少量 owner 审计，而不是仓库中完全没有 `unsafe`。
