# `kcpu_slot` — 统一 CPU-local Slot 设计文档

## 定位

`kcpu_slot` 是 X-Kernel 的公共 CPU-local 基础设施 crate，负责定义、初始化和访问每个逻辑 CPU 独立的数据 slot。

它位于平台启动、架构 HAL、调度器、IRQ 和需要 CPU-local cache 的内核模块之间：

```text
                +-----------------------------+
                | scheduler / irq / allocator |
                +--------------+--------------+
                               |
                 typed CPU-local handles / guards
                               |
                +--------------v--------------+
                |           kcpu_slot          |
                | static / dynamic / cell      |
                | init / layout / access       |
                +------+---------------+-------+
                       |               |
                arch base register   linker section
                 GS/gp/TPIDR/$r21    .cpu_slot.template
```

目标不是替代架构寄存器或调度器，而是集中表达以下不变量：

1. 一个对象在每个 CPU 上只有一个实例；
2. 当前 CPU 访问期间，任务不会迁移；
3. 需要 IRQ 重入保护的对象不能只依赖“禁止抢占”；
4. 静态对象、动态对象和仅当前 CPU 的 cell 具有不同的生命周期与访问语义；
5. 架构 fast path 不应被高层类型安全抽象强制退化为通用指针计算。

## 背景

X-Kernel 当前直接依赖 `percpu 0.4`。它的 `.percpu` 模板、64 字节对齐、架构寄存器和基本整数 raw access 都很简洁，适合热路径，但访问安全主要依赖调用者正确使用 `read_current_raw()`、`current_ref_mut_raw()` 和 IRQ/preemption guard。

Asterinas 的 CPU-local 实现提供了值得吸收的架构边界：

- `CpuSlot<T, S>` 与仅当前 CPU 的 `CpuSlotCell<T>` 分离；
- 静态对象与动态对象分离；
- `PinCurrentCpu` 显式表示 CPU 不迁移；
- 动态对象可以对每个 CPU 独立执行初始化和析构；
- `!Copy`、`!Clone`、`!Send` 防止 storage handle 被错误复制或跨任务发送；
- x86 等架构保留单指令 `GS:[offset]` fast path。

`kcpu_slot` 采用两者的组合：保留 X-Kernel 的低层 section/寄存器模型，吸收 Asterinas 的类型和生命周期模型。

## 当前 X-Kernel 的 percpu 使用现状

当前工作区直接依赖 crates.io `percpu 0.4`，在多个架构和子系统中使用
`#[percpu::def_percpu]`。当前使用可以分为以下几类：

| 使用类别 | 代表位置 | 典型数据 | 当前访问方式 |
|---|---|---|---|
| 平台身份与 CPU 基础状态 | `platforms/kplat/src/cpu.rs`、`arch/khal/src/percpu.rs` | CPU ID、BSP 标记、当前任务指针 | `read_current()`、`write_current_raw()` |
| 调度器核心状态 | `task/ktask/src/run_queue.rs`、`task/ktask/src/timers.rs`、`task/ktask/src/future/time.rs` | run queue、idle task、退出队列、timer runtime | `current_ref_*_raw()`、手动 `NoPreempt`/IRQ guard |
| IRQ、softirq 和 bottom-half | `arch/kirq/src/bottom_half/*.rs`、`io/watchdog/src/*.rs` | softirq context、worker/lockup 状态、IRQ 统计 | raw 指针或 `with_current()` |
| IPI/TLB 与架构硬件状态 | `arch/kipi/src/*.rs`、`mm/kalloc/src/pcp.rs`、`virt/kvmm/src/arch/x86_64/vmx.rs` | TLB shootdown、per-CPU page cache、VMXON 状态 | 当前 CPU raw 访问，部分路径依赖 IRQ 关闭 |
| 驱动和计时器缓存 | `drivers/timer/src/arm_generic.rs`、`drivers/x86-apic/src/lib.rs` | tick、APIC/定时器本地状态 | `read_current()`、`write_current_raw()` |

这些调用点共同依赖同一个底层模型：链接器把变量放入 `.percpu`，启动时复制 CPU area，架构寄存器保存当前 CPU base，访问器通过“base + symbol offset”定位实例。

## 当前使用方式存在的问题

### 1. 当前 CPU 稳定性依赖调用者约定

`read_current_raw()`、`current_ref_mut_raw()` 的安全条件是禁止抢占或保证任务不会迁移，但类型系统没有区分：

- 只需要禁止抢占的访问；
- 必须同时关闭本地 IRQ 的访问；
- 可以安全远端读取的 `T: Sync` 对象；
- 只能当前 CPU 独占访问的对象。

结果是安全条件散落在调用点的注释和 guard 生命周期中。新增调用点容易复制一个 raw 访问，却遗漏 `NoPreempt` 或 IRQ 保护。

### 2. “禁止抢占”和“禁止 IRQ 重入”容易混淆

当前任务指针在 `arch/khal/src/percpu.rs` 中针对非 x86 架构手动使用 `kspin::IrqSave`，原因是多条指令读取 CPU base/offset 时必须防止异步切换。但其他 `def_percpu` 变量的调用者不一定有统一规则：

- 禁止抢占只能防止任务迁移；
- IRQ 仍可能在同一 CPU 重入并访问同一变量；
- 非 `Sync` 的 `current_ref_mut_raw()` 若被 IRQ 访问，会形成 Rust 别名规则之外的并发修改。

当前 API 没有把这两个条件区分出来。

### 3. 静态模板按字节复制，复杂类型约束不够显式

`percpu` 初始化会把 `.percpu` 模板复制到其他 CPU area。对于 `Option<T>`、指针、引用计数或带析构语义的值，调用者必须自己判断初始值是否可以安全按位复制。

当前没有统一的“静态对象必须是可复制初始状态”标记，也没有 debug 机制检查 BSP 模板是否在复制前已经被使用。

### 4. 缺少动态 per-CPU 对象生命周期

现有 `def_percpu` 适合内核镜像中永久存在的静态变量，但不适合：

- 可选模块启用后创建、卸载时销毁的状态；
- 按 CPU 独立构造的统计对象；
- 需要对每 CPU 执行析构的 cache；
- 不应被模板 bitwise copy 的对象。

调用者只能自行维护 `[T; NR_CPUS]`、指针数组或全局锁，容易产生重复释放、CPU 数量不一致和部分初始化失败后的清理问题。

### 5. 远端访问的同步协议没有统一边界

`remote_ptr()`/`remote_ref_*()` 可以根据 CPU ID 访问其他 CPU 的 slot，但“远端读是否要求 `Sync`”“远端写需要什么锁或 IPI”“CPU 是否可能 offline”主要由调用者自行保证。

这在 TLB、workqueue、统计和虚拟化状态等场景中容易形成隐式数据竞争。

### 6. 初始化职责分散且与上游 crate 强耦合

当前 `percpu::init_in_place()`、`init_percpu_reg()`、平台 `boot_cpu_init()`、AP 启动入口和 linker script 分散在不同 crate。初始化顺序必须满足：

```text
percpu area 初始化 → 当前 CPU base 寄存器 → CPU ID/BSP 状态 → trap/IRQ → 调度器
```

但这个顺序没有由一个 X-Kernel-owned crate 统一建模。另一个实际问题是 `percpu` 的 `preempt` feature 使用 `kernel_guard`，而 X-Kernel 调度器主要通过 `kspin` 提供抢占接口，guard 后端存在重复抽象和集成风险。

### 7. 宏名称表达了实现，不表达对象语义

`def_percpu` 很适合作为底层代码生成器，但它无法从名称上说明变量是：

- 仅当前 CPU cell；
- 可远端读取的 typed slot；
- 需要独立构造的动态对象；
- 只允许在 IRQ-disabled 区间借用的对象。

## `kcpu_slot` 的问题目标与解决方案

| 现有问题 | `kcpu_slot` 解决方案 |
|---|---|
| raw API 依赖调用者记忆安全条件 | 用 `CpuSlot<T>`、`CpuSlotCell<T>` 和 `PinCurrentCpu` 区分访问语义 |
| 迁移与 IRQ 重入语义混在一起 | `PinCurrentCpu` 只表达 CPU 固定，IRQ 重入继续由上层现有 IRQ guard 负责 |
| 静态 bitwise copy 约束隐含 | 静态 slot 明确要求复制安全的初始状态；debug 下追踪模板是否已被提前使用 |
| 没有动态对象生命周期 | 提供 `DynamicCpuSlot<T>` 与 `CpuSlotChunk`，按 CPU 独立初始化、失败回滚和析构 |
| 远端访问容易数据竞争 | 默认只开放 `T: Sync` 的远端共享读取；远端写入改由原子、IPI 或调用方锁协议完成 |
| 初始化流程散落在平台和上游依赖 | `kcpu_slot` 只提供当前 CPU area 初始化；BSP/AP 生命周期和 online 状态由平台唯一管理 |
| fast path 与类型安全难以兼顾 | `cpu_slot!`/`cpu_slot_cell!` 生成隐藏 `.cpu_slot.template` 模板、正常地址 descriptor 和 typed offset；不依赖 `def_percpu` |
| 抢占 guard 后端重复 | `PinCurrentCpu` 只定义 CPU 固定契约，具体由 `kspin`/调度器提供，不再让业务 crate 直接依赖另一套 percpu guard 实现 |

## crate 放置位置

建议将 crate 放在 `arch/kcpu_slot`，而不是 `util/kcpu_slot`。

原因是它并非纯粹的通用容器，而是直接拥有以下架构边界：

- GS、`gp`、TPIDR、`$r21` 等 CPU-local base 寄存器；
- `.cpu_slot.template` 链接 section、模板 LMA 和 `_cpu_slot_*` 布局符号；
- BSP/AP 启动时安装 CPU-local base 的协议；
- 架构专用单指令 load/store/add/bit 操作；
- 与 trap、IRQ、抢占和 CPU online 顺序相关的执行上下文契约。

推荐依赖方向：

```text
arch/kcpu_slot
    ├── arch-specific base-register code
    ├── linker/boot contract
    └── PinCurrentCpu trait
          ▲
          ├── arch/khal
          ├── task/ktask + task/kspin
          ├── arch/kirq
          └── drivers / mm / virt
```

`arch/kcpu_slot` 仍应保持平台无关的高层 storage API；具体寄存器代码放在 `src/arch/` 子模块，平台 crate 负责调用初始化函数和提供 AP memory。这样可以避免 `kcpu_slot` 依赖 `khal` 或调度器，保持底层依赖方向单向。

## 范围

建议新 crate 放置于 `arch/kcpu_slot`，初始文件布局如下：

```text
arch/kcpu_slot/
├── Cargo.toml
├── src/
│   ├── lib.rs              # 公共 API 和 feature 选择
│   ├── layout.rs           # section/layout/area 计算
│   ├── static_local.rs     # 静态 CPU-local storage
│   ├── dynamic_local.rs    # 动态 chunk 和对象分配
│   ├── cell.rs             # 当前 CPU cell
│   ├── guard.rs            # PinCurrentCpu / access guard
│   ├── arch/
│   │   ├── mod.rs
│   │   ├── x86_64.rs
│   │   ├── aarch64.rs
│   │   ├── riscv.rs
│   │   └── loongarch64.rs
│   └── macros.rs           # cpu_slot! / cpu_slot_cell!
└── docs/
    ├── design.md
    └── security.md
```

本 crate 不负责：

- CPU 启动流程、AP trampoline 和具体寄存器初始化时机；
- 抢占计数实现；
- IRQ 控制器操作；
- 通用内存分配器策略。

这些能力通过 `KcpuSlotPlatform` 和 `PinCurrentCpu` 接口注入。

## 架构

### 三类访问对象

#### `CpuSlot<T>`

由 `cpu_slot!` 宏创建。宏把按位可复制的模板放入 `.cpu_slot.template`，并在普通
数据区创建 descriptor。它适用于：

- CPU ID、BSP 标记；
- 简单计数器和位标志；
- 可安全按位复制的整数句柄。

```rust
cpu_slot! {
    static CPU_ID: usize = 0;
}
```

公开的 `CpuSlot<T>` 是正常地址空间中的 descriptor，不是 VMA 0 模板本体；模板只
通过 linker symbol offset 访问。

#### `CpuSlotCell<T>`

由 `cpu_slot_cell!` 创建，只允许访问当前 CPU 的实例；其类型必须实现 sealed 的
`StaticSlotValue`：

```rust
cpu_slot_cell! {
    static NEED_RESCHED: bool = false;
}
```

它适用于单字长标志、计数器和指针。当前基础实现提供带 pin 的 `UnsafeCell`
访问；原子 `load/store/add/sub/bit*` 快路径应在具体架构原子语义确定后继续补充，
不能把普通读改写误当成原子操作。

`StaticSlotValue` 只覆盖明确允许按位复制的基础值和数组；带所有权或析构语义的
对象必须使用动态 slot。

#### `DynamicCpuSlot<T>`

由 `CpuSlotChunk` 分配。每个对象在所有 CPU chunk 中使用同一个 offset，但初始化闭包可获得目标 `CpuId`：

```rust
let counters = chunk.alloc(|cpu| CpuCounter::new(cpu))?;
```

它适用于运行时创建的统计对象、可选模块状态和需要显式析构的 cache。动态对象 handle 不可复制，必须通过所属 chunk 释放。

### Storage 抽象

核心内部 trait：

```rust
unsafe trait CpuSlotStorage<T: 'static> {
    fn ptr_current(&self, pin: &impl PinCurrentCpu) -> *mut T;
    fn ptr_cpu(&self, cpu: CpuId) -> *mut T;
    unsafe fn drop_cpu(&mut self, cpu: CpuId);
}
```

该 trait 不直接公开给普通调用者。实现必须保证：

- `ptr_current` 指向当前 CPU 的实例；
- `ptr_cpu` 的 CPU ID 已验证；
- 返回指针满足对齐、有效性和对象生命周期；
- 对象释放后任何旧 handle 都不能再次访问。

### CPU 固定接口

```rust
pub unsafe trait PinCurrentCpu {
    fn current_cpu(&self) -> CpuId;
}
```

平台和任务 crate 为以下 guard 实现该 trait：

- `kspin::NoPreempt` / `NoPreemptIrqSave`；
- `kspin::IrqSave`；
- trap/IRQ 框架提供的不可迁移 guard。

`PinCurrentCpu` 只表达“不会迁移”，不自动表达“不会被 IRQ 重入”。需要引用非
`Sync` 对象或可被 IRQ 访问的对象时，调用方必须使用上层已有的 IRQ guard。

建议提供两个入口：

```rust
pub unsafe fn get_with<'a>(&'a self, pin: &'a PinCurrentCpu)
    -> &'a T;
```

其中 `get_with_irq` 的 guard 同时实现 `PinCurrentCpu`，从而把 IRQ 重入约束编码进类型。

## 调用约束 / 执行上下文

### 初始化

- BSP 必须在首次使用 CPU-local 数据前完成 `init_bsp()`。
- AP 必须在进入 Rust 代码、启用本地 IRQ 前完成 `init_ap(cpu_id, base)`。
- 平台在 BSP/AP 启动上下文为当前 CPU 准备 area，并调用 `initialize_cpu()`；
  `kcpu_slot` 不复制平台的 BSP/AP 状态机。
- 静态模板复制前禁止访问会被复制的 BSP CPU-local 对象。
- 动态分配要求页分配器和 CPU 数量已经初始化；不允许在最早期 boot allocator 不可用阶段调用。

### 当前 CPU 访问

- `get_with` 不阻塞、不睡眠；可在调度器、IRQ-disabled 区间和普通内核线程中使用。
- 传出的引用生命周期不能超过 pin guard。
- `CpuSlotCell` 的连续操作若要求读写同一 CPU 实例，必须持有 `PinCurrentCpu`；若可能被 IRQ 访问，必须关本地 IRQ。
- `get_on_cpu` 只能返回 `T: Sync` 的共享引用；远端写入不提供无条件安全 API，必须由调用方提供同步协议。

### 禁止场景

- 在用户指针、MMIO 或 DMA 内存上调用 CPU-local API；
- 在未完成平台寄存器初始化前调用当前 CPU 访问；
- 把 `CpuSlotCell` 或其内部引用跨越可能迁移的 await/block 点；
- 在持有动态 chunk 的对象引用时销毁 chunk；
- 在 IRQ handler 与普通线程同时修改非原子 CPU-local 对象而未关闭 IRQ或加锁。

## 初始化状态机

```text
Uninitialized
    │ init_bsp(num_cpus)
    ▼
TemplateReady
    │ init_ap(cpu_id, base)
    ▼
CpuOnline  ── CpuSlotChunk::alloc() ──► DynamicReady
```

规则：

1. `init_bsp` 校验 CPU 数量并复制 BSP 模板、安装 BSP base；
2. `init_ap` 只初始化当前 CPU 的 area，不复制或修改其他 CPU 的状态；
3. CPU online 后只允许重新初始化明确标记为 resettable 的对象；
4. 不支持运行时 CPU hotplug，首版将 CPU 数量视为固定值。

## 算法流程

### 静态对象地址

```text
normal-address descriptor
        │
        ├─ offset = linker symbol VMA emitted by the macro
        │
        └─ current_ptr = arch_cpu_local_base() + offset
```

X86_64、AArch64、RISC-V、LoongArch 优先使用架构特定指令生成 offset/base 访问；无法生成单指令时使用普通指针计算。

### 静态对象复制

1. 链接脚本将模板 LMA 与 CPU area 分开，并按模板实际大小的 64 字节对齐值设置
   area stride；
2. BSP/AP 启动上下文调用 `initialize_cpu`，复制隐藏模板并安装当前 CPU base；
3. `kcpu_slot` 不维护 CPU count、online bitmap 或 BSP/AP 状态机；
4. AP 完成平台自己的 CPU ID 初始化后，再开放 IRQ。

静态对象只允许使用可安全按位复制的初始状态。需要独立构造的对象必须使用动态 CPU-local 分配或显式 per-CPU 初始化回调。

### 动态对象分配

1. `CpuSlotChunk` 为每 CPU 预留固定大小 area；
2. bump cursor 找到满足 size/alignment 的空槽；
3. 以相同 offset 计算所有 CPU 的地址；
4. 对每个 CPU 调用 `init(cpu_id)` 构造对象；
5. 任何 CPU 构造失败时，按已完成 CPU 逆序析构；预留空间不回收；
6. handle drop 对所有 CPU 调用析构；首版 chunk 不复用已释放的空间。

## 并发模型

### 本 CPU

CPU-local 的主要收益是避免跨 CPU 共享。静态值的当前 CPU 可变访问通过
`PinCurrentCpu` 保证不迁移；IRQ 重入由上层现有 IRQ guard 管理。

### 远端 CPU

远端读取只对 `T: Sync` 开放，并且只返回共享引用。远端写入不在基础 API 中提供；需要远端更新时使用：

- 原子类型；
- IPI/消息传递；
- 调用方持有的全局锁；
- CPU offline/online 期间的独占初始化路径。

### 原子与 fast path

“单指令”不等于“跨 CPU 原子”。`gs:[offset]` 形式只保证当前 CPU 对自己的实例访问短且不需要锁；跨 CPU 访问仍然必须使用原子类型或同步协议。

`CpuSlotCell` 的普通 `load/store` 可以不关 IRQ；读改写操作在没有架构单指令实现时必须临时关 IRQ，不能仅依赖 Rust 编译器生成的多条指令。

## Drop / 资源释放

- 静态 CPU-local 对象的存储随内核镜像存在，不在运行时释放；首版要求其初始值满足模板复制约束。
- 动态对象由 `CpuSlotChunk` 管理。对象必须先在所有 CPU 上析构，再释放 chunk。
- 动态对象 handle drop 负责析构自己的所有 CPU 实例；chunk 必须保持有效直到所有
  handle 释放。
- CPU offline 暂不支持；未来若实现，必须先阻止新访问、迁移或销毁该 CPU 的动态对象，再撤销其 base。

## 设计决策

### 采用的方案

1. **独立 section + offset ABI**：使用 `.cpu_slot.template`、模板 LMA 和 `_cpu_slot_*` 符号，保持 linker/架构 fast path 的优势，同时与旧 `.percpu` 完全隔离。
2. **不保留 `def_percpu` 兼容层**：新调用点只能使用 `cpu_slot!`/`cpu_slot_cell!`。迁移作为一次架构切换提交完成，目标镜像不同时启用旧 `.percpu` 与 `.cpu_slot.template`。
3. **引入 typed descriptor**：公开 descriptor 不携带 VMA 0 存储，模板和访问路径分离。
4. **拆分 `CpuSlot` 与 `CpuSlotCell`**：让“可远端共享引用”和“当前 CPU 内部可变状态”具有不同 API。
5. **统一 `PinCurrentCpu`**：表达 CPU 固定；抢占和 IRQ 的具体 guard 由平台/调度器提供。
6. **动态对象按 CPU 独立构造**：不对包含堆指针、锁或引用计数的对象执行无条件字节复制。
7. **远端写入默认关闭**：远端写入极易造成隐蔽数据竞争，优先通过原子、IPI 或锁完成。

### 明确不采用的方案

- 不直接把所有 CPU-local 变量改成 `Mutex<Vec<T>>`：会破坏本 CPU 热路径和 IRQ 场景。
- 不把旧 `def_percpu` 暴露为新 crate API：它无法表达动态对象所有权、`CpuSlotCell` 的当前 CPU 限制和 guard 生命周期。
- 不把所有访问都强制放入 `NoPreemptIrqSave`：安全但会扩大关 IRQ 区间、增加延迟。
- 不只依赖 `unsafe fn` 文档：关键 CPU 固定条件应通过 guard/生命周期表达。
- 不在首版实现 CPU hotplug：它会显著增加 storage、引用和寄存器撤销状态机的复杂度。
- 不复制 Asterinas 的页分配 chunk 到所有小对象：动态对象分配应按 size class 配置，避免小对象浪费整页。

## 安全访问 API 演进计划

当前版本已经通过 `PinCurrentCpu` 表达“执行上下文固定在某个逻辑 CPU”，但 slot
访问仍保留 `unsafe`，以便在 guard 适配尚未完成前验证底层布局和指针不变量。后续
不应让所有调用点长期手写 `unsafe`，计划按以下两层契约演进：

1. **CPU 固定契约**：由 `kspin::NoPreempt`、`NoPreemptIrqSave` 以及 IRQ/trap 或
   调度器提供的真实上下文实现 `unsafe PinCurrentCpu`。该 trait 只保证 CPU 身份和
   base 对应关系稳定，不单独证明可以安全持有 slot 引用。
2. **本地访问能力**：在 `kcpu_slot` 中增加更强的 `CpuLocalGuard`（名称可调整）
   capability trait，要求实现者同时保证任务不会迁移、访问期间不会发生会触碰同一
   slot 的本地 IRQ 重入。首版只为 `NoPreemptIrqSave` 实现该能力，不把
   `NoPreempt` 或 `IrqSave` 自动视为同等级 guard。
3. **安全访问器**：为 `CpuSlot`、`CpuSlotCell` 和 `DynamicCpuSlot` 提供接收
   `&impl CpuLocalGuard` 的安全 `get`/`get_mut`/`read`/`write`；返回引用的生命周期
   受 guard 约束。`CpuSlotCell::read`/`write` 仍只提供普通内存读写，不承诺原子性。
4. **远端读取**：继续只对 `T: Sync` 提供安全远端读取；远端写入由原子、IPI 或上层
   锁协议实现，不增加无条件的通用写接口。
5. **底层逃生口**：保留初始化、自定义 storage 和特殊架构路径所需的
   `unsafe` `*_unchecked` API，并把安全不变量集中在这些少数边界，而不是扩散到
   普通业务调用点。

该演进不改变最终镜像只保留一套 CPU-local 机制的目标；在调用点迁移完成前，
`kspin` guard 适配和安全访问器可以先独立验证，但不引入旧 `percpu` 的兼容层。

## 与现有 X-Kernel 的迁移策略

1. 第一阶段，完成新 crate 的 `.cpu_slot.template` 布局、架构寄存器和 typed API。
2. 第二阶段，将 `khal::percpu::current_task_ptr`、CPU ID、BSP 标记迁移到 `CpuSlotCell`。
3. 第三阶段，将调度器 run queue、timer runtime、softirq 状态迁移到 `CpuSlot<T>`，为访问点补齐 `PinCurrentCpu`。
4. 第四阶段，为统计计数器、可选 cache 和模块生命周期对象引入 `DynamicCpuSlot<T>`。
5. 迁移以一次架构切换完成：删除旧 `percpu` 依赖、`.percpu` 区域和旧 base owner，
   最终镜像只保留 `.cpu_slot.template` 和 `kcpu_slot`。

## 评审与验证要求

实现该 crate 前，至少需要增加：

- 单 CPU 静态访问测试；
- 多 CPU 静态复制测试；
- 动态对象按 CPU 独立初始化/析构测试；
- CPU 迁移 guard 生命周期测试；
- 非 `Sync` 类型只能通过 IRQ guard 访问的编译测试；
- 无效 CPU ID、未初始化 base、未对齐 base 和重复释放测试；
- x86_64、AArch64、RISC-V64、LoongArch64 的链接/启动 smoke test；
- 最终镜像链接测试，确认 `PinCurrentCpu` provider、架构 base 和 section 符号都存在。
