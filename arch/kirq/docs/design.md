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
│   ├── state.rs        # descriptor map, virq allocation, and action state
│   ├── dispatch.rs     # normal IRQ action fanout
│   ├── notify.rs       # IRQ line/source waiter registration and wake dispatch
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
            │       ├── dispatch_actions(&pending)
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
    └── IRQ_CONTROL_LOCK
    └── IRQ_STATE
            ├── virq -> IrqStateDesc
            └── (domain, hwirq) -> virq

domain::Published<ReverseMap>
    └── lock-free data-plane (domain, hwirq) -> virq snapshot

configure_platform_irq() / set_platform_irq_enabled()
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

- `try_map()`、`try_enable()`、`try_register()`、`try_register_disabled()`、
  `try_enable_irq()`、`try_disable_irq_nosync()` 和 `unregister()` / `free_irq()`
  API 可以从内核普通上下文调用，内部用 `SpinNoIrq` 保护全局 IRQ state。会触发平台
  configure/enable/disable 的控制路径先获取 `IRQ_CONTROL_LOCK`，再获取
  `IRQ_STATE`，形成状态变更和平台操作的固定顺序。
- `disable_irq()`、`synchronize_irq()` 和 `free_irq()` 会等待当前 software hardirq
  dispatch 的 `in_flight` 计数归零，不能从 hardirq、softirq 或 BH-disabled context
  调用。`disable_irq_nosync()` / `try_disable_irq_nosync()` 只对已经存在的
  descriptor 做 lookup-only disable，更新嵌套 disable depth 并 mask 平台 line；
  它不会创建 descriptor、分配 `virq` 或发布 domain mapping，也不等待正在运行的
  handler。
- 兼容入口 `enable()`、`register()` 仍存在。`enable(spec, false)` 是
  `disable_irq_nosync()` 的 legacy boolean bridge，可从 hardirq 路径使用且不会等待；
  `enable(spec, true)` 是 legacy enable bridge：存在 disable nesting 时先递减 depth，
  depth 到 `0` 时启动平台 line；depth 已经是 `0` 时仍会下发平台 enable，以保留
  per-CPU/static IRQ bring-up 的既有语义。新代码应优先使用语义更明确的 `try_*` /
  `*_irq` 入口处理 descriptor 冲突和 enable/disable nesting。
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

- generic `kirq` 在 primary handler fanout 后调用
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
   -> descs[virq] = IrqStateDesc { desc, actions[] }
   -> register() installs one regular action
   -> dispatch_actions()
   -> disable_irq_nosync() masks without waiting
   -> synchronize_irq() waits for in-flight snapshots to exit
   -> unregister()/free_irq() removes regular action and synchronizes teardown
   -> remove_if_unused() for non-MSI descriptors
```

`IrqState` 是 `kirq` 控制面的 aggregate root。它以 `virq` 作为主键保存
OS-visible IRQ descriptor 与运行态，并维护按 domain 拆分的控制面
`domain_states: domain -> hwirq -> virq` 以及反向索引
`virq_to_mapping: virq -> (domain, hwirq)`。控制面 API 必须通过 `IrqState`
完成 descriptor 解析、冲突检测、动态 `virq` 分配、action 生命周期更新和 MSI
清理；hardirq 数据面只消费 `IrqState` 发布到 `domain/mod.rs` 的 immutable
reverse-map snapshot，不直接把 `IrqState::domain_states` 或 `virq_to_mapping`
当作热路径查询结构。

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

`actions` 是 `kirq` 内部 action list。`register(desc, handler)` 仍是兼容的
single-action API：它只允许空 line 上安装一个非 shared action，第二次 regular
registration 继续返回 `false` 并记录 warning。`register_shared()` /
`try_register_shared()` 会安装带 `IrqActionToken` 的 shared action；同一 line 上的
shared action 由 `kirq` 在 dispatch 中统一 fanout，`kdriver::resource` 只保存
devres token 与 `IrqActionToken` 的适配关系。shared action 当前只支持默认
auto-enable 路径，不暴露 disabled shared registration API。后续 shared action
注册如果补齐了更具体的 descriptor metadata，`kirq` 会在不改变当前 enable/depth
状态的前提下重新下发 stale platform configure。
handler 的 normal IRQ 调用参数是已经解析出的 OS-visible `virq`；shared IRQ handler
依赖这个参数识别当前 line，而不应从设备或平台私有状态反推。action 内部仍预留
future threaded slot 和 optional name，但本阶段 threaded slot 始终
不产生调度、唤醒、softirq 或 workerqueue 行为。

`IrqDescRuntimeState` 是 descriptor/action 运行态的唯一 owner。它当前保存：

- `actions`：当前安装在 line 上的 regular/shared primary action list。非 shared
  regular API 仍只能安装一个 action；shared API 最多安装固定数量的 shared action；
- `generation`：descriptor/platform configuration metadata 变化序号；action 和
  lifecycle 不递增该值；
- `configured_generation`：上一次成功下发到平台 configure 路径的 descriptor
  generation，用于避免每次 enable 都重复 configure；
- `is_enabled`：IRQ core 视角下平台 line 是否已 unmask/enable；
- `disable_depth`：嵌套 disable 深度；`try_register_disabled()` 以 depth `1` 安装
  handler，后续 `try_enable_irq()` 递减到 `0` 时才启用平台 line；
- `in_flight`：当前已经从 descriptor 复制出 callback snapshot、但还没有完成
  primary callback 的 dispatch 数量。`synchronize_irq()`、`disable_irq()` 和
  `free_irq()` 通过它提供驱动 teardown 同步语义；
- `teardown_depth`：等待式 teardown 正在进行的深度。该值非 `0` 时拒绝同一 line
  的新 action 注册，并阻止 unused descriptor 被 dispatch guard 提前删除；
- `is_msi`：从 descriptor flags 派生的 MSI lifetime marker；
- `oneshot_mask_pending`：后续 threaded/oneshot 语义预留，本阶段不驱动硬件 mask。

这些字段是 crate-private 运行态，不构成 public ABI。后续 shared IRQ、oneshot mask
和 threaded IRQ 只能在这个 owner 内扩展，不能绕过 `IrqStateDesc` 在 dispatch、
MSI free 或 unregister 路径上另建并行状态。

primary handler 的 public return 仍是 `IrqEvent`。dispatch 内部把它分类为：

- `NOT_HANDLED` -> `Unhandled`；
- `HANDLED` -> `Handled { sources: 0 }`；
- `from_sources(bits)` -> `Handled { sources: bits }`。

内部还保留 future-only `WakeThread { sources }` 分类。当前 public handler 无法返回
该分类；即使内部测试构造该值，它也只表示未来 threaded IRQ 语义，不会触发 IRQ
thread wake、softirq 或 workerqueue work。带
future-only thread slot 的 action 在当前非线程化 core 中不可 dispatch：它不会同步
运行 primary handler。

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

devres IRQ request 通过 `kirq::try_register_shared()` 安装 shared action，并把返回的
`IrqActionToken` 包装成 `device_res::IrqHandlerToken`。release 时，`kdriver` 使用
`kirq::free_irq_action()` 删除单个 action；只有最后一个 action 离开时，
`kirq` 才 mask line、等待 `in_flight` 归零并清理 descriptor。这样 shared IRQ 的
fanout、teardown 和后续 threaded/oneshot 扩展点都集中在 IRQ core，而不是隐藏在
devres adapter 的本地聚合表中。handler return 的 `IrqEvent` sources 由 `kirq`
在同一 line 的 fanout 完成后合并，并由 `kirq` 自己的 waiter notification 模块统一
唤醒 line/source waiter。`kdriver` 不再参与 IRQ wake bridge；它只负责 devres IRQ
resource 到 `kirq` action 生命周期的适配。

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
   -> dispatch_actions(&pending) resolves via domain snapshot

free_msix(virq)
   -> verify handler state is unused
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

`unregister()` / `free_irq()` do not delete MSI descriptors. MSI allocation ownership is held
by the `MsiAllocation`/`MsiResource`, so `free_msix()` is the final cleanup point
for the descriptor, domain mapping, published domain snapshot, and backend
vector. This keeps the documented device teardown order valid: unregister the
handler first, then free the MSI resource.
`free_msix()` releases the backend vector before removing the kirq descriptor and
published mapping; if backend release fails, the OS-side mapping is retained so
cleanup can be retried.

### Async notification bridge

```text
kirq action fanout
   -> merge IrqEvent sources for the resolved virq
   -> kirq::notify::dispatch_irq_event_waiters(virq, sources)
      ├── line-level PollSet wake
      └── matching source PollSet wake
```

`kirq` 不再提供旧的 line-level wake subscription API，也不再通过 `kdriver` 安装
fanout-complete hook。当前 poll/future IRQ wake 的生产路径由 `kirq::notify`
直接持有 waiter table：任务上下文注册 line/source waiter，hardirq fanout 完成后
`kirq` 在锁内 clone 匹配的 `PollSet`，释放锁后先 wake line waiter，再按 source
index wake source waiter。`drivers/irq-notify` 兼容 crate 已移除；上层内核子系统
直接调用 `kirq` 的 waiter 注册 API。

这个 notification 模块仍只是迁移桥，不是 Linux threaded IRQ、softirq 或 workerqueue
的最终模型。后续 bottom-half 能力应在 `kirq` 内形成明确的 softirq / IRQ thread /
workerqueue 所有权；等待者表和 poll/waitqueue 集成后续可继续迁移到通用
async/wait notification 层。

#### Linux 对照与后续替代形态

当前 `kirq::notify` 的生产调用方只有两类：

- `ktask::future::register_irq_waker()`，被 `ktty` 等代码用来等待某条 IRQ line 被认领；
- `knet` 的 RX source waiter，用于在网络 RX IRQ source 到来后唤醒协议栈 poll 任务。

这不是 Linux 中的通用 IRQ core 语义。Linux 网卡路径通常是：驱动 probe 时注册
NAPI poller；硬中断 handler 只确认设备事件、mask/ack 必要状态，然后调用
`napi_schedule_irqoff()` / `__napi_schedule()`；NAPI 被挂到 per-CPU poll list 后
raise `NET_RX_SOFTIRQ`；协议栈在 softirq/NAPI poll 中消费 RX ring、提交 skb，最后
由 socket 层的 `sk_data_ready()` / `sock_def_readable()` 唤醒 socket waitqueue。
也就是说，“上层任务被唤醒”属于协议栈或具体子系统的 readiness/waitqueue 语义，
不是 IRQ core 也不是 devres provider 的职责。

因此后续替代方向是：

- `kdriver::resource` / devres 只管理 IRQ resource lifetime，把 handler/action 接入
  `kirq`，不注册上层 async waiter；
- 网卡驱动通过网络子系统注册 NAPI-like poller 或 RX bottom-half 对象；IRQ handler
  在 hardirq 中只调度该 bottom half，不直接唤醒协议栈任务；
- `knet` 在 RX poll/协议栈处理产生 socket readiness 后，使用自身的 `PollSet` /
  socket waitqueue 唤醒等待者；
- `ktty` 等 line-level IRQ waiter 也应迁到对应设备/TTY 输入队列的 readiness
  waitqueue；直接等待 IRQ line 只作为过渡或诊断机制保留。

`kirq` 的长期 owner 是 hardirq dispatch、softirq、IRQ thread 和 workerqueue 等
中断执行上下文基础设施；具体“事件变为可读/可写后唤醒谁”应由网络、TTY、块设备等
上层子系统拥有。

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
      -> dispatch_actions()
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
3. 若 `IrqStateDesc` 已有 action，返回 `false`。
4. 把 `Arc<dyn IrqHandler>` 包装成内部 regular action；normal IRQ dispatch 会以
   resolved `virq` 调用 `handler.handle(virq)`。
5. 释放 `IRQ_STATE`。
6. 默认调用平台 `configure()` 和 `enable(hwirq, true)`；disabled registration 只安装
   handler 并把 `disable_depth` 置为 `1`，等待后续显式 `enable_irq()`。

### `enable_irq(desc)`

1. 解析 `IrqSpec` 到已经存在的 descriptor，不创建新 descriptor 或发布新 domain
   mapping。
2. 若该 line 正在等待 `free_irq()` / `free_irq_action()` teardown 同步，返回
   `TeardownInProgress`，避免 action 已撤销后重新打开平台 line。
3. 若该 line 没有任何 registered action，返回 `NoIrqAction`；普通显式 enable 只服务
   disabled registration / nested disable 的 action 生命周期。
4. 递减 `disable_depth`；只有深度回到 `0` 时才按需要 configure 并 unmask 平台 line。
   legacy `enable(spec, true)` 仍保留平台 bring-up 用的 force-enable 语义，但同样不能
   穿过 teardown gate；命中已有 teardown descriptor 时必须在 resolve/publish 前拒绝，
   避免失败路径 merge metadata 或插入 domain mapping。

### `free_irq(desc)`

1. 拒绝 hardirq、softirq 和 BH-disabled context 中的等待式 teardown。
2. 在 `IRQ_CONTROL_LOCK -> IRQ_STATE` 顺序下解析 `virq`。
3. 移除 regular action。
4. 如果 descriptor 已经没有 action 使用者，先 mask 平台 line，并把
   `disable_depth` 固定到至少 `1`。
5. 释放锁后等待该 `virq` 的 `in_flight` dispatch 计数归零。
6. 重新进入控制面锁并删除 unused non-MSI descriptor。MSI descriptor 仍由
   `free_msix()` 作为资源 owner 做最终释放。

### `free_irq_action(desc, token)`

1. 拒绝 hardirq、softirq 和 BH-disabled context 中的等待式 teardown。
2. 在 `IRQ_CONTROL_LOCK -> IRQ_STATE` 顺序下解析 `virq`。
3. 只移除 token 对应的 shared action；若 line 还有其它 action，保留平台 enable
   状态和 descriptor。
4. 若移除的是最后一个 action，则 mask 平台 line。
5. 释放锁后等待该 `virq` 的 `in_flight` dispatch 计数归零，保证旧 snapshot 不再
   持有被移除 action。
6. 最后一个 action 被移除时，重新进入控制面锁并删除 unused non-MSI descriptor。

### `synchronize_irq(desc)`

1. 拒绝 hardirq、softirq 和 BH-disabled context 中的等待式同步。
2. 解析 `IrqSpec` 到已存在的 `virq`，不创建新 descriptor。
3. 循环观察 `IrqDescRuntimeState::in_flight`，直到当前已进入的 callback snapshot
   全部退出。
4. 该 API 不 mask 平台 line；需要阻止新 handler 进入时应先调用 `disable_irq()`。
5. 调用方不能持有 handler 可能获取的锁或阻止目标 CPU handler 退出的资源锁，否则
   可能形成 handler 等锁、teardown 等 handler 的死锁。

`in_flight` 等待使用 descriptor-local `kpoll::Completion` 作为 wake source：
`begin_dispatch()` 在 `0 -> 1` 转换时 `reinit()`，`IrqDispatchGuard::drop()` 在
`1 -> 0` 转换时于 `IRQ_STATE` 锁内把 completion 标记为完成，并在释放
`IRQ_STATE` 后 wake waiter。这样上一代 dispatch 的 delayed wake 不能在下一代
`begin_dispatch()` 之后重新把同一个 completion 置为 completed，同时避免在全局
IRQ state 锁内运行 poll wakeup。`kirq` 通过 `IrqSyncWaitIf`
调用 scheduler 层提供的等待实现，因此不依赖 `ktask`/`ksync`。等待实现按
`try_wait/register/try_wait` 协议消费 completion token。如果 `synchronize_irq()` 无法
在当前 task context 注册 waiter，会通过 `IrqDescError::SyncWaitFailed` 暴露失败，不把
失败折叠成静默轮询；`free_irq*()` 在 action 已摘除后若同步失败则 fail-stop，避免
调用方继续释放设备资源。`Completion` 在该路径里只作为 wake source；真实完成条件必须始终是
descriptor-local `in_flight == 0`，并按 check/register/recheck 协议验证。
`synchronize_irq()` 不 mask line，持续 IRQ 仍可能让等待条件反复变为非零；teardown
路径应使用 `disable_irq()` 或 `free_irq()`。

### normal IRQ dispatch

1. `khal::irq::irq_handler()` 创建 `NoPreempt` guard。
2. `kirq::handle_irq()` 创建 lifecycle guard，触发 `on_irq_enter`。
3. `HardIrqContextGuard` 进入 generic hardirq context。
4. 平台后端 claim pending IRQ，返回 `PendingIrq { IrqRef, completion cookie }`。
5. `dispatch_actions(&pending)` 通过 domain snapshot 解析 `virq`，并复制 action
   list；strict domain miss 保持未解析状态。resolve 与
   descriptor lookup 在同一个 `IRQ_STATE` 临界区内完成，避免 free/unregister 插入
   两者之间。若 snapshot 中存在 callback work，dispatch 同步增加该 descriptor 的
   `in_flight` 计数，guard drop 时递减并尝试清理 unused descriptor。返回值只表示本次
   claim 解析出的 OS-visible IRQ，不表示 descriptor 存在或 handler 已服务该 IRQ；
   descriptor miss 或没有任何 action claim 时会记录一次 `Unhandled IRQ` 后直接返回
   resolved `virq`。
6. 在不持有 `IRQ_STATE` 的情况下按 snapshot 顺序调用所有 action primary handler，
   每个 handler 都收到 resolved `virq`，随后合并并分类每个 `IrqEvent`。若整条
   line 被认领，fanout 完成后调用 `kirq::notify` waiter dispatcher。本阶段不调度 threaded
   handler，也不做 per-action 统计。
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
- normal dispatch 从 `IRQ_STATE` 中复制 action list 后释放锁，避免
  在执行驱动 handler 时持有全局 IRQ state。resolve 和 descriptor lookup 保持在同一
  临界区内，避免并发 unregister 造成“已解析但 descriptor 已删除”的中间态。
- `in_flight` 是 descriptor-local 计数，由 dispatch snapshot guard 维护；对应的
  `in_flight_zero` completion 只负责唤醒等待者，不参与 cleanup 判定。最后一个
  guard drop 时在 `IRQ_STATE` 锁内发布 completion done state，锁外执行 wake，保证
  completion state 和 `in_flight == 0` 的观察顺序一致。
  `free_irq()` 先移除 action，action list 为空时就 mask 平台 line；随后在锁外等待
  `in_flight` 归零，最后只在 `action_count == 0 && in_flight == 0 && teardown_depth == 0`
  时清理 descriptor。平台 mask 与 descriptor cleanup 使用不同判定：并发 shared
  teardown 中即使已有 waiter 让 `teardown_depth != 0`，最后一个 action 离开仍必须
  关闭平台 line。等待期间 descriptor 保留 `teardown_depth` gate，阻止同一 line 重新
  注册或通过 `enable_irq()` 重新打开平台 line；等待式 API 禁止在 interrupt-like
  context 调用，避免当前 CPU 自己等待自己退出 handler。调用方也不能持有 handler
  可能获取的锁或阻止目标 CPU handler 退出的资源锁。
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
- `IRQ_WAITERS` 使用 `SpinNoIrq<Vec<IrqPollSets>>` 保护按 `virq` 排序的
  line/source waiter table。fanout-complete hot path 先通过 atomic entry-count
  hint 跳过完全空表，再用二分查找定位 `virq` 并只在锁内 clone 匹配的 `PollSet`，
  锁外执行 wake。新 entry 插入使用 `try_reserve()` 把 OOM 显式返回给注册方；
  descriptor cleanup 会在释放 `IRQ_STATE` 后移除该 `virq` 的 waiter entry，
  避免已注销 IRQ 的 waiter 表长期增长。
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

### 为什么移除 irq_notify crate

`irq_notify` 的剩余职责曾经只是把旧的 `register_irq_waker()` /
`register_source_waker()` 转发到 `kirq`，没有独立状态或 owner。IRQ waiter table 和
fanout-complete wake dispatch 的 owner 是 `kirq`，上层内核子系统直接依赖 `kirq`；
`kdriver` 和 devres 不承载 poll/future IRQ wake bridge。
后续 Linux-style softirq、IRQ thread 和 workerqueue 应由 `kirq` 提供；poll/waitqueue
等待者注册则由通用 async/wait notification 层承接。

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
- `unregister()` 移除 regular action，并在 descriptor 无 action 使用者时
  删除 `IRQ_STATE` 条目、禁用平台 IRQ，并移除对应 IRQ waiter entry。MSI
  descriptor 仍由 MSI resource owner 最终释放，`free_msix()` 删除 MSI descriptor
  时同样移除对应 waiter entry。
  如果 action 已经从 dispatch table 摘除，但 scheduler wait provider 无法等待
  escaped handler snapshot 退出，`free_irq()` / `unregister()` 采用 fail-stop
  语义 panic；这样避免调用方继续释放设备状态，而旧 handler 仍可能在其它 CPU 上运行。
- `unregister_nmi()` 同时清理 `NMI_TABLE`、fallback action 和 `PER_CPU` 标记。
