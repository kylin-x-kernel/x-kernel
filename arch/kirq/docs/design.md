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
├── lib.rs          # crate-level public surface
├── desc.rs         # OS-visible IRQ descriptor and virq/hwirq vocabulary
├── domain.rs       # per-domain lock-free reverse-map snapshots
├── state.rs        # descriptor map, virq allocation, regular/wakeup state
├── dispatch.rs     # normal IRQ fanout and wakeup subscription dispatch
├── context.rs      # IRQ execution-context tracking and diagnostics
├── softirq.rs      # fixed-vector softirq pending bits and bounded runner
├── deferred.rs     # IRQ-tail deferred-execution handoff
├── lifecycle.rs    # hardirq entry/exit extension hooks
├── msi.rs          # MSI/MSI-X allocation and message composition bridge
├── nmi.rs          # pseudo-NMI registration and lock-minimal dispatch table
├── platform.rs     # IntrManagerIf bridge and claimed IRQ completion guards
└── manager.rs      # public API and dispatch-entry orchestration
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

## 状态机

### Descriptor / handler 生命周期

```text
unknown
   -> try_resolve_and_publish(desc)
   -> descs[virq] = IrqStateDesc { desc, handler?, wake_subscription? }
   -> register() installs regular handler
   -> dispatch_subscribers()
   -> unregister() removes handler
   -> remove_if_unused() for non-MSI descriptors
```

`IrqDesc::try_merge()` 允许后来的 descriptor 补齐 trigger、polarity、controller、
domain、affinity 和 flags，但不会丢掉已经确认的 metadata。hwirq、virq、domain
或 `(domain, hwirq) -> virq` 映射冲突会返回 `IrqDescError`，release 构建中不会
静默覆盖 metadata。

### virq 映射

- 显式 `virq` 直接使用调用者指定的 OS-visible IRQ number；
- 带 `domain` 但无 `virq` 的 descriptor 通过 `(domain, hwirq)` 分配动态 virq；
- 新增 domain mapping 后重建并发布该 domain 的 immutable reverse-map snapshot；
- 未注册在 `domain.rs` 静态 registry 中的 domain 会返回 `IrqDescError::UnknownDomain`，
  不会留下控制面 mapping 或返回一条数据面不可解析的死线；
- 动态 virq 从 `DYNAMIC_VIRQ_BASE` 开始；
- 只有 plain `usize` 的旧调用保持兼容，被解释为 `IrqDesc::from_virq()`。

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
      ├── regular handler handle()
      └── wake handler(virq)
```

`OneShot` wakeup 在第一次 dispatch 后被移除，`Persistent` wakeup 保留。
当前实现要求 wakeup subscription 依附于已有 regular handler。

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

`softirq.rs` 实现 Linux-like fixed-vector foundation，使用 per-CPU pending bit。
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

1. 解析 `IntoIrqDesc`。
2. 通过 `try_resolve_and_publish()` 获得稳定 `virq`，必要时发布 domain snapshot。
3. 若 `IrqStateDesc.handler` 已存在，返回 `false`。
4. 保存 `Arc<dyn IrqHandler>`。
5. 释放 `IRQ_STATE`。
6. 调用平台 `configure()` 和 `enable(hwirq, true)`。

### normal IRQ dispatch

1. `khal::irq::irq_handler()` 创建 `NoPreempt` guard。
2. `kirq::handle_irq()` 创建 lifecycle guard，触发 `on_irq_enter`。
3. `HardIrqContextGuard` 进入 generic hardirq context。
4. 平台后端 claim pending IRQ，返回 `PendingIrq { IrqRef, completion cookie }`。
5. `dispatch_subscribers(&pending)` 通过 domain snapshot 解析 `virq`，并复制 regular
   handler 和 wakeup callback；strict domain miss 保持未解析状态。resolve 与
   descriptor lookup 在同一个 `IRQ_STATE` 临界区内完成，避免 unregister 插入两者之间。
   返回值只表示本次 claim 解析出的 OS-visible IRQ，不表示 descriptor 存在或 handler
   已服务该 IRQ；descriptor miss 会记录一次 `Unhandled IRQ` 后直接返回 resolved `virq`。
6. 在不持有 `IRQ_STATE` 的情况下调用 handler。
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
- normal dispatch 从 `IRQ_STATE` 中复制 handler/wakeup callback 后释放锁，避免
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
- `unregister()` 移除 regular handler，并在 descriptor 无 regular/wakeup 使用者时
  删除 `IRQ_STATE` 条目、禁用平台 IRQ。
- `unregister_nmi()` 同时清理 `NMI_TABLE`、fallback handler 和 `PER_CPU` 标记。
