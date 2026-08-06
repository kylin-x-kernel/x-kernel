# kirq — 设计文档

## 定位

`kirq` 是 X-Kernel 的通用中断核心。它位于架构 trap 适配层和平台中断
控制器后端之间，向驱动、IPI、计时器和 NMI 使用者提供统一的 IRQ 描述、
注册、映射、使能、dispatch 和完成语义。

`kirq` 位于 irqchip 驱动之上。各 irqchip 驱动只负责具体控制器行为
（configure、mask/unmask、claim、complete、priority、IPI delivery），并通过
`kirq::IntrManagerIf` 接入。中断处理核心能力和 Linux-style 扩展语义
（context、softirq、workerqueue、threaded IRQ 等）属于 `kirq`。

`khal::irq` 只保留 trap-entry adapter 和 HAL 特定 helper；generic IRQ state 与
dispatch ownership 属于 `kirq`。

## 背景

重构前，generic IRQ descriptor、domain mapping、dispatch、NMI、MSI 和后续
bottom-half 扩展点分散在 HAL 或平台/驱动 glue 中。这样容易出现两类问题：一是
不同平台对 hwirq/virq/domain 的解释不一致，二是中断热路径为了兼容控制面状态而
重新引入全局锁或隐式 identity fallback。

`kirq` 把这些语义收束到一个 ownership boundary：控制面负责 descriptor 与
domain snapshot 发布，数据面只消费稳定的 `PendingIrq`/`IrqRef` claim 和已发布的
reverse-map snapshot；trap adapter、irqchip backend、devres provider 各自只保留
自己的适配责任。

## 范围

```text
arch/kirq/src/
├── lib.rs              # crate-level public surface and stable re-exports
├── model/
│   ├── mod.rs
│   └── desc.rs         # OS-visible IRQ descriptor and virq/hwirq vocabulary
├── runtime/
│   ├── mod.rs
│   ├── action.rs       # internal regular IRQ action and return classification
│   ├── state.rs        # descriptor map, virq allocation, action/wakeup state
│   ├── dispatch.rs     # normal IRQ fanout and wakeup subscription dispatch
│   ├── nmi.rs          # pseudo-NMI registration and lock-minimal dispatch table
│   └── manager.rs      # public API and dispatch-entry orchestration
├── domain/
│   └── mod.rs          # per-domain lock-free reverse-map snapshots
├── backend/
│   ├── mod.rs
│   ├── platform.rs     # IntrManagerIf bridge and claimed IRQ completion guards
│   └── msi.rs          # MSI/MSI-X allocation and message composition bridge
└── bottom_half/
    ├── mod.rs
    ├── context.rs      # IRQ execution-context tracking and diagnostics
    ├── deferred.rs     # IRQ-tail deferred-execution handoff
    ├── lifecycle.rs    # hardirq entry/exit extension hooks
    └── softirq.rs      # fixed-vector softirq pending bits and bounded runner
```

`kirq` deliberately does not depend on `khal`, `kcpu`, `kplat`,
`kbuild_config`, or `device_res`. Architecture trap registration and x86 APIC
trap adapters remain in `khal::irq`; x86 APIC MSI vector allocation is an
irqchip/backend detail reached through `kirq::MsiBackendIf`, not through
`khal`. `IPI_IRQ` stays at Kconfig call sites such as `kipi`, not in `kirq`.
Driver devres integration is owned by `kdriver::resource`, which translates
`device_res` IRQ resources into `kirq` descriptors and handlers.

## 架构

```text
CPU trap entry
    │
    └── khal::irq adapter
            ├── NoPreempt
            ├── kirq::handle_irq(vector)
            │       ├── IrqLifecycleGuard::enter()
            │       ├── HardIrqContextGuard::enter()
            │       ├── IntrManagerIf::dispatch_irq()
            │       ├── dispatch_subscribers(&pending)
            │       │       └── resolve + descriptor lookup under IRQ_STATE
            │       ├── PendingIrq::complete()
            │       ├── drop HardIrqContextGuard
            │       ├── deferred::run_hardirq_exit_deferred()
            │       └── lifecycle guard drop
            │
            └── kirq::handle_nmi(vector)
                    ├── IntrManagerIf::dispatch_nmi()
                    ├── dispatch_nmi_handler(raw_hwirq)
                    └── DispatchedIrq::complete()
```

注册/配置路径：

```text
try_map/try_register/try_enable/unregister
    └── IRQ_STATE
            ├── virq -> IrqStateDesc
            └── (domain, hwirq) -> virq

domain::Published<ReverseMap>
    └── lock-free data-plane (domain, hwirq) -> virq snapshot

configure_and_enable_platform_irq()
    └── IntrManagerIf::{configure, enable}

alloc_msix()
    ├── MsiBackendIf::alloc_msi_vector()
    ├── MsiBackendIf::compose_msi_message()
    └── try_map() maps (MSI_DOMAIN, backend_vector) -> virq

register_nmi()
    |-- IRQ_STATE fallback metadata
    `-- NMI_TABLE keyed by hwirq
```

## 调用约束 / 执行上下文

- `try_map()`、`try_enable()`、`try_register()`、`unregister()` 和 wakeup 订阅 API
  可以从内核普通上下文调用，内部用 `SpinNoIrq` 保护全局 IRQ state。兼容入口
  `enable()`、`register()` 仍存在，但新代码应优先使用 `try_*` 入口处理 descriptor
  冲突。
- `handle_irq()` 运行在 hardirq trap adapter 调用路径中。调用者必须已经屏蔽本地
  IRQ、禁用抢占并 pin 当前 CPU，函数本身不能睡眠。
- `handle_nmi()` 运行在 pseudo-NMI adapter 调用路径中，不触碰 normal IRQ
  dispatch fanout，不调用 lifecycle hooks，也不获取 `IRQ_STATE`。
- `translate_hwirq()` 从 domain 发布的 immutable reverse-map snapshot 做 lock-free
  查询；strict domain miss 返回 `None`，不会伪装成 identity `virq`。
- `IrqLifecycleHook` 在 trap adapter 已禁抢占、normal IRQ dispatch 生命周期仍未
  结束时执行。它不是 hardirq-depth 边界；hardirq/softirq/BH 状态由
  `kirq::context` 维护。hook 不能睡眠，不能等待可能被中断上下文持有的锁，也不能
  假设当前有进程上下文。
- `DeferredExecutorHook` 在 normal IRQ controller completion 之后、generic hardirq
  depth 已退出之后、lifecycle exit 之前执行。它只是后续 deferred execution
  subsystem 的交接点，不能睡眠、不能递归调用 deferred runner，不能假设当前有进程
  或 task 上下文。
- `alloc_msix()` / `free_msix()` 必须从普通内核上下文调用。它们通过 backend
  分配控制器本地 vector 并返回 OS-visible virq 与 device-visible MSI message；
  调用方不能读取当前 APIC id 或假设 virq 等于硬件 vector。
- `IntrManagerIf::notify_cpu()` 必须满足 publish-before-notify 顺序，IPI/TLB
  shootdown 依赖该契约。

## 中断控制器后端契约

`IntrManagerIf` 是 `kirq` 和具体 irqchip driver 的边界。`kirq` 负责 descriptor、
handler fanout、上下文和 completion ordering；irqchip driver 负责具体控制器行为。

### configure / enable

- `configure(desc)` 在普通非 MSI line enable 前调用。它消费已经规范化的
  `IrqDesc` metadata，包括 trigger、polarity、controller、source、affinity 和
  flags。后端只应使用自己理解的字段，未知或无关 metadata 不应造成错误配置。
- `enable(id, on)` 的 `id` 是 controller-local hwirq 或本地 source id，不是动态
  virq。`on = true` 表示 unmask/enable source，`on = false` 表示 mask/disable
  source。
- MSI descriptor 带 `IrqFlags::MSI`，不走普通 line configure/enable。MSI enablement
  来自 bus/device 层写入设备可见的 `MsiMessage`。

### dispatch / claim / ack

- `dispatch_irq(vector)` 必须完成控制器 claim 和必要 ack，使本次 pending interrupt
  被表示为一个 `DispatchedIrq`。返回 `None` 表示 spurious/no claim，generic IRQ
  core 不运行 handler fanout，也不运行 deferred executor。
- normal dispatch 的 `DispatchedIrq::irq()` 应是 OS-visible virq。只有 legacy
  unmapped line interrupt 可以显式 fallback raw hwirq；MSI mapping miss 不能 fallback
  raw vector。
- `dispatch_nmi(vector)` 返回 raw hwirq，不依赖 normal `IRQ_STATE` translation，也不
  打开 normal IRQ window。
- GIC backend 当前在 dispatch 中读取 IAR、过滤 special id，并把 completion cookie
  留给后续 deactivate/DIR。x86 APIC/IO-APIC edge/local vector 可以在 dispatch 阶段
  early EOI，level IO-APIC 通过非零 cookie 延迟 EOI。RISC-V PLIC external IRQ 用
  claim hwirq 作为 completion cookie。LoongArch 当前 backend 在 dispatch 阶段完成
  EIOINTC，`complete_irq()` 为 no-op；后续 cleanup 必须作为单独 phase 处理，不能在
  本 foundation 里改变硬件行为。

### complete / EOI / deactivate

- generic `kirq` 在 primary handler 和 wake compatibility fanout 后调用
  `DispatchedIrq::complete()`。
- `DispatchedIrq` 是 RAII guard；若 normal control flow 没有显式 complete，drop 会
  补发 completion。guard 不是 `Send`，completion 必须发生在 claim 它的 CPU 上。
- `complete_irq(cookie)` 只完成 dispatch 返回的 cookie。level-triggered line 必须在
  handler fanout 后最终 EOI/deactivate，避免 handler 处理前重新触发。edge-triggered
  line 可以使用 cookie `0` no-op。

### notify_cpu publish-before-notify

`notify_cpu(id, target)` 必须保证调用者在 Normal memory 中发布的请求状态先于 IPI
被目标 CPU 观察。x86、GIC 和 RISC-V backend 应在各自实现中提供架构等价的 fence 或
barrier；调用者不应在每个 IPI call site 重复补 fence。

## 状态机

### Descriptor / action 生命周期

```text
unknown
   -> try_resolve_and_publish(IrqSpec)
   -> descs[virq] = IrqStateDesc { desc, regular_action?, wake_subscription? }
   -> register() installs one regular action
   -> dispatch_subscribers()
   -> unregister() removes regular action
   -> remove_if_unused() for non-MSI descriptors
```

`IrqState` 是 `kirq` 控制面的 aggregate root。它以 `virq` 作为主键保存
OS-visible IRQ descriptor 与运行态，并维护按 domain 拆分的控制面
`domain_states: domain -> hwirq -> virq`。控制面 API 必须通过 `IrqState`
完成 descriptor 解析、冲突检测、动态 `virq` 分配、action/wakeup 生命周期更新和
MSI 清理；hardirq 数据面只消费 `IrqState` 发布到 `domain/mod.rs` 的 immutable
reverse-map snapshot，不直接把 `IrqState::domain_states` 当作热路径查询结构。

`try_resolve_and_publish()` 的输入是 `IrqSpec`。`IrqSpec::PlainVirq` 表示调用者只在
OS-visible IRQ namespace 中引用一条已知 `virq`，不会携带或更新硬件 metadata；
`IrqSpec::Desc` 表示调用者提交完整 IRQ resource descriptor，可以参与 domain
mapping、descriptor merge、冲突检测和动态 `virq` 分配。

`try_resolve_spec()` 返回一个解析结果对象，携带规范化后的 descriptor 以及
`snapshot_domain_to_publish`。`try_resolve_desc()` 只处理完整 descriptor 分支，不再
根据一组 default 字段反推 plain virq 语义。`snapshot_domain_to_publish` 不是
`IrqDesc::domain` 的副本，而是表示本次解析是否新增了 `(domain, hwirq) -> virq`
mapping，从而需要重新发布该 domain 的 lock-free reverse-map snapshot。这样
mapping 发布是 descriptor 解析的显式结果，而不是隐藏在全局 dirty 标志里的副作用。

`IrqDesc::try_merge()` 允许后来的 descriptor 补齐 trigger、polarity、controller、
domain、affinity 和 flags，但不会丢掉已经确认的 metadata。hwirq、virq、domain
或 `(domain, hwirq) -> virq` 映射冲突会返回 `IrqDescError`，release 构建中不会
静默覆盖 metadata。

`regular_action` 是 `kirq` 内部 action 表示。当前 public
`register(desc, handler)` 只会构造一个 regular action，并且每个 `virq` 仍最多
存在一个 regular action；第二次 regular registration 继续返回 `false` 并记录
warning。action 内部预留 identity、action flags、future threaded slot 和 optional
name，但本阶段 threaded slot 始终不产生调度、唤醒、softirq 或 workerqueue 行为。

`IrqDescRuntimeState` 是 descriptor/action 运行态的唯一 owner。它当前保存：

- `regular_action`：当前公开 API 安装的唯一 primary handler action；
- `wake_subscription`：兼容 wake bridge 状态，不参与 action identity；
- `generation`：descriptor 运行态变化序号，仅用于后续诊断/快照基础；
- `is_msi`：从 descriptor flags 派生的 MSI lifetime marker；
- `shared_action_count`：当前只能是 `0` 或 `1`，为后续 shared fanout 预留；
- `oneshot_mask_pending`：后续 threaded/oneshot 语义预留，本阶段不驱动硬件 mask。

这些字段是 crate-private 运行态，不构成 public ABI。后续 shared IRQ、oneshot mask
和 threaded IRQ 只能在这个 owner 内扩展，不能绕过 `IrqStateDesc` 在 dispatch、
MSI free、unregister 或 wake cleanup 路径上另建并行状态。

primary handler 的 public return 仍是 `IrqEvent`。dispatch 内部把它分类为：

- `NOT_HANDLED` -> `Unhandled`；
- `HANDLED` -> `Handled { sources: 0 }`；
- `from_sources(bits)` -> `Handled { sources: bits }`。

内部还保留 future-only `WakeThread { sources }` 分类。当前 public handler 无法返回
该分类；即使内部测试构造该值，它也只表示未来 threaded IRQ 语义，不会触发 IRQ
thread wake、softirq、workerqueue work 或 wakeup subscription callback。带
future-only thread slot 的 action 在当前非线程化 core 中不可 dispatch：它不会同步
运行 primary handler，也不会触发 wake compatibility callback。

### virq 映射

- 显式 `virq` 直接使用调用者指定的 OS-visible IRQ number；
- 带 `domain` 但无 `virq` 的 descriptor 通过 `(domain, hwirq)` 分配动态 virq；
- 新增 domain mapping 后重建并发布该 domain 的 immutable reverse-map snapshot；
- 未注册在 `domain/mod.rs` 静态 registry 中的 domain 会返回 `IrqDescError::UnknownDomain`，
  不会留下控制面 mapping 或返回一条数据面不可解析的死线；
- 动态 virq 从 `DYNAMIC_VIRQ_BASE` 开始；
- plain `usize` 在 API 边界被解释为 `IrqSpec::PlainVirq`，只引用 `virq` 本身；
- `enable(usize, on)` 不携带新 metadata，但会解析到 stored identity descriptor；
  对低号 platform-static line，这仍可能按 identity `hwirq` 调用平台 enable；
- `try_map()` 只接受 `IrqDesc`，因为建立 domain mapping 必须有明确的
  `(domain, hwirq)` 资源语义，plain `virq` 不能参与硬件映射。
- 同一 `virq` 不能同时作为两条不同 `(domain, hwirq)` mapping 的目标；显式 `virq`
  插入 mapping 前必须检查反向一致性。

`kdriver::resource` 是 devres IRQ vocabulary 到 `kirq` 的边界 adapter。无
`hwirq/domain/controller` metadata 的 `IrqResource` 必须转换成
`IrqSpec::PlainVirq`；带硬件 routing metadata 的 resource 才转换成
`IrqSpec::Desc`。这样 device resource 的 plain IRQ handle 不会误入 descriptor
merge/mapping 分支。

### MSI/MSI-X

```text
alloc_msix(affinity)
   -> backend allocates controller-local vector
   -> backend composes MsiMessage { address, data }
   -> kirq maps (MSI_DOMAIN, vector) to virq
   -> caller registers handler on virq and programs device with message

x86 MSI-X dispatch
   -> hardware vector
   -> PendingIrq::Domain(MSI_DOMAIN, vector)
   -> dispatch_subscribers(&pending) resolves via domain snapshot

free_msix(virq)
   -> verify handler/wakeup state is unused
   -> remove MSI descriptor and mapping
   -> backend frees controller-local vector
```

MSI resources are edge-triggered and marked with `IrqFlags::MSI`. They do not
run through normal `IntrManagerIf::configure/enable` line configuration; the
device-visible state is the MSI message written into the PCI MSI-X table by the
bus/device layer. The x86 backend owns APIC destination selection and message
encoding, matching Linux's split between PCI MSI device domains and the x86
vector parent domain.

If an MSI-X vector fires without a matching `(MSI_DOMAIN, vector) -> virq`
mapping, normal dispatch reports an unhandled strict-domain miss. It must not
dispatch the raw APIC vector as an OS-visible IRQ.

`unregister()` does not delete MSI descriptors. MSI allocation ownership is held
by the `MsiAllocation`/`MsiResource`, so `free_msix()` is the final cleanup point
for the descriptor, domain mapping, published domain snapshot, and backend
vector. This keeps the documented device teardown order valid: unregister the
handler first, then free the MSI resource.
`free_msix()` releases the backend vector before removing the kirq descriptor and
published mapping; if backend release fails, the OS-side mapping is retained so
cleanup can be retried.

### Wakeup 订阅

```text
regular handler registered
   -> subscribe_wakeup[_once]()
   -> dispatch_subscribers()
      ├── regular action primary handle()
      └── wake handler(virq)
```

`OneShot` wakeup 在第一次 dispatch 后被移除，`Persistent` wakeup 保留。
当前实现要求 wakeup subscription 依附于已有 regular handler。
dispatch 会在 `IRQ_STATE` 锁内形成 `IrqDispatchSnapshot`，其中包含 descriptor、
regular action、wakeup callback 和是否存在 regular action 的布尔状态。若 OneShot
wake 在快照阶段被移除，并且 descriptor 不再有 action/wake 使用者，cleanup 会在
回调执行前完成。之后 primary handler 和 wake callback 都在锁外运行。

wakeup subscription 是 legacy compatibility bridge，主要服务当前
`ktask::future::poll` 的 IRQ wake 接入。它不是 action，不是 IRQ thread target，
也不是 wake-only IRQ 模型。本里程碑只隔离并记录该路径，后续应迁移到新的
async/wait notification 机制，例如 `PollSet`、waitqueue 或 event-notify；迁移前
不应继续扩展 `subscribe_wakeup*()` 的语义或参数面。

### Lifecycle hook

```text
register_irq_lifecycle_hooks()
   -> handle_irq()
      -> IrqLifecycleGuard::enter()
         ├── snapshot current hooks
         └── call on_irq_enter
      -> dispatch and complete hardirq
      -> Drop guard
         └── call snapshot on_irq_exit
   -> clear_irq_lifecycle_hooks()
```

guard 在 enter 时保存 exit hook 快照，因此处理中途清除 hook 不会破坏本次
enter/exit 配对。未注册 hook 时，IRQ entry 通过 atomic active bit 直接跳过全局
hook 锁；active bit 为真时才获取锁复制 hook 快照。lifecycle hook 描述
trap/preempt-off 生命周期，不等价于 Linux `hardirq_count` 边界。

### IRQ context

```text
handle_irq()
   -> HardIrqContextGuard::enter()
      -> platform dispatch / handler dispatch / controller completion
   -> drop HardIrqContextGuard
   -> deferred executor
```

`kirq::context` 在每 CPU state 中维护 hardirq、serving-softirq 和 BH-disabled depth。
public 查询 API 会使用 `NoPreemptIrqSave` 保护当前 CPU slot 读取，避免普通任务查询
被本地 IRQ 打断时和 hardirq guard 并发读写同一 per-CPU slot。
hardirq guard 和 softirq runner 的 IRQ-tail hot path 已经由调用方建立本地 IRQ
masked + 当前 CPU pinned 上下文，因此使用 `*_irqoff` crate-local helper 直接读写
per-CPU slot，不重复保存和恢复 IRQ state。

上下文诊断语义：

- hardirq、serving softirq 和 BH-disabled 都是 non-sleepable / interrupt-like。
- `interrupt_context_level()` 按 hardirq、softirq、BH-disabled、task 的优先级给出
  粗粒度诊断；BH-disabled 不是 hardirq，但仍不能运行 sleepable 回调。
- `is_in_interrupt_context()` 是 bottom-half gating predicate，不是 future IRQ-thread
  predicate。未来 sleepable IRQ-thread context 应是独立执行上下文，不属于 softirq、
  deferred executor，也不应通过该 predicate 表示。
- `local_bh_disable()` 在 hardirq 中调用会增加诊断计数并限流 warning；underflow
  诊断同样计数并限流，避免错误路径造成日志风暴。

`local_bh_disable()` 返回的 `LocalBhGuard` 持有 `NoPreempt` 到 drop，因此
BH-disabled 临界区不会迁移到另一个 CPU。enter/drop 更新 per-CPU depth 时额外短暂
屏蔽本地 IRQ；outermost drop 只在同一 CPU 且不处于 hardirq/serving-softirq 时尝试
direct softirq drain。`LocalBhGuard` 不是 `Send`，不能跨线程转移。

### Softirq foundation

```text
open_softirq(vec, action)
   -> SOFTIRQ_ACTIONS[vec] = action

raise_softirq(vec)
   -> current CPU SOFTIRQ_PENDING.fetch_or(bit, Release)

run_pending_softirqs()
   -> pending.swap(0, Acquire)
   -> run action snapshot in vector order
   -> handler context leak check
   -> repeat up to restart limit
```

`bottom_half/softirq.rs` 实现 Linux-like fixed-vector foundation，使用 per-CPU pending bit。
当前不创建 ksoftirqd、workerqueue thread、tasklet 或 threaded IRQ handler。当
restart limit 命中，或当前 context 不允许 direct run 时，pending work 保留在
per-CPU pending mask 中，等待后续 handoff。

`open_softirq()` 只用于 init-time 注册。本阶段没有 unregister API；runner 在
`SpinNoIrq` 下复制 action table snapshot，但不会持有注册锁执行 action。

### Deferred executor

```text
register_deferred_executor()
   -> handle_irq()
      -> dispatch_subscribers()
      -> PendingIrq::complete()
      -> run_hardirq_exit_deferred(ctx)
         └── call on_hardirq_exit(ctx)
   -> clear_deferred_executor()
```

deferred executor 是单 owner hook。`softirq::init()` 把 softirq runner 安装为当前
owner，`kruntime::init_interrupt()` 在开本地 IRQ 前调用它。没有 claim 到平台 IRQ 的
spurious vector 不触发 deferred executor。pseudo-NMI path 也不触发 deferred
executor。executor 在注册/清空控制面通过 `SpinNoIrq` 保持单 owner；IRQ-tail hot
path 从 atomic function-pointer slot 读取当前 hook，未安装 executor 时直接返回，不
获取全局锁。workerqueue、tasklet 和 threaded IRQ 后续应挂在 softirq 或显式 future
API 之上，而不是增加额外 implicit deferred owner。

## 算法流程

### `register(desc, handler)`

1. 解析 `IrqSpec`。
2. 通过 `try_resolve_and_publish()` 获得稳定 `virq`，必要时发布 domain snapshot。
3. 若 `IrqStateDesc.regular_action` 已存在，返回 `false`。
4. 把 `Arc<dyn IrqHandler>` 包装成内部 regular action。
5. 释放 `IRQ_STATE`。
6. 调用平台 `configure()` 和 `enable(hwirq, true)`。

### normal IRQ dispatch

1. `khal::irq::irq_handler()` 创建 `NoPreempt` guard。
2. `kirq::handle_irq()` 创建 lifecycle guard，触发 `on_irq_enter`。
3. `HardIrqContextGuard` 进入 generic hardirq context。
4. 平台后端 claim pending IRQ，返回 `PendingIrq { IrqRef, completion cookie }`。
5. `dispatch_subscribers(&pending)` 通过 domain snapshot 解析 `virq`，并复制 regular
   action 和 wakeup callback；strict domain miss 保持未解析状态。resolve 与
   descriptor lookup 在同一个 `IRQ_STATE` 临界区内完成，避免 unregister 插入两者之间。
   返回值只表示本次 claim 解析出的 OS-visible IRQ，不表示 descriptor 存在或 handler
   已服务该 IRQ；descriptor miss 会记录一次 `Unhandled IRQ` 后直接返回 resolved `virq`。
6. 在不持有 `IRQ_STATE` 的情况下调用 action primary handler，并分类 `IrqEvent`。
7. `PendingIrq::complete()` 完成控制器 EOI/deactivate。
8. `HardIrqContextGuard` 退出 generic hardirq context。
9. 若本次 claim 到 IRQ，`run_hardirq_exit_deferred()` 调用当前注册的 deferred
   executor；`DeferredRunContext::resolved_irq()` 携带上一步解析出的 IRQ identity，
   strict domain miss 时为 `None`，不表达 handler 是否实际运行。
   默认 owner 是 `softirq`，会在 context 允许时 drain 当前 CPU pending softirq。
10. lifecycle guard drop，触发 `on_irq_exit`。
11. 返回 `khal::irq` adapter 后释放 `NoPreempt`。

### pseudo-NMI dispatch

1. `khal::irq::nmi_handler()` 创建 `NoPreempt` guard。
2. `kirq::handle_nmi()` 调用平台 NMI-specific dispatch path claim interrupt。
3. 对 NMI claim，`DispatchedIrq::irq()` 表示 raw hwirq，而不是 translated virq。
4. `dispatch_nmi_handler(hwirq)` 从 `NMI_TABLE` 克隆 handler。
5. 在不持有 NMI 表锁的情况下调用 handler。
6. 完成 `DispatchedIrq`。

NMI path 不触发 lifecycle hooks，避免 hardirq 扩展点被用于更严格的 pseudo-NMI
执行上下文。

## 并发模型

- `IRQ_STATE` 使用 `SpinNoIrq`，因为 descriptor 状态会被普通 IRQ 路径读取和修改。
- domain reverse-map snapshot 由控制面在 `IRQ_STATE` 锁内发布；IRQ 数据面只做
  atomic load 和 immutable snapshot lookup。普通 IRQ unregister 不删除 domain
  mapping；MSI resource final free 删除 mapping 后会发布替换 snapshot。
- normal dispatch 从 `IRQ_STATE` 中复制 action/wakeup callback 后释放锁，避免
  在执行驱动 handler 时持有全局 IRQ state。resolve 和 descriptor lookup 保持在同一
  临界区内，避免并发 unregister 造成“已解析但 descriptor 已删除”的中间态。
- `NMI_TABLE` 使用 `SpinRaw`，依赖“boot-time 写入 + NMI-only 读取 + pseudo-NMI
  不同 CPU 本地不可重入”的不变量，避免 NMI path 使用会保存 IRQ state 的锁。
- `IRQ_LIFECYCLE_HOOKS` 使用 `SpinNoIrq`，只在 active bit 为真时于 entry 短暂复制
  函数指针。无 hook 的 IRQ hot path 通过 atomic active bit 跳过全局锁；hook 本体
  不在该锁内执行。
- `IRQ_CONTEXT_STATE` 是 per-CPU 普通整数状态；所有读写都通过 `NoPreemptIrqSave`
  或明确的 IRQ-off 调用约束保护。public 查询使用 `NoPreemptIrqSave`；hardirq 和
  softirq IRQ-tail hot path 使用 `*_irqoff` helper 复用调用方已经建立的
  local-IRQ-masked + CPU-pinned 上下文，避免重复保存/恢复 IRQ state。
- `SOFTIRQ_PENDING` 是 per-CPU atomic bit mask。`raise_softirq*()` 用
  `fetch_or(Release)` 发布 pending bit，runner 用 `swap(0, Acquire)` 获取 batch，
  避免 handler 运行期间重新 raise 的 bit 被清 pending 覆盖。
- `SOFTIRQ_ACTIONS` 使用 `SpinNoIrq` 保护 init-time 注册和 runner snapshot；action
  在锁外执行。
- `DEFERRED_EXECUTOR_HOOKS` 使用 `SpinNoIrq` 保护注册/清空控制面；runner hot path
  只从 atomic function-pointer slot 读取 hook，不持锁调用 owner。当前不使用全局
  reentry guard，避免 SMP 上一个 CPU 的 hardirq-exit handoff 误抑制另一个 CPU；
  后续 softirq pending state 落地时应改为 per-CPU pending/reentry 模型。
- `PendingIrq` / `DispatchedIrq` 用 RAII 保证 claimed interrupt 最终 complete，同时通过
  `PhantomData<*mut ()>` 阻止跨线程/跨 CPU 发送。

## 设计决策

### 为什么拆成独立 crate

IRQ descriptor、state、dispatch、NMI table 和 lifecycle hooks 是一组可被多个
架构/平台/驱动共同依赖的 ownership boundary。把它们保留在 `khal` 会让后续
softirq、workerqueue 和 threaded IRQ 扩展持续依赖 HAL trap-entry 细节。

### 为什么 `khal::irq` 仍然存在

`khal::irq` 现在只负责 trap handler registration 和进入 `kirq` 前的 HAL 执行约束。
它不再 re-export generic `kirq` API，也不再暴露 x86 MSI-X/APIC helper；驱动、
运行时、IPI、timer 和 irqchip 后端应直接依赖 `kirq`。

### 为什么 devres 只适配到 `kirq`

`device_res` 是驱动框架的 OS-neutral resource model。它描述驱动需要的 IRQ 资源，
但不拥有内核中断处理语义。x-kernel 的 devres provider 位于 `kdriver::resource`，
负责把 `device_res` 的 trigger/controller/domain/event/handler 转换为 `kirq`
类型。这样驱动框架获得内核 IRQ 能力，同时 `kirq` 不反向依赖驱动框架。

### 为什么 lifecycle hook 和 deferred executor 都采用单 owner

lifecycle hook 表示 hardirq enter/exit 通知，deferred executor 表示 hardirq exit
之后的下半部交接点。当前阶段两者都不需要完整 notifier chain。单 owner 注册规则让
ownership 明确，也避免多个未排序 hook 在 hardirq exit 上产生隐式依赖。后续如果
softirq 需要再分发多个 consumer，应由 softirq subsystem 显式维护 dispatcher。

### 为什么 wakeup subscription 不升级为 action

当前 wakeup subscription 是历史兼容路径：regular handler 处理 IRQ 后额外调用
wake callback，把事件桥接给现有异步等待实现。它缺少 Linux threaded IRQ 所需的
action identity、dev-id teardown、mask/oneshot、thread lifecycle 和 scheduler
handoff 语义。把它当作 `WakeThread` 或 future IRQ thread target 会把兼容 API 固化
为新模型的基础，导致后续无法清晰迁移到 wait notification / workerqueue /
threaded IRQ 分层。因此本阶段只保留兼容行为，并在 action return 中明确
`WakeThread` 不触发 wake compatibility callback。

### 为什么 NMI 独立于 normal IRQ state

pseudo-NMI 可以打断 normal IRQ critical section。如果 NMI dispatch 获取
`IRQ_STATE`，它可能和被打断的 normal IRQ 路径自锁。因此 NMI handler 表独立存储，
并以 hwirq 为 key 直接查找。

### 为什么 normal IRQ claim 使用 `PendingIrq`

平台 claim 和 OS-visible `virq` 解析是两个不同步骤。`PendingIrq` 保存 raw claim
source 与 completion cookie，让 LAPIC timer、LoongArch EXTIOI 等显式 identity
claim 可以直接派发，也让 GIC/PLIC/IO-APIC/MSI 等 domain IRQ 通过发布的
lock-free snapshot 解析，避免每次中断都查询控制面的 mapping table。

## Drop / 资源释放

- `PendingIrq` / `DispatchedIrq` 的 `Drop` 会补发 completion，避免 normal
  control flow early return 遗漏 EOI。
- `IrqLifecycleGuard` 的 `Drop` 执行 entry 时保存的 exit hook。
- `clear_deferred_executor()` 清空 hardirq-exit handoff owner；正常运行期应由
  deferred execution owner 一次性注册。
- `unregister()` 移除 regular action，并在 descriptor 无 regular/wakeup 使用者时
  删除 `IRQ_STATE` 条目、禁用平台 IRQ。
- `unregister_nmi()` 同时清理 `NMI_TABLE`、fallback action 和 `PER_CPU` 标记。
