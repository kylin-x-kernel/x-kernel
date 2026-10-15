# kipi — 安全与可靠性分析

## 信任模型

### Fixed RCU Notification Boundary

`notify_rcu` validates the destination but not RCU readiness. Its fixed reason
only raises Rcu softirq; it cannot run callbacks or wake the GP task inline.
IRQ-off scheduler callers must release their locks before reenabling interrupts.
The destination action and services must exist before request publication; NMI
and CPU-hotplug use are excluded. Boot-pending RCU reasons use the same retained
word protocol and cannot overwrite scheduler, timer or BH reasons.

### Fixed Timer Notification Boundary

`notify_timer_reprogram` accepts only a typed CPU identifier from trusted kernel
callers, not a timer pointer or user payload. It validates bounds and CPU mapping,
but mapping presence alone does not prove service readiness. The caller publishes
timer state and releases timer locks first. `timer-reprogram` links its own static
provider; generic IPI and BH users need no scheduler or timer provider.

| Threat | Asset / trigger / impact | Response |
| --- | --- | --- |
| Lost refresh | Timer progress; independent ready/pending publication or clearing after handling loses a request | Mitigate: readiness and reason bits share one AcqRel RMW word; take reasons before processing, never clear afterward; retain pre-enable requests. |
| Cross-reason overwrite | Scheduler/timer progress; simultaneous requests erase another reason | Mitigate: fetch-or publishes independent bits, fetch-and retains ENABLED; test simultaneous reasons and requests during service. |
| Receiver deadlock | Availability; sender holds a timer lock required by the endpoint | Mitigate: release timer locks before sending; endpoint cannot sleep, invoke callbacks or acquire runqueue locks. |
| Premature endpoint access | State integrity; boot drain precedes local timer initialization | Mitigate: enablement contract includes local timer readiness; no CPU-hotplug/reset protocol. |

The new fixed timer path adds no unsafe operations, allocation, payload storage
or lifetime ownership. It relies on the interrupt backend's publish-before-notify
contract and eventual target IRQ service. Long IRQ masking remains a latency risk.

Scheduler and timer fixed-notification features require matching providers.
`ktask/ipi` enables these two features and supplies both endpoints. Built-in BH
notifications call kirq directly. The notification module is always compiled;
linking it does not enable delivery or satisfy service initialization contracts.
No empty fallback drops requests for a feature-selected endpoint.

### Fixed BH Notification Boundary

The protected asset is the static `PENDING: [PendingWord; NR_CPUS]`, including
independent ordinary/high-priority BH reasons. Only typed kernel CPU identifiers
enter the two public notification functions; no Work pointer, user data or closure
is accepted. Bounds and present-CPU mapping are checked before indexing/publication.
All existing atomic ordering and enablement invariants also protect these bits.
The BH path adds no unsafe code or allocation and does not own service storage.

| Threat | Asset / trigger / impact | Response |
| --- | --- | --- |
| Lost BH opportunity | Queue progress; clearing requests after delivery loses a concurrent kick | Mitigate: take reasons before delivery, retain ENABLED and reuse AcqRel request/enable handshake; category bits are independent. |
| Hardirq callback execution | Context integrity; an IPI directly executes business Work | Eliminate from this path: receiver only calls kirq's local softirq raise; kirq decides when a BH pass may run. |
| Premature service use | Availability; demand becomes visible before target action/state exists | Transfer readiness to the service owner: prepare and install before exposing producers; fixed ENABLED is not service readiness. |
| Early daemon absence | Progress; boot drain precedes AP ksoftirqd creation | Mitigate: retained static softirq pending and daemon's initial pending check; runtime keeps startup non-preemptible until local services exist. |
| IPI flood or masked IRQs | Availability/latency; excessive trusted notifications or blocked target | Accept existing transport limitation: reasons coalesce but post-enable calls each send an IPI; no rate/latency bound or NMI/hotplug support. |

Serial transport integration tests temporarily replace Tasklet/High actions and
restore them with kirq's scoped guards. They require quiescent production BH queues
and acknowledge callbacks before restoring handlers. They do not establish global
workng service lifetime/readiness or production-load performance acceptance.

RCU transport tests leave the boot-owned Rcu action installed. Persistent
unittest-only per-CPU counters observe delivery after the receiver marks the
vector pending. They verify deferred delivery with IRQs masked, not one-to-one
request acknowledgment: concurrent service requests may coalesce normally.

固定通知 `notify_reschedule` 不要求普通回调队列 ready，但调用方必须已向合法
目标的运行队列发布任务状态。CPU 范围/存在性由入口验证；静态槽不分配、不释放，
也不保存任务指针。正常 handler 在 ENABLED 前不得取走原因；启用与发送共用
一个原子字，避免独立 ready/pending 变量间的丢请求窗口。取走原因后不再清零，
保证处理中到达的新原因保留。硬件仍依赖 publish-before-notify 契约。
The scheduler endpoint only sets the local preemption flag; the independent
timer endpoint refreshes hardware. Neither takes a mutable RunQueue reference,
allocates or sleeps.
`enable_fixed_notifications` 只能在当前 scheduler、IPI 队列和 handler 已初始化后调用；
它保存 IRQ 状态，不使用会在析构时主动调度的禁抢占 guard。没有新增 unsafe。
调用者还须为 CPU-bound 启动路径，因为线路启用的内部锁可能触发已有抢占检查；
主核 late-init 线程固定亲和性，AP 在 IRQ 关闭且尚未设置本次 pending 前开启线路。
线路启用失败会 panic，不发布 ENABLED，避免软件 ready 与硬件线路状态失配。

`kipi` 不是用户态边界模块，它的主要调用者是内核中的页表、I-cache 和
跨 CPU 协调逻辑。信任关系如下：

```text
内核调用者
   │
   │ safe API: run_on_cpu / run_on_each_cpu / run_on_each_cpu_via_ipi
   │           page_table::TlbFlushIf / karch::IcacheFlushIf
   │
   v
┌───────────────────────────────────────────┐
│                   kipi                    │
│  ┌──────────────┐   ┌──────────────────┐  │
│  │ IPI 队列路径 │   │ TLB 协议状态路径 │  │
│  └──────────────┘   └──────────────────┘  │
└───────────────────────────────────────────┘
   │
   │ kiface / checked publication / IRQ backend
   v
平台中断控制器与 CPU 本地执行上下文
```

默认信任前提：

- 调用者在正确的执行阶段使用 `kipi`，即目标 CPU 的本地队列已初始化；
- 平台 `notify_cpu()` 实现满足 publish-before-notify 契约；
- `ipi_handler()` 能在目标 CPU 上被正常投递和执行。

## 外部边界 / 攻击面

`kipi` 不直接接收用户态输入，但它确实跨越了若干高风险边界：

1. **中断控制器边界**  
   通过 `kirq::notify_cpu()` 与 GIC/APIC/SBI 等后端交互，依赖其
   IPI 投递和发布顺序正确。

2. **per-CPU slot 引用边界**
   私有 `ipi_queue()` 从显式 CPU area 构造共享 `LazyInit<SpinNoIrq<_>>`
   引用，依赖“目标 CPU 的本地队列已经初始化”这一前提；队列修改只经锁完成。

3. **页表一致性边界**  
   `tlb.rs` 与 `page_table`、`memspace` 的一致性协议耦合，
   一旦请求发布、ack 或重试逻辑出错，可能导致旧 TLB 项继续生效。

4. **中断上下文边界**  
   `ipi_handler()` 和 TLB shootdown handler 运行在不可睡眠上下文中，
   不能持有会阻塞的锁，也不能执行长路径。

5. **回调边界**  
   广播/单播回调由其他 CPU 提供并在目标 CPU 上执行。`kipi` 只保证投递，
   不验证回调逻辑本身是否适合在 IPI 上下文运行。

本模块不直接涉及：

- 用户内存或用户指针；
- DMA 缓冲区；
- 网络/文件系统外部输入；
- FFI。

## unsafe 代码清单

### 1. `ipi_queue()` 的显式 CPU-area 访问

不变量：

- 目标 CPU 必须存在于逻辑 CPU 映射中；
- 目标 CPU 的本地 IPI 队列必须已经初始化；
- 返回值只允许作为共享引用使用；任何生产或消费都必须取得内层 `SpinNoIrq`。

防护：

- 对外 API 在远程入队前显式检查 `IPI_QUEUE_READY`；
- `IPI_QUEUE_READY` 由目标 CPU 在初始化后 Release 发布，发送端 Acquire 检查；
- accessor 不构造远端 `&mut`，多生产者和本地消费者都由同一把锁串行化。

### 2. `LAST_HANDLED_PENDING_EPOCH` 的 guard-backed per-CPU 访问

不变量：

- 只能访问当前 CPU 的本地 slot；
- 不能把别的 CPU 的 epoch 缓存错误地写入当前 CPU 本地状态。

防护：

- helper 获取 `NoPreemptIrqSave`，通过按值 `read`/`write` 完成一次访问，不生成引用；
- helper 只暴露“当前 CPU 读/写”接口，不提供跨 CPU 写路径。

### 3. 平台 `notify_cpu()` 中的内联汇编屏障

位置：

- `drivers/platform/irq/src/gicv2.rs:167`
- `drivers/platform/irq/src/gicv3.rs:86`
- `drivers/platform/irq/src/riscv.rs:83`

不变量：

- 屏障必须覆盖“请求状态已写入普通内存”到“IPI 被目标 CPU 观测”之间的顺序；
- 否则目标 CPU 可能先响应 IPI，再看见旧 request 状态。

防护：

- `IntrManagerIf::notify_cpu` 现在把该契约写成接口要求；
- AArch64 / RISC-V 后端已在发送路径显式补 barrier。

## 内存安全不变量

以下不变量对 `kipi` 的健壮性和内存安全都关键：

1. **每 CPU 队列必须先初始化再被远程访问**  
   私有 area accessor 没有运行时恢复能力，前置条件错了就是逻辑错误；
   `IPI_QUEUE_READY` 的 Release/Acquire 是该初始化发布协议。

2. **TLB 请求必须先 publish，再允许目标 CPU 看到 IPI**  
   否则目标 CPU 可能跳过 request，发起 CPU 永久等不到 ack。

3. **同一发起 CPU 的 request slot 在 active 生命周期内不可被另一个任务复用**  
   这由 `ActiveShootdownSlot` 的 no-preempt guard 维持。

4. **TLB shootdown 不能在 flush 路径内重建 residency 状态**  
   residency 由调度切换路径维护；flush 路径只允许消费一个保守快照。

5. **pending epoch 只能作为 fast path gate，不能替代真实 ack 协议**  
   epoch 只决定“要不要扫描”，真正的完成条件仍是 `acked_seq_by_cpu`。

## 线程安全

### 通用 IPI 队列

- 每个目标 CPU 的队列是多生产者、单消费者模型；
- 生产者通过 `SpinNoIrq` 串行化入队；
- 消费者只在目标 CPU 的 IPI handler 上 pop。

### TLB shootdown

- `REQUEST_SLOTS` 按发起 CPU 分片，避免 initiator 间共享单个全局 pending 状态；
- `acked_seq_by_cpu` 按目标 CPU 维度记录确认进度；
- `PENDING_EPOCH_BY_CPU` 只是降低无关 IPI 的扫描成本，不改变协议正确性；
- `needs_retry_full_flush` 用于同 CPU 嵌套请求的兜底补发，避免直接重入复用 slot。

## 威胁分析

| 编号 | 威胁 | 触发条件 | 影响 | 缓解 |
|------|------|----------|------|------|
| T-01 | 目标 CPU 在观察到 request 状态前先处理 IPI | 平台 `notify_cpu()` 缺少发布屏障 | TLB shootdown 丢失，发起 CPU 自旋等待 | 在 IRQ backend 中定义并实现 publish-before-notify 契约 |
| T-02 | 同一 CPU 上两个任务重入复用同一个 request slot | `flush_remote()` 生命周期内允许抢占 | request 状态被覆盖，可能死锁或漏 flush | `ActiveShootdownSlot` 绑定 no-preempt 与 active slot |
| T-02a | 两核 `flush_remote` 互等 ack | 等待时 IRQ 关闭，不处理对方 shootdown | libc-test 等并行 unmap 整机卡住 | 等待循环轮询 `handle_shootdown()`，不依赖本核已进 IPI handler |
| T-03 | 普通 IPI 共用同一向量导致无意义全表扫描 | 每次 IPI 都无条件扫描 TLB slots | 高频无关 IPI 造成 O(CPU_NUM) 热路径开销 | per-target `pending_epoch` fast path |
| T-04 | 调用者把不适合中断上下文的逻辑作为回调广播到其它 CPU | 回调内部睡眠、拿大锁或跑长路径 | IPI handler 延迟、锁顺序问题、可用性下降 | `kipi` 文档约束回调必须适合该执行上下文；模块不兜底 |
| T-05 | 队列未初始化前被远程入队 | AP 尚未执行 `init()`，但已经成为 IPI 目标 | 通过原始 per-CPU 引用访问未就绪槽位 | API 显式检查 `IPI_QUEUE_READY`，失败返回 `TargetCpuNotReady` |
| T-06 | 停机 IPI 无法到达关中断自旋的 CPU | 目标 CPU 长期屏蔽本地中断，永远不进入 `ipi_handler` | 该 CPU 未停靠，终点前仍在变更共享状态 | 1s 有界超时后告警并继续（对齐 Linux `smp_send_stop`），不阻塞终点 |
| T-07 | NMI 看门狗唤醒停靠 CPU 或误判故意停机为 lockup | 平台带 NMI/pseudo-NMI hard-lockup 看门狗，park 后 NMI 源仍开启 | 停靠 CPU 被反复唤醒；看门狗 panic 后走平台断电，`halt` 变成 `power_off` | park 前调用 `khal::quiesce_nmi()` 停靠本地 NMI 源（无 NMI 平台为 no-op） |
| T-08 | 停机编排者任务在协议中途被迁移 | 从可抢占的 syscall 上下文进入终点，认领 id 与广播/等待基于过期的 CPU id | 被离开的 CPU 被当作 orchestrator 永不 park，协议必然吃满超时并留下运行中的 CPU | `stop_other_cpus` 全程持有 `NoPreempt` 守卫，把编排者钉在认领的 CPU 上 |
| T-09 | 等待集包含 IPI 队列未初始化的 present CPU | AP 尚未执行 `kipi::init()` 即触发停机 | 该 CPU 永远无法 ack，每次停机固定白等 1s 超时并误报告警 | 发布时一次性快照等待集，只含队列已就绪的 CPU；广播仍覆盖全部其它 CPU |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 局部影响 | 系统影响 | 检测/缓解 |
|------|----------|----------|----------|-----------|
| F-01 | 目标 CPU IPI 队列未初始化 | `run_on_cpu` / 广播失败 | 相关跨核操作无法执行 | 返回 `KipiError::TargetCpuNotReady` |
| F-02 | IPI 队列塞满 | 当前回调无法投递 | 局部控制流失败，上层需自行恢复 | 返回 `QueueFull` |
| F-03 | 目标 CPU 长时间不 ack TLB request | 发起 CPU 自旋等待 | 页表修改路径停顿，可能表现为系统卡住 | 超时后记录 warning；等待中轮询 `handle_shootdown` 避免 IRQ-off 互等 |
| F-04 | pending epoch 未同步更新 | 目标 CPU fast path 直接返回，不扫描 slots | 单测或协议调用错误时出现假阴性 / 卡住 | 正式路径由 `flush_remote()` 封装；单测通过 helper 同步更新 |
| F-05 | callback panic | 当前回调失败 | 其它已排队回调仍继续处理 | `ipi_handler()` 继续消费队列，不让单个回调中断整个队列 |

## 故障管理

- 通用 IPI 投递路径以 `Result` 返回错误，而不是静默失败；
- TLB shootdown 的主要失败模式不是 `Result`，而是“目标 CPU 不 ack”，因此采用
  自旋等待 + 超时 warning 的诊断模型；
- 同 CPU 嵌套 TLB 请求不直接 panic，而是设置 `needs_retry_full_flush` 并在当前请求结束后补发；
- 对平台 IPI 顺序语义的要求被写入接口契约，避免隐式依赖散落在调用点。

## 隐私分析

`kipi` 不处理用户隐私数据，也不直接接触用户缓冲区、文件内容或网络载荷。  
它处理的内容主要是：

- CPU ID；
- 回调对象；
- TLB shootdown 协议状态；
- 诊断日志中的地址和 request 序号。

需要注意的是，诊断日志可能包含虚拟地址或 CPU 拓扑信息，因此仍应视为内核内部调试信息。

## 已知限制

- 通用 IPI 回调和 TLB shootdown 共用一个 IPI 向量，设计上要求 handler 顺序稳定；
- TLB shootdown 在等待 ack 时采用自旋，不适合极长延迟目标；
- `kipi` 只保证跨 CPU 投递，不保证回调本身适合在 IPI 中断上下文运行；
- `pending_epoch` 只是性能优化门槛，不是完整协议状态，因此测试若直接操作内部 slot，
  必须同步更新该 gate。

## 审计清单

- 检查所有新增 `notify_cpu()` 调用点是否依赖 publish-before-notify 语义。
- 检查是否有任何路径在 `ActiveShootdownSlot` 生命周期内进入可能睡眠的代码。
- 检查 `flush_remote` 等待 ack 时是否轮询 `handle_shootdown`（IRQ 关闭下两核互等）。
- 检查任何新测试若手工构造 request slot 状态，是否同步更新 `pending_epoch`。
- 检查新的广播回调是否真的能在 IPI handler 上下文执行，而不会睡眠或长时间占用 CPU。
- 检查新平台 IRQ backend 是否兑现 `IntrManagerIf::notify_cpu` 的顺序契约。
