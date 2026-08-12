# kirq — 安全与可靠性分析

## 信任模型

`kirq` 不直接暴露用户态 ABI。它信任内核调用者提供的 handler 和 descriptor
符合执行上下文约束，同时把以下边界视为外部依赖：

```text
内核驱动 / 子系统
   │  IrqDesc + Arc<dyn IrqHandler>
   v
kirq
   │  IntrManagerIf
   v
平台中断控制器后端
   │  MMIO / CPU priority / EOI / IPI
   v
硬件中断控制器
```

默认前提：

- 平台后端正确实现 `IntrManagerIf` 的 configure、enable、dispatch、complete 和
  notify ordering；
- 平台后端把 normal dispatch 映射为 OS-visible virq，并且不会在 MSI mapping miss
  时把 raw vector 当作 virq 派发；
- MSI backend 正确实现 `MsiBackendIf` 的 vector allocation、free 和 message
  composition；
- IRQ handler 可以在 hardirq 上下文运行；
- NMI handler 满足更严格的 pseudo-NMI 执行约束；
- lifecycle hook 和 deferred executor 都只由一个明确 owner 安装；当前 deferred
  executor owner 是 softirq。
- `device_res` / devres 是驱动框架适配层，不能成为 `kirq` 的依赖。驱动资源到
  kernel IRQ core 的转换由 `kdriver::resource` 负责。
- `IrqAction`、`IrqDescRuntimeState` 和 `IrqActionSnapshot` 是 `kirq` 内部状态
  边界。驱动、devres 和 irqchip backend 不能直接复用或修改这些结构。
- `kirq::notify` 是当前 poll/future IRQ wake 的 owner；原 `irq_notify`
  forwarding crate 已移除。该路径不能被扩展成 threaded IRQ、softirq 或 workerqueue
  的所有权边界。

## 外部边界 / 攻击面

1. **设备中断输入**
   真实硬件或模拟设备可以触发 IRQ。`kirq` 不信任设备行为，只按平台后端
   claim 出来的 interrupt id 进行 descriptor lookup 和 handler dispatch。

2. **固件/平台 metadata**
   device tree、ACPI 或平台静态表会生成 `IrqDesc`。错误 metadata 可能导致
   trigger、polarity、domain 或 hwirq 配置错误。

3. **平台控制器后端**
   GIC/APIC/PLIC 后端负责 MMIO、priority、EOI/deactivate 和 IPI 发送。`kirq`
   通过 trait contract 约束它们，但不直接验证 MMIO side effect。

4. **MSI backend**
   x86 APIC 等后端负责把 controller-local vector 和 CPU/APIC destination 编码成
   device-visible MSI message。`kirq` 只保存 virq 映射和消息载体，不解析具体
   架构格式。

5. **内核 handler / hook 回调**
   regular handler、NMI handler、lifecycle hook、waiter wake 和 deferred
   executor 都是内核提供的回调。IRQ core 只管理调用顺序，不验证回调内部是否睡眠
   或拿错锁。

6. **trap adapter 边界**
   `kirq` 不注册 CPU trap handler。`khal::irq` 必须在进入 `handle_irq()` /
   `handle_nmi()` 前建立正确的 trap-entry 执行约束。

本模块不直接处理用户指针、DMA buffer、文件内容、网络报文或 FFI。

## unsafe 代码清单

`arch/kirq/src` 的本地 unsafe 边界位于 `domain::Published<T>`：

- `get()` 通过 `AtomicPtr::load(Acquire)` 读取已发布快照；
- `publish()` 用 `Box::into_raw` 发布新快照；
- 旧快照故意泄漏，避免 racing IRQ reader 悬垂；
- `Published<T>` 没有 `Drop`，只用于 static domain registry。

安全不变量是：快照发布前完全初始化，发布后不可变，且永不释放。

`arch/kirq/src/bottom_half/context.rs` 也使用 `percpu` raw access：

- `IRQ_CONTEXT_STATE.current_ref_raw()` 用于复制当前 CPU 的 context snapshot；
- `IRQ_CONTEXT_STATE.current_ref_mut_raw()` 用于更新当前 CPU 的 context depth。

这些访问由两类上下文保护：public 查询使用 `NoPreemptIrqSave`，当前 CPU 被 pin
住且本地 IRQ 被屏蔽；hardirq guard 和 softirq IRQ-tail hot path 使用 crate-local
`*_irqoff` helper，调用方必须已经建立 local-IRQ-masked + CPU-pinned 上下文。
因此普通任务查询不会与本地 hardirq guard 并发读写同一个非原子 per-CPU slot，
IRQ-tail hardirq/softirq 也不会重复保存/恢复 IRQ state。

`arch/kirq/src/bottom_half/softirq.rs` 使用 `SOFTIRQ_PENDING.current_ref_raw()` 访问当前 CPU 的
per-CPU atomic pending mask。调用方在访问前 pin 当前 CPU；pending mask 自身是
`AtomicUsize`，用于处理 IRQ/softirq 之间的 pending bit 发布和获取。

其它相关 unsafe 边界位于模块外：

- 架构 trap 入口由 `kcpu` 汇编和 trap dispatch 宏进入 `khal::irq` adapter；
- 平台 IRQ backend 在 `drivers/irq` 中执行 MMIO、priority mask 和必要的汇编屏障；
- `kiface` 把平台实现绑定到 `IntrManagerIf`。

因此本模块的主要安全责任不是局部内存安全，而是保持上下文、锁顺序和 completion
语义正确。

## 内存安全不变量

- regular/shared IRQ handler 被包装成 `IrqAction` 后存入 `IRQ_STATE`；dispatch 前
  克隆 action list，调用期间不借用 `IRQ_STATE` 内部存储。
- dispatch snapshot 若包含 regular/shared action，会增加 descriptor-local
  `in_flight` 计数。`free_irq()` 返回前必须等待该计数归零，避免驱动释放 MMIO/DMA
  后仍有旧 handler snapshot 运行。
- 内部 `WakeThread` return 分类在当前里程碑是 inert future state，不能调度 task、
  raise softirq 或创建 workerqueue work。
- `PendingIrq` / `DispatchedIrq` 不能跨线程/跨 CPU 发送，completion 必须在 claim
  它的 CPU 上完成。
- `IntrManagerIf::complete_irq(cookie)` 必须只消费本次 dispatch 返回的 completion
  cookie。generic handler fanout 之前不能对 level-triggered line 做最终
  EOI/deactivate。
- `IRQ_STATE` 中的 `virq -> IrqStateDesc` 和 `domain -> hwirq -> virq` 映射必须保持一致。
- 带 domain 的 descriptor 只能使用 `domain/mod.rs` 静态 registry 中已注册的 domain；
  未知 domain 必须返回 `IrqDescError::UnknownDomain`，不能创建数据面不可解析的映射。
- `try_disable_irq_nosync()` 必须保持 lookup-only；未知 IRQ 返回
  `IrqDescError::UnknownIrq`，不能在 hardirq-safe disable 路径中创建 descriptor 或
  domain mapping。
- Domain reverse-map snapshots are immutable views of the build table. Writers
  publish a replacement snapshot after mapping changes; old snapshots are never
  mutated or freed while data-plane readers may still hold them.
- MSI allocation 必须只向调用者暴露 virq 和 `MsiMessage`；backend-local vector、
  APIC id 和 affinity 选择不能泄露到 `device_res` 或 generic driver。
- NMI dispatch 不得依赖 `IRQ_STATE`，否则 pseudo-NMI 可能打断 normal IRQ 持锁路径并
  自锁。
- lifecycle exit hook 必须与本次 enter 捕获的 hook 配对，不能在处理中途因全局 hook
  被清理而丢失。lifecycle 表示 trap/preempt-off 生命周期，不是 hardirq-depth 边界。
- hardirq depth 必须由 `HardIrqContextGuard` 配对维护，并在 claimed normal IRQ 完成
  controller completion 后、deferred executor 运行前退出。
- `LocalBhGuard` 必须在整个 BH-disabled 生命周期内持有 `NoPreempt`，并且不能跨线程
  移动。这样 enter/drop 和 outermost drain 都绑定在同一 CPU 的 per-CPU depth 上。
- deferred executor 只能在 claimed normal IRQ 完成 controller completion 且 generic
  hardirq depth 已退出后运行；它不能持有 `DispatchedIrq` 或影响 completion ownership。
- softirq pending bit 不能通过 `load + store(0)` 获取；runner 必须使用 atomic
  `swap(0, Acquire)`，raise 必须用 `fetch_or(Release)`，避免 handler 期间重新 raise
  的 work 被覆盖。

## 线程安全

### normal IRQ state

`IRQ_STATE` 使用 `SpinNoIrq` 保护 descriptor 和 action list。
dispatch path 在持锁期间解析 `PendingIrq`、查找 descriptor，并复制 action
snapshot，然后释放锁再调用 handler。这样保留 resolve 与 descriptor lookup 的原子性，
同时避免回调重入 IRQ API 时自锁。

Domain `hwirq -> virq` reverse maps are published as immutable snapshots.
Dispatch resolves a `PendingIrq` through the snapshot rather than querying the
control-plane `domain_states` table; strict misses stay misses instead of falling
back to identity dispatch. The control plane also keeps `virq_to_mapping` as the
reverse index for the same mappings, so "one virq maps to at most one hardware
line" is checked without scanning all domains.

Waiting teardown APIs (`disable_irq()`, `synchronize_irq()`, and `free_irq()`)
are valid only from ordinary kernel context. They reject hardirq, softirq, and
BH-disabled callers before waiting on `in_flight`, because those contexts may be
the same execution path that must exit to make progress.
Callers must also avoid holding locks or resources that their IRQ handler may
acquire while waiting; otherwise teardown can wait for a handler that is blocked
behind the caller's own lock.

MSI allocation 也通过 `IRQ_STATE` 建立 `(MSI_DOMAIN, backend_vector) -> virq`
映射并发布到 MSI domain snapshot。普通 `unregister()` 不删除 MSI descriptor 或
mapping；最终清理由 `free_msix()` 在确认 handler state 已撤销后完成，
先释放 backend vector，再移除 descriptor/domain mapping 并发布替换 snapshot。
若仍有 handler 或 backend 释放失败，释放路径记录 warning 并保留 OS-side mapping，
暴露 handler/resource 生命周期顺序或 backend 状态问题，并允许调用方重试。

### NMI table

`NMI_TABLE` 使用 `SpinRaw`。这依赖以下约束：

- 常规写入发生在 boot-time 或 NMI handler 自身；
- normal IRQ 和进程上下文不读写 NMI 表；
- pseudo-NMI 不会在同一 CPU 上嵌套打断另一个 pseudo-NMI handler。

### Lifecycle hooks

`IRQ_LIFECYCLE_HOOKS` 只保存两个函数指针。`IrqLifecycleGuard::enter()` 复制 hook
快照，并把 exit hook 存入 guard。回调不在锁内执行。hook 运行于 trap/preempt-off
生命周期内，但不作为 hardirq-depth 状态来源。

### IRQ context state

`IRQ_CONTEXT_STATE` 是 per-CPU 普通计数状态。查询和 guard 更新都使用
`NoPreemptIrqSave` 或明确的 IRQ-off 调用约束保护当前 CPU slot。public 查询自己
建立 guard；hardirq 和 softirq IRQ-tail hot path 复用调用方已经建立的
local-IRQ-masked + CPU-pinned 上下文。

hardirq、serving-softirq 和 BH-disabled 都被视为 non-sleepable / interrupt-like
状态。`interrupt_context_level()` 显式区分 BH-disabled 和普通 task context，避免
diagnostics 把 BH-disabled 误报成可睡眠上下文。未来 IRQ thread 是 sleepable context，
必须另建执行上下文边界，不能复用 `is_in_interrupt_context()` 作为 thread-state 标记。
异常 context 诊断使用原子计数并限流 warning，避免 hot path 中的错误状态造成日志风暴。

### Deferred executor

`DEFERRED_EXECUTOR_HOOKS` 只保存一个 hardirq-exit 函数指针，并用 `SpinNoIrq`
保护注册/清空控制面。runner hot path 从 atomic function-pointer slot 读取 hook，
不获取注册锁。executor 运行在 normal hardirq exit / NoPreempt 上下文，不能睡眠、
不能依赖当前进程/task 上下文、不能递归调用 deferred runner，也不能获取可能由被
中断上下文持有的 sleepable 锁。

当前不加入全局 reentry guard，因为它会在 SMP 上抑制其它 CPU 的合法 hardirq-exit
handoff；softirq pending state 使用每 CPU pending mask 和 context gating。

### Softirq state

`SOFTIRQ_PENDING` 是 per-CPU atomic bit mask。softirq runner 通过 atomic exchange
获取 batch，action table 在 `SpinNoIrq` 下 snapshot，handler 在锁外执行。runner
入口/出口保持 local IRQ masked，但 action loop 内临时打开 local IRQ，并在检查新
pending bit 前重新关闭。`ktask` 提供 per-CPU `ksoftirqd/N`，但 `kirq` 只通过
`SoftirqDaemonIf` 请求唤醒当前 CPU daemon，不创建任务或阻塞。hardirq、active
softirq 和 BH-disabled context 中 raise softirq 只保留 pending bit；direct runner
超过 restart 上限时保留 pending、记录诊断计数，并唤醒 `ksoftirqd/N`。
`raise_softirq_irqoff()` 期望调用方已经关闭 local IRQ；若 debug/UT 路径发现误用，
会记录诊断并临时关闭 local IRQ 后再访问 per-CPU pending/context state。
BH-disabled state 使用 `LocalBhGuard` RAII 管理，guard 持有 `NoPreempt` 并通过类型
约束避免跨线程 drop，从而防止在不同 CPU 上扣减错误的 per-CPU depth。

## 威胁分析

| 编号 | 威胁 | 触发条件 | 影响 | 缓解 |
|------|------|----------|------|------|
| T-01 | IRQ metadata 配置错误 | 固件或平台提供错误 trigger/polarity/domain | 中断丢失、重复触发或无法 mask | `IrqDesc` 明确携带 metadata，平台 configure 只消费规范化描述 |
| T-02 | handler 在全局 IRQ 锁内执行 | dispatch 未复制 action 就调用 | 回调重入注册/注销路径时死锁 | `dispatch_actions()` 先复制 action，再释放 `IRQ_STATE` |
| T-03 | 数据面 mapping miss 被当作 identity virq | strict domain 未发布或缺失映射 | 错 handler 或错误确认中断 | `PendingIrq` 通过 domain snapshot 解析；strict miss 保持 unhandled |
| T-04 | NMI path 获取 normal IRQ state | pseudo-NMI 打断 normal IRQ 持锁区 | 同 CPU 自锁或 NMI 延迟 | NMI 使用独立 `NMI_TABLE`，dispatch 不触碰 `IRQ_STATE` |
| T-05 | claimed IRQ 未 complete | normal control flow 提前返回或忘记显式 complete | 控制器认为中断仍 active，后续中断异常 | claimed IRQ guard Drop 补偿 completion |
| T-06 | lifecycle enter/exit 不配对 | IRQ 中途 clear/re-register hook | hardirq nesting 或 deferred work 状态损坏 | guard 保存 entry 时的 exit hook 快照 |
| T-07 | hook/handler 睡眠或持有错误锁 | 调用者违反 hardirq/NMI 上下文约束 | deadlock、latency spike 或调度状态错误 | rustdoc 和设计文档明确回调上下文，IRQ core 不提供 sleepable dispatch |
| T-08 | IPI 请求先于内存发布被目标 CPU 观察 | 平台 `notify_cpu` 缺少顺序保证 | TLB shootdown 等跨核协议漏处理 | `IntrManagerIf::notify_cpu` 文档要求 publish-before-notify |
| T-09 | deferred executor 在 completion 前或 hardirq depth 内运行 | executor work 拉长 active IRQ、影响 EOI/deactivate 或被误判为 hardirq | interrupt tail latency、controller 状态异常或 context 诊断错误 | `handle_irq()` 固定顺序为 dispatch、complete、drop hardirq context、deferred、lifecycle exit |
| T-10 | 全局 reentry guard 抑制其它 CPU | SMP 上一个 CPU 的 executor 运行期间另一个 CPU 被跳过 | lost handoff opportunity | 当前不实现全局 guard；后续使用 per-CPU pending/reentry |
| T-11 | per-CPU context 非原子状态被本地 IRQ 并发访问 | 普通任务查询被 IRQ 打断，同时读写同一 CPU slot | context snapshot 不一致或 data race | public path 用 `NoPreemptIrqSave`，hardirq/softirq IRQ-tail path 只走已 masked/pinned 的 `*_irqoff` helper |
| T-12 | softirq pending 获取覆盖新 raise | runner 用 `load + store(0)` 清 pending | softirq work 丢失 | raise 用 `fetch_or(Release)`，runner 用 `swap(0, Acquire)` |
| T-13 | BH-disabled guard 跨 CPU drop | guard 生命周期没有 pin 当前 CPU 或可跨线程移动 | per-CPU BH depth 泄漏、underflow 或在错误 CPU drain softirq | `LocalBhGuard` 持有 `NoPreempt` 且不是 `Send` |
| T-14 | driver 读取当前 APIC id 生成 MSI message | MSI message 由设备层而非 IRQ backend 编码 | SMP/affinity/x2APIC 下中断投递到错误 CPU | `MsiBackendIf::compose_msi_message()` 是唯一 message composition 边界 |
| T-15 | MSI vector 与 handler 生命周期失配 | 设备释放 MSI resource 时 handler 仍注册，或 handler 注销提前删除 MSI mapping | vector reuse 后旧 handler、stale mapping 或 backend vector 泄漏 | `unregister()` 保留 MSI descriptor；`free_msix()` 检测 descriptor 是否仍被使用并完成最终清理；驱动 teardown 应先撤 handler 再释放 MSI resource |
| T-16 | descriptor 冲突被静默合并 | hwirq/domain/virq metadata 不一致 | handler 绑定到错误 IRQ 或平台 configure 错误 | `try_resolve_desc()` / `try_merge()` 运行时返回 `IrqDescError` |
| T-17 | MSI mapping 丢失后 raw vector 被当成 virq | backend vector 无 `(MSI_DOMAIN, vector)` 映射 | 误派发到同号 OS IRQ | MSI dispatch 使用 strict MSI domain snapshot；miss 只记录 unhandled，不 fallback raw vector |
| T-18 | 未注册 domain 被映射成功 | 控制面接受未知 `IrqDomainId` | 驱动拿到永远不可达的 virq | `try_resolve_and_publish()` 在改状态前返回 `UnknownDomain` |
| T-19 | BH-disabled 被误判为普通 task context | diagnostics 只区分 hardirq/softirq/task | sleepable future callback 可能在 BH-disabled 区间运行 | `InterruptContextLevel::BhDisabled` 和 docs 明确其 non-sleepable 语义 |
| T-20 | context misuse 日志风暴 | underflow 或 hardirq 中反复 local_bh_disable | 日志淹没真实故障，拉长 IRQ 处理时间 | warning 计数保留，日志只输出初始样本和 2 的幂次样本 |
| T-21 | poll/future IRQ wake 被当作 future IRQ thread target | 后续 threaded IRQ 直接复用 poll/future wake bridge | 缺少 thread lifecycle、dev-id teardown、oneshot mask 和 scheduler ownership，导致悬空唤醒或错误 teardown | 文档明确 poll/future IRQ wake 只是临时 waiter notification；threaded IRQ、softirq 和 workerqueue 必须在 `kirq` 内形成独立所有权 |
| T-21b | 多个 worker 并发 drain 同一 queue | provider 在未扩展队列状态机前创建多个 `system_wq` consumer | `RunningAndQueued` follow-up 可能被提前观察为空或造成顺序异常 | M4 只启动一个 `kworker/system_wq`；`run_one_work()` rustdoc 要求 provider 串行化同一 queue |
| T-21c | workerqueue 等待式 API 在 interrupt-like context 调用 | hardirq/softirq/BH-disabled 中调用 `flush_work()` 或 `cancel_work_sync()` | 当前 CPU 等待自身 bottom-half 退出，导致死锁或调度状态错误 | KIRQ 在进入 `WorkqueueSyncWaitIf` 前执行 context gate 并返回 `InvalidContext` |
| T-21d | work callback 等待自身完成 | callback 中调用 `flush_work(self)` 或 `cancel_work_sync(self)` | self-deadlock，worker 永远无法完成当前 callback | KIRQ 通过 `WorkerqueueTaskContextIf` 在当前任务上记录 opaque work key，对同一 work 返回 `SelfWait`，不依赖 CPU affinity |
| T-21e | work callback 嵌套 drain 其它 work | callback 中调用 `run_one_work()` 执行 B，再在 B 中等待外层 A | task-local 单层 context 被内层覆盖，漏检 `flush_work(A)` self-deadlock | M4 禁止 nested `run_one_work()`；当前 task 已有 worker context 时直接返回 `false` 且不消费 pending work |
| T-21f | 设备生命周期 work 被强行 leak、提前释放或 wrong-queue cancel | workerqueue API 要求 `&'static Work`，队列只保存裸指针，或 cancel API 依赖调用方传入 owner queue | 多设备/热插拔驱动无法释放 work，或 queued/running callback 访问已释放设备状态 | M4C 使用 refcounted `WorkItem`；queue/running callback 持有 handle clone，enqueue 不分配；pending owner queue 记录在 `WorkItem` 状态中，teardown 通过 `cancel_work_sync(work)` / `flush_work(work)` 收敛生命周期 |
| T-21g | single-consumer worker callback 等待同队列 pending work | W1 callback 中 `flush_work(W2)`，且 W2 pending 在同一 queue，或 W2 running 后又被 requeue 到该 queue | 唯一 consumer 被 W1 阻塞，W2 永远无法运行并完成，形成永久死锁 | KIRQ 在 pending `WorkState` 中记录 owner queue key，在 task-local context 中记录当前 queue key；同队列 pending flush 和 worker callback 中 flush running work 都返回 `SelfWait`，等待循环也会重查该谓词 |
| T-22 | oneshot runtime 字段被误当成已实现行为 | 后续代码只改 flag，没有安装 mask protocol | oneshot IRQ 无法屏蔽重入或误解除 mask | review 要求 oneshot mask 和 thread identity 在 `IrqDescRuntimeState` owner 内成组实现 |
| T-23 | threaded slot 提前保存 task/worker 指针 | 未建立线程生命周期、取消、CPU 亲和和退出同步 | handler return 后唤醒已释放对象，或在 hardirq 中触发 sleepable 路径 | 当前 `IrqThreadSlot` 不携带目标，带 slot 的 action 不会被当前 dispatch 路径同步执行；后续 milestone 必须先定义所有权和 teardown ordering |
| T-24 | action snapshot 与 cleanup 分离不完整 | dispatch 多次取锁并依赖跨锁状态 | unregister/free 与 dispatch 交错后调用 stale callback 或保留 stale mapping | `IrqActionSnapshot` 在一个 `IRQ_STATE` 临界区内形成完整快照，回调只使用快照 |
| T-25 | stale 平台 enable/disable 操作乱序 | register/enable/disable/unregister 在不同 CPU 上交错，平台操作在 `IRQ_STATE` 锁外执行 | 已释放或已 disable 的 line 被迟到 enable 重新打开 | 控制路径通过 `IRQ_CONTROL_LOCK -> IRQ_STATE -> platform op` 串行化；hardirq dispatch 不获取控制锁 |
| T-26 | 驱动需要清 pending 后再开 IRQ，但 request 立即 enable | 复杂设备 request handler 后尚未初始化 DMA ring/MMIO 状态 | 中断 handler 观察未初始化设备状态 | `try_register_disabled()` 以 disable depth `1` 安装 handler，驱动完成初始化后显式 `try_enable_irq()` |
| T-27 | handler teardown 后仍在运行 | free/unregister 只移除 `Arc`，不等待已经复制的 dispatch snapshot | 驱动释放 MMIO/DMA 后旧 handler 访问已释放状态 | dispatch guard 维护 `in_flight`，`free_irq()` 先 mask unused line，再通过 descriptor-local completion 等待计数归零；completion done state 在 `IRQ_STATE` 锁内发布、wake 在锁外执行 |
| T-28 | 等待式 IRQ API 在 interrupt-like context 调用 | hardirq/softirq/BH-disabled 中调用 `disable_irq()`、`synchronize_irq()` 或 `free_irq()` | 当前 CPU 等待自己退出，导致死锁或长时间自旋 | `try_*` API 使用 `is_in_interrupt_context()` gating 并返回 `InvalidContext`；兼容 wrapper 记录 warning 后返回失败值 |
| T-28b | IRQ sync wait provider 无 current task | scheduler 尚未初始化或非 task context 绕过上层 gate 调用等待 provider | provider 内部 `block_on()` panic，或等待 API 伪成功 | `ktask` provider 在阻塞前检查 current task，失败时返回 `PollRegisterError::InvalidState`，KIRQ 映射为 `SyncWaitFailed`；`free_irq*()` 在 action 已摘除后同步失败会 fail-stop，避免释放未同步设备状态 |
| T-29 | shared IRQ token 释放错误 action | devres adapter 保存的 token 与 kirq action identity 不一致，或 token 在旧 action 同步前复用 | 释放错误 handler、旧 handler 继续访问已释放设备状态 | token 是 line-local `IrqActionToken`，free-by-token 先摘除 action 再等待 `in_flight`，token 单调递增且不回收 |
| T-30 | shared 和 non-shared action 混用 | 普通 `register()` 后又注册 shared action，或反向混用 | dispatch/teardown 语义不明确，可能错误关闭 line | `try_register()` 只允许空 line，`try_register_shared()` 拒绝已有 non-shared action |
| T-31 | IRQ waiter 在 fanout 完成前被唤醒 | 每个 shared action 返回后立即唤醒 waiter | waiter 观察到其它 shared action 尚未 ack/完成 | `kirq` 合并整条 line 的 `IrqEvent` sources，fanout 完成后通过 `kirq::notify` 统一唤醒一次 |
| T-32 | free/register 在 teardown wait 窗口重叠 | action 已摘除但旧 snapshot 仍在运行时，同一 virq 被重新注册 | 新旧 handler 生命周期交叠，后续 threaded/worker teardown 难以定义 | `teardown_depth` gate 在等待期间阻止新 action 注册，并阻止 descriptor 被提前 cleanup |
| T-33 | handler 无法识别触发的 IRQ | shared handler 回调没有 IRQ 参数，只能依赖外部状态推断 | 共享 line、diagnostics 或通用 wrapper 误判事件来源 | normal IRQ handler 统一接收 resolved `virq` 参数；NMI handler 接收 raw hwirq |
| T-34 | 并发 shared teardown 漏掉平台 mask | 最后一个 action 离开时已有 teardown waiter，cleanup gate 被误用于 disable 判定 | descriptor 被清理但平台 line 仍 unmasked，后续中断落到 stale/unknown line | 平台 disable 只看 action list 是否为空；descriptor cleanup 才同时要求 `in_flight == 0` 和 `teardown_depth == 0` |
| T-35 | teardown 期间失败注册仍修改 descriptor/mapping | 注册路径先 resolve/publish，再检查 teardown gate | 返回失败但留下 merged metadata 或新 domain mapping | 注册先做 lookup-only teardown gate，命中后不进入 resolve/publish |
| T-36 | teardown wait 窗口重新 enable action-less line | 最后一个 action 已摘除并 mask 后，另一 CPU 调用 `enable_irq()` / legacy force-enable | descriptor 被删除后平台 line 又 unmasked，后续 unhandled IRQ 或 storm；或失败路径留下 merged metadata/domain mapping | `try_enable_irq()` lookup-only 解析已存在 descriptor，并拒绝 `teardown_depth != 0` 或 `action_count == 0` 的普通 enable；legacy force-enable 在 resolve/publish 前先检查已有 teardown descriptor |
| T-37 | 等待式 teardown 持有 handler 需要的锁 | task context 持 driver lock 调用 `free_irq()` / `synchronize_irq()`，旧 handler snapshot 在另一 CPU 等同一把锁 | teardown 等 handler，handler 等 teardown caller，形成死锁 | public rustdoc 和设计文档明确等待时不能持有 handler 可能获取的锁或阻止 handler 退出的资源锁 |
| T-38 | IRQ waiter 表拖慢所有 handled IRQ | fanout-complete path 对每个 IRQ 扫描全局 waiter 表，或已注销 IRQ 的 waiter entry 永久保留 | timer/IPI/设备 IRQ 在全局锁上竞争，运行时间越长 hot path 成本越高 | waiter table 按 `virq` 排序并用二分查找，空表通过 atomic entry-count hint 跳过锁；descriptor cleanup 在释放 `IRQ_STATE` 后移除对应 waiter entry 并唤醒遗留 waiter |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 局部影响 | 系统影响 | 检测/缓解 |
|------|----------|----------|----------|-----------|
| F-01 | duplicate regular handler 注册 | 第二个 action 被拒绝 | 设备初始化失败但不会覆盖已有 handler | `register()` 返回 `false` 并 warning |
| F-02 | duplicate shared action token 释放 | token 与目标 line 不匹配或已经释放 | 目标 handler 未移除或释放失败 | `free_irq_action()` 只按 line-local token 删除 action，失败返回 `None` |
| F-03 | unknown IRQ dispatch | 无 handler 可调用 | 中断被 complete，但功能事件丢失 | warning 记录 `Unhandled IRQ` |
| F-04 | NMI 未注册 | 无 NMI handler 可调用 | 性能计数等 NMI 事件丢失 | warning 记录 `Unhandled NMI` |
| F-05 | 平台 backend 不 complete 或重复 complete | 控制器状态异常 | 可能中断风暴或中断停止 | claimed IRQ guard 内部 `completed` 标记防止重复 complete |
| F-06 | trap adapter 未禁用抢占 | `handle_irq()` 执行上下文不满足约束 | lifecycle 和调度边界混乱 | `khal::irq` adapter 创建 `NoPreempt` |
| F-07 | deferred executor 递归调用 runner | executor re-enter 自身 | 栈增长或 hardirq tail 不可控 | API 文档禁止递归；softirq runner 用 serving-softirq context gating |
| F-08 | hardirq context guard 未配对退出 | hardirq depth 泄漏 | softirq/BH gating 长期误判 | guard RAII + context diagnostic counter |
| F-09 | softirq handler 泄漏 context state | handler 修改 hardirq/softirq/BH depth 后返回 | 后续 context 判断错误 | handler 前后 snapshot 比较，warning 并恢复 |
| F-10 | `local_bh_disable()` 在 hardirq 中误用 | hardirq 中增加 BH depth | direct runner 被延迟，可能暴露调用者上下文错误 | 记录 `bh_in_hardirq_warnings`，outermost drop 在 hardirq 中不 drain |
| F-10b | BH-disabled raise 被误当作可立即调度 daemon | BH-disabled 临界区中 raise softirq 后抢先唤醒 daemon | 同 CPU direct drain 语义变弱，产生额外 wake 和测试竞态 | `wake_softirqd_if_needed()` 把 BH-disabled 视为 interrupt-like context，只保留 pending；outermost BH enable 或 restart fallback 再处理 |
| F-10c | `raise_softirq_irqoff()` 被 IRQ-enabled caller 误用 | 调用方未满足 `*_irqoff` 前置条件 | `irq_context_snapshot_irqoff()` 读取 per-CPU context state 的前置条件被破坏 | 记录 `irqoff_misuse_warnings` 并在访问 pending/context state 前临时关闭 local IRQ |
| F-11 | descriptor mapping 冲突 | `try_*` API 返回错误或兼容 API warning/fail-fast | 阻止错误 metadata 进入 `IRQ_STATE` | `IrqDescError` 携带冲突字段 |
| F-12 | descriptor 使用未知 domain | `try_*` API 返回 `UnknownDomain` | 设备初始化失败但不会创建不可达 IRQ | domain registry 边界校验先于状态变更 |
| F-13 | controller completion cookie 错配 | complete 错误 active IRQ 或 no-op | 中断丢失、重复触发或 priority/deactivate 状态泄漏 | `PendingIrq`/`DispatchedIrq` 持有 opaque cookie，generic IRQ core 只把原 cookie 交还 backend |
| F-14 | spurious vector 运行 IRQ tail | 没有 claim 到真实 IRQ 却运行 handler/deferred | 错误 handler 调用或 pending work 时序混乱 | `dispatch_irq()` 返回 `None` 时 `handle_irq()` 不运行 fanout 和 deferred executor |
| F-15 | IRQ waiter table owner 漂移 | fanout 已处理事件但没有通知 async waiter，或其它 crate 重新实现 wake bridge | poll/future 等待者可能延迟到下一次事件，或 shared fanout 顺序被破坏 | waiter table 和 fanout-complete dispatch 由 `kirq::notify` 持有；`kdriver` 和 devres 不参与 wake bridge |
| F-16 | future `WakeThread` 被同步执行 | `WakeThread` return 被当成 handled primary work | future thread 未实现时错误唤醒或执行 sleepable work | 当前 `WakeThread` 分类保持 inert，threaded IRQ milestone 必须显式实现 thread ownership |
| F-17 | action runtime 字段漂移 | `generation`、MSI marker、action list 或 oneshot pending 与 descriptor 不一致 | MSI free 判断、shared/threaded 诊断或 cleanup 决策错误 | descriptor merge/update 统一经过 `IrqStateDesc` 方法，后续新增字段必须同步在该 owner 内维护 |
| F-18 | disable nesting 不配对 | 多次 disable 后只 enable 一次，或 legacy boolean API 语义不清 | IRQ line 长期 masked 或过早 unmask | `disable_depth` 在 `IrqDescRuntimeState` 中显式维护，`enable(spec,false)` 映射 nosync disable，`enable(spec,true)` 映射 depth-aware enable |
| F-19 | free_irq 与 dispatch 并发 | teardown 移除 action 时另一个 CPU 已复制 handler snapshot | descriptor 被删除但旧 handler 仍执行 | `in_flight` 阻止 unused descriptor 删除，`free_irq()` 等待 snapshot guard drop 后再完成 cleanup |
| F-20 | synchronize_irq 未先 disable | 等待期间设备继续触发新中断 | 调用可能长期等待或只能观察当前窗口 | 文档规定 `synchronize_irq()` 不 mask line；teardown 使用 `disable_irq()` / `free_irq()` |
| F-21 | 释放 shared line 的一个 action 时关闭整条线 | free-by-token 不检查剩余 action | 其它共享设备中断丢失 | `free_irq_action()` 只在最后一个 action 移除后产生 platform disable plan |
| F-22 | enable_irq 作用于未知或无 action descriptor | 显式 enable 创建 descriptor 或 unmask 没有 handler 的 line | 未处理 IRQ 或错误平台状态 | `try_enable_irq()` 不创建 descriptor，未知 IRQ 返回 `UnknownIrq`，无 action 返回 `NoIrqAction` |
| F-23 | enable_irq 与 free_irq wait 重叠 | teardown 已 mask line 但 descriptor 尚未清理 | platform line 在 action 删除后被重新打开 | `teardown_depth` 让 `try_enable_irq()` 返回 `TeardownInProgress` |

## 故障管理

- 注册失败以 `bool` 返回，并尽量在改变状态前检查重复项。
- 查询类 API 使用 `Option` 表达未映射或 strict domain miss。
- dispatch 对 unknown IRQ/NMI 记录 warning，但仍让 claimed IRQ complete。
- handler panic 是不可恢复的内核错误路径，不依赖 claimed IRQ guard Drop 继续
  维持控制器状态。
- platform enable/disable 在需要平台绑定时才调用，plain dynamic virq 不强行触碰
  控制器。
- platform dispatch 返回 `None` 的 spurious vector 不运行 handler fanout 或 deferred
  executor。
- context misuse 诊断使用 counters 保留完整次数，warning 输出限流。
- `disable_irq()`、`synchronize_irq()` 和 `free_irq()` 在 interrupt-like context 中通过
  `try_*` API 返回 `InvalidContext`；兼容 wrapper 记录 warning 后返回失败值。
  `disable_irq_nosync()` 是唯一可在 hardirq-like 路径使用的 non-waiting disable API。
- `free_irq()` 在移除最后一个 action 时先 mask non-MSI line，再等待
  `in_flight` 归零并删除 unused descriptor。等待期间 `teardown_depth` 阻止同一 line
  重新注册，也阻止 `enable_irq()` 重新打开 action-less line。普通 unregister 只是兼容
  wrapper。等待式 API 调用方不能持有 handler 可能获取的锁。
- MSI descriptor 不走普通 platform configure/enable；分配失败返回 `None` /
  `Unsupported`，message compose 失败时立即释放 backend vector 并在失败时 warning。
  正常释放由 `free_msix()` 先释放 backend vector，成功后再删除 descriptor、mapping
  和 domain snapshot，并同步删除 `virq_to_mapping` reverse index；backend 释放失败时
  保留 OS-side mapping 以便重试。
- lifecycle hook 注册是单 owner；后续如果需要多个 consumer，应显式升级为
  notifier chain，而不是让多个子系统覆盖同一全局 hook。
- IRQ waiter notification 由 `kirq::notify` 持有。fanout hot path 在 waiter table
  锁内 clone 匹配的 `PollSet`，锁外 wake；兼容 crate 和 kdriver 不能另建 dispatch
  owner。
- deferred executor 注册是单 owner。`softirq::init()` 在启动路径安装默认 owner；
  重复注册返回 `false`，空 executor 也返回 `false`；runner 在未注册时返回
  `NoExecutor`。
- workerqueue 的 `system_wq` 由 `kirq` 拥有队列和 work 状态，`ktask` 只通过
  `WorkerqueueHostIf` 提供一个 `kworker/system_wq` 执行上下文。当前
  `system_wq` 是 single-consumer 模型；其它队列需要 owner 显式 drain，不能假定会被
  ktask 自动执行。
- workerqueue self-wait 诊断通过 `WorkerqueueTaskContextIf` 使用任务本地 opaque
  work key 和 queue key。callback 可以 sleep/yield；诊断不能依赖 per-CPU slot，否则
  worker 迁移后会漏检 self-wait、同 queue pending wait 或污染旧 CPU 状态。work key
  来自 `WorkItem` 底层 allocation identity；queue entry 和 running callback 都持有
  `WorkItem` clone，避免 work 状态在仍可观察时被释放。当前只维护单层 context，不能
  表达嵌套 worker stack，因此 `run_one_work()` 在已有 current-work context 时拒绝嵌套
  drain。
- `flush_work()` / `cancel_work_sync()` 使用 `WorkqueueSyncWaitIf` 阻塞等待 work
  变为 idle。等待式 API 拒绝 hardirq、serving-softirq 和 BH-disabled context；
  pending owner queue 是 `WorkItem` 状态的一部分，cancel 路径从 work 反查并移除
  pending entry，避免调用方传错 queue 后把未取消 work 当成 idle。`cancel_work_sync()`
  在 running work 上发布 `Canceling` 状态，阻止新的 queue attempt 在 teardown
  等待窗口内重新排队。
  驱动释放设备状态前仍必须停止新的 queue 来源，并等待或取消相关 `WorkItem`；callback
  捕获设备对象时应使用 owner-safe 引用，避免 work 与设备之间形成不可释放强引用环。

## 隐私分析

`kirq` 不处理用户数据。它处理的信息主要是 IRQ number、controller metadata、
handler 引用和 CPU targeting policy。日志可能暴露 IRQ 编号或 hwirq，对普通用户
可见前应由 procfs/syslog 权限策略控制。

## 已知限制

- `kirq` 已支持同一 IRQ line 上的 fixed-capacity shared action list，但尚未提供
  per-action statistics、procfs 输出、debug-shirq 注入或 Linux-style dev_id 指针 ABI。
- MSI/MSI-X 当前只提供最小 allocation/message bridge，不支持 interrupt remapping、
  managed IRQ、affinity rebalance 或动态 post-enable MSI-X table entry 管理。
- lifecycle hook 目前是单 owner 模型，不是 notifier chain。
- deferred executor 目前是 softirq 的单 owner hardirq-exit handoff，不是
  workerqueue 或 threaded IRQ 实现；workerqueue 只通过显式 `queue_work()` handoff
  到 `kworker/system_wq`。
- `/proc/interrupts` 仍无 per-IRQ/per-CPU 统计输出。
- poll/future IRQ waiter notification 不是最终 waitqueue、softirq、workerqueue 或
  threaded IRQ 模型；IRQ waiter table 当前由 `kirq::notify` 持有。
- `synchronize_irq()` 通过 descriptor-local completion 和 `IrqSyncWaitIf` provider
  等待 `in_flight` 归零；completion 只是 wake source，等待返回后仍必须重查
  descriptor predicate。后续 IRQ thread / workerqueue 引入后可以复用同一等待边界，
  但上下文禁止规则保持不变。
- `flush_work()` / `cancel_work_sync()` 当前只覆盖普通 work item，不提供 delayed
  work、workqueue destruction、barrier work、CPU hotplug drain 或 rescuer 语义。
- `WakeThread`、`IrqThreadSlot` 和 oneshot pending 只是 foundation state。它们尚未绑定
  scheduler task、workerqueue、mask protocol、thread teardown、CPU affinity 或
  per-action statistics；带 future-only thread slot 的 action 在当前 core 中不可
  dispatch，不能降级为 hardirq 同步 primary。

## 审计清单

- 新增 IRQ API 是否明确说明 hardirq/NMI/普通上下文约束。
- 新增 dispatch 路径是否在调用 handler 前释放 `IRQ_STATE`。
- 新增 action return 分类是否保持 `NOT_HANDLED` 为 unhandled，而不是 threaded wake。
- 新增 `WakeThread` 处理是否仍和 `kirq::notify` 迁移桥分离，除非 threaded IRQ 设计
  已经建立独立所有权。
- 新增 threaded IRQ 设计是否先定义 thread slot ownership、cancel/teardown ordering、
  oneshot mask/unmask、CPU affinity 和 sleepable context 边界。
- 新增 shared IRQ 设计是否在 `IrqDescRuntimeState` 内维护 action list、dev-id identity
  和 fanout result aggregation，而不是只增加 flag 或计数。
- 新增 wake notification 功能是否走新的 async/wait notification 机制，而不是扩展
  `kirq::notify` 迁移桥或重新引入 `subscribe_wakeup*()` API。
- 新增 NMI 功能是否避免读取或写入 normal IRQ shared state。
- 新增平台 backend 是否满足 `DispatchedIrq` completion 和 `notify_cpu`
  publish-before-notify 契约。
- 新增平台 backend 是否只在 generic handler fanout 后对 level IRQ 做最终
  EOI/deactivate。
- 新增 context check 是否把 BH-disabled 视为 non-sleepable，而不是普通 task context。
- 新增 diagnostics 是否保留计数、避免分配，并且不会在 hot path 里无限刷 warning。
- 新增 MSI backend 是否只暴露 virq/message，且不让驱动读取 APIC id 或裸 CPU vector。
- 新增 deferred execution 功能是否只通过 `bottom_half/deferred.rs` 的 hardirq-exit handoff
  进入，而不是修改平台后端。
- deferred executor 是否在 completion 之后、lifecycle exit 之前运行。
- deferred executor 是否仍保持单 owner 且不引入 `ktask`/`kprocess` 依赖。
