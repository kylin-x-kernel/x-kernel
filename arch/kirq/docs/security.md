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
- MSI backend 正确实现 `MsiBackendIf` 的 vector allocation、free 和 message
  composition；
- IRQ handler 可以在 hardirq 上下文运行；
- NMI handler 满足更严格的 pseudo-NMI 执行约束；
- lifecycle hook 和 deferred executor 都只由一个明确 owner 安装；当前 deferred
  executor owner 是 softirq。
- `device_res` / devres 是驱动框架适配层，不能成为 `kirq` 的依赖。驱动资源到
  kernel IRQ core 的转换由 `kdriver::resource` 负责。

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
   regular handler、wakeup handler、NMI handler、lifecycle hook 和 deferred
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

`arch/kirq/src/context.rs` 也使用 `percpu` raw access：

- `IRQ_CONTEXT_STATE.current_ref_raw()` 用于复制当前 CPU 的 context snapshot；
- `IRQ_CONTEXT_STATE.current_ref_mut_raw()` 用于更新当前 CPU 的 context depth。

这些访问由两类上下文保护：public 查询使用 `NoPreemptIrqSave`，当前 CPU 被 pin
住且本地 IRQ 被屏蔽；hardirq guard 和 softirq IRQ-tail hot path 使用 crate-local
`*_irqoff` helper，调用方必须已经建立 local-IRQ-masked + CPU-pinned 上下文。
因此普通任务查询不会与本地 hardirq guard 并发读写同一个非原子 per-CPU slot，
IRQ-tail hardirq/softirq 也不会重复保存/恢复 IRQ state。

`arch/kirq/src/softirq.rs` 使用 `SOFTIRQ_PENDING.current_ref_raw()` 访问当前 CPU 的
per-CPU atomic pending mask。调用方在访问前 pin 当前 CPU；pending mask 自身是
`AtomicUsize`，用于处理 IRQ/softirq 之间的 pending bit 发布和获取。

其它相关 unsafe 边界位于模块外：

- 架构 trap 入口由 `kcpu` 汇编和 trap dispatch 宏进入 `khal::irq` adapter；
- 平台 IRQ backend 在 `drivers/irq` 中执行 MMIO、priority mask 和必要的汇编屏障；
- `kiface` 把平台实现绑定到 `IntrManagerIf`。

因此本模块的主要安全责任不是局部内存安全，而是保持上下文、锁顺序和 completion
语义正确。

## 内存安全不变量

- `Handler = Arc<dyn IrqHandler>` 在 dispatch 前克隆，调用期间不借用
  `IRQ_STATE` 内部存储。
- `PendingIrq` / `DispatchedIrq` 不能跨线程/跨 CPU 发送，completion 必须在 claim
  它的 CPU 上完成。
- `IRQ_STATE` 中的 `virq -> IrqStateDesc` 和 `(domain, hwirq) -> virq` 映射必须保持一致。
- 带 domain 的 descriptor 只能使用 `domain.rs` 静态 registry 中已注册的 domain；
  未知 domain 必须返回 `IrqDescError::UnknownDomain`，不能创建数据面不可解析的映射。
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

`IRQ_STATE` 使用 `SpinNoIrq` 保护 descriptor、handler 和 wakeup subscription。
dispatch path 在持锁期间解析 `PendingIrq`、查找 descriptor，并复制
handler/wakeup snapshot，然后释放锁再调用 handler。这样保留 resolve 与 descriptor
lookup 的原子性，同时避免回调重入 IRQ API 时自锁。

Domain `hwirq -> virq` reverse maps are published as immutable snapshots.
Dispatch resolves a `PendingIrq` through the snapshot rather than querying the
control-plane `mappings` table; strict misses stay misses instead of falling
back to identity dispatch.

MSI allocation 也通过 `IRQ_STATE` 建立 `(MSI_DOMAIN, backend_vector) -> virq`
映射并发布到 MSI domain snapshot。普通 `unregister()` 不删除 MSI descriptor 或
mapping；最终清理由 `free_msix()` 在确认 handler/wakeup subscription 已撤销后完成，
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
获取 batch，action table 在 `SpinNoIrq` 下 snapshot，handler 在锁外执行。当前没有
ksoftirqd；direct runner 超过 restart 上限时保留 pending 并记录诊断计数。
BH-disabled state 使用 `LocalBhGuard` RAII 管理，guard 持有 `NoPreempt` 并通过类型
约束避免跨线程 drop，从而防止在不同 CPU 上扣减错误的 per-CPU depth。

## 威胁分析

| 编号 | 威胁 | 触发条件 | 影响 | 缓解 |
|------|------|----------|------|------|
| T-01 | IRQ metadata 配置错误 | 固件或平台提供错误 trigger/polarity/domain | 中断丢失、重复触发或无法 mask | `IrqDesc` 明确携带 metadata，平台 configure 只消费规范化描述 |
| T-02 | handler 在全局 IRQ 锁内执行 | dispatch 未复制 handler 就调用 | 回调重入注册/注销路径时死锁 | `dispatch_subscribers()` 先复制 handler/wakeup，再释放 `IRQ_STATE` |
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

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 局部影响 | 系统影响 | 检测/缓解 |
|------|----------|----------|----------|-----------|
| F-01 | duplicate regular handler 注册 | 第二个 handler 被拒绝 | 设备初始化失败但不会覆盖已有 handler | `register()` 返回 `false` 并 warning |
| F-02 | unregister 后仍有 wakeup subscription | stale wakeup 被清理 | 避免 descriptor 永久保留 | `unregister()` 同步移除 stale wakeup |
| F-03 | unknown IRQ dispatch | 无 handler 可调用 | 中断被 complete，但功能事件丢失 | warning 记录 `Unhandled IRQ` |
| F-04 | NMI 未注册 | 无 NMI handler 可调用 | 性能计数等 NMI 事件丢失 | warning 记录 `Unhandled NMI` |
| F-05 | 平台 backend 不 complete 或重复 complete | 控制器状态异常 | 可能中断风暴或中断停止 | claimed IRQ guard 内部 `completed` 标记防止重复 complete |
| F-06 | trap adapter 未禁用抢占 | `handle_irq()` 执行上下文不满足约束 | lifecycle 和调度边界混乱 | `khal::irq` adapter 创建 `NoPreempt` |
| F-07 | deferred executor 递归调用 runner | executor re-enter 自身 | 栈增长或 hardirq tail 不可控 | API 文档禁止递归；softirq runner 用 serving-softirq context gating |
| F-08 | hardirq context guard 未配对退出 | hardirq depth 泄漏 | softirq/BH gating 长期误判 | guard RAII + context diagnostic counter |
| F-09 | softirq handler 泄漏 context state | handler 修改 hardirq/softirq/BH depth 后返回 | 后续 context 判断错误 | handler 前后 snapshot 比较，warning 并恢复 |
| F-10 | `local_bh_disable()` 在 hardirq 中误用 | hardirq 中增加 BH depth | direct runner 被延迟，可能暴露调用者上下文错误 | 记录 `bh_in_hardirq_warnings`，outermost drop 在 hardirq 中不 drain |
| F-11 | descriptor mapping 冲突 | `try_*` API 返回错误 | 阻止错误 metadata 进入 `IRQ_STATE` | `IrqDescError` 携带冲突字段 |
| F-12 | descriptor 使用未知 domain | `try_*` API 返回 `UnknownDomain` | 设备初始化失败但不会创建不可达 IRQ | domain registry 边界校验先于状态变更 |

## 故障管理

- 注册失败以 `bool` 返回，并尽量在改变状态前检查重复项。
- 查询类 API 使用 `Option` 表达未映射或 strict domain miss。
- dispatch 对 unknown IRQ/NMI 记录 warning，但仍让 claimed IRQ complete。
- handler panic 是不可恢复的内核错误路径，不依赖 claimed IRQ guard Drop 继续
  维持控制器状态。
- platform enable/disable 在需要平台绑定时才调用，plain dynamic virq 不强行触碰
  控制器。
- MSI descriptor 不走普通 platform configure/enable；分配失败返回 `None` /
  `Unsupported`，message compose 失败时立即释放 backend vector 并在失败时 warning。
  正常释放由 `free_msix()` 先释放 backend vector，成功后再删除 descriptor、mapping
  和 domain snapshot；backend 释放失败时保留 OS-side mapping 以便重试。
- lifecycle hook 注册是单 owner；后续如果需要多个 consumer，应显式升级为
  notifier chain，而不是让多个子系统覆盖同一全局 hook。
- deferred executor 注册是单 owner。`softirq::init()` 在启动路径安装默认 owner；
  重复注册返回 `false`，空 executor 也返回 `false`；runner 在未注册时返回
  `NoExecutor`。

## 隐私分析

`kirq` 不处理用户数据。它处理的信息主要是 IRQ number、controller metadata、
handler 引用和 CPU targeting policy。日志可能暴露 IRQ 编号或 hwirq，对普通用户
可见前应由 procfs/syslog 权限策略控制。

## 已知限制

- 目前不支持多个 regular handler 共享同一个 IRQ line；`IrqFlags::SHARED` 只是
  descriptor metadata，尚未实现 action list。
- MSI/MSI-X 当前只提供最小 allocation/message bridge，不支持 interrupt remapping、
  managed IRQ、affinity rebalance 或动态 post-enable MSI-X table entry 管理。
- lifecycle hook 目前是单 owner 模型，不是 notifier chain。
- deferred executor 目前是 softirq 的单 owner hardirq-exit handoff，不是
  workerqueue 或 threaded IRQ 实现。
- `/proc/interrupts` 仍无 per-IRQ/per-CPU 统计输出。
- wakeup subscription 当前依附 regular handler，尚未形成独立 wake-only IRQ 模型。

## 审计清单

- 新增 IRQ API 是否明确说明 hardirq/NMI/普通上下文约束。
- 新增 dispatch 路径是否在调用 handler 前释放 `IRQ_STATE`。
- 新增 NMI 功能是否避免读取或写入 normal IRQ shared state。
- 新增平台 backend 是否满足 `DispatchedIrq` completion 和 `notify_cpu`
  publish-before-notify 契约。
- 新增 MSI backend 是否只暴露 virq/message，且不让驱动读取 APIC id 或裸 CPU vector。
- 新增 deferred execution 功能是否只通过 `deferred.rs` 的 hardirq-exit handoff
  进入，而不是修改平台后端。
- deferred executor 是否在 completion 之后、lifecycle exit 之前运行。
- deferred executor 是否仍保持单 owner 且不引入 `ktask`/`kprocess` 依赖。
