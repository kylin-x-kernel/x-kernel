# kwork — 安全与可靠性分析

## 信任模型

`kwork` 信任 callback 是内核代码，但不信任调用者一定遵守执行上下文、producer 停止
顺序、等待依赖或资源生命周期约束。所有 sleepable API 都必须经过 context/dependency
gate。

普通 system/custom/dynamic queue callback 在 task-context shared worker pool 中执行，
可以睡眠。BH queue callback 在 softirq context 中执行，不能睡眠、不能阻塞。

## 外部边界 / 攻击面

`kwork` 本身不直接处理用户态 ABI 或设备寄存器，但它位于多个内核边界之间：

- 调用方边界：任意内核模块可提交 callback、flush/cancel/destroy work；
- scheduler 边界：`ktask` 提供 worker task、manager task、wait、timer、sleep/resume/tick
  accounting；
- IRQ 边界：`kirq` 提供 bottom-half softirq raise/drain 和 interrupt-like context 判定；
- timer 边界：delayed work 依赖 provider timer handle 和 generation 检查；
- teardown 边界：callback 捕获的外部对象生命周期由调用方维护，不由 `kwork` 自动保护。

经检查，本模块：

- 不直接访问用户内存或用户指针；
- 不直接操作 MMIO / PIO 寄存器；
- 不直接管理 DMA buffer 或 device-owned memory；
- 不使用 FFI、inline assembly 或 architecture-specific raw interface；
- 不直接解析 bootloader、firmware、device tree 或 ACPI 数据；
- 不直接处理文件系统、网络、IPC 外部输入。

主要攻击面是内核调用方误用、provider 契约失配、异步 teardown 竞态、错误上下文中阻塞、
以及 shared worker pool 资源耗尽。

## unsafe 代码清单

`kwork` 不包含 `unsafe` block、unsafe trait 实现或裸指针解引用。对象 identity 使用
地址值作为 opaque key，但不会通过该值重建引用。

## 内存安全不变量

- `ScheduledWork` 和 `DelayedScheduledWork` 由 `Arc` 持有；pending entry、
  running callback、timer waker 都持有 handle；
- dynamic queue owner 持 `WorkQueueHandle` clone，pending/running/delayed 实例不会悬空；
- `WorkInstanceId` 必须随 queued/running/delayed 实例检查；stale timer/tick/finish 只能
  no-op 或进入 stale 修复路径，不能提交新状态；
- `WorkerExecutionToken` 必须匹配当前 worker slot execution，防止 slot 复用后的旧 tick
  或 finish 影响新 callback；
- entry 的 `binding_key` 必须与 resolved binding 的 queue identity 一致；
- pool take/cancel/barrier 必须复核 binding、pool key、binding key、instance id；
- callback 捕获的外部对象必须由调用方同步 teardown；`ScheduledWork` 的 Arc 只保护
  scheduled instance 本身，不保护外部资源生命周期。
- `ScheduleAttrs` 只表达一次 schedule 的 queue target 和可选 CPU 绑定；显式
  `queue_work(&ScheduledWork)` 调用按传入 queue 投递。
- `ScheduledWork::new()` 和 `DelayedScheduledWork::new()` 会分配
  work 实例；IRQ-like producer 只能使用提前创建好的 `ScheduledWork` 入队。
- `BudgetedPoller` 用 `Idle/Scheduled/Running/RunningPending` 原子状态保证单 executor owner；
  运行中到达的 notify 必须转换为 follow-up round，不能丢 wake，也不能并发执行 poll
  callback；
- `BudgetedPoller` backing work 入队失败不能留下无 executor 的不可重投递 pending 状态；
  idle publish 和 follow-up requeue 失败会恢复为 idle，后续 notify 必须能重新尝试投递；
- `BudgetedPoller` 内部双 backing `ScheduledWork` 只保护后续 round 投递，不拥有调用方的协议
  状态、设备状态或 wait source。

## 线程安全

`kwork` 使用 `SpinNoIrq` 保护 work、queue、queue-pool binding 和 pool 热状态。状态提交和计数更新在锁内
完成，provider wake、callback、timer register/cancel 和 task wait 在锁外执行。

并发不变量：

- `WorkerPool::runnable_count` 等于 pool pending store 中 `Runnable` entry 数；
- `WorkQueuePoolState::nr_active` 只统计该 binding 占用的 active token；
- inactive 激活必须按 `binding_key` 过滤；
- in-flight color 计数包含普通 work 和 linked barrier；
- queue flush snapshot 必须覆盖目标 queue 的全部 per-CPU queue-pool binding；
- color 不得复用仍 in-flight 的值；
- `nr_running` 只统计仍参与 bounded concurrency 的 worker；
- Sleeping 和 CPU-intensive worker 不计入 `nr_running`；
- wait source 只是 wake hint，醒来后必须重查真实 predicate。

## 威胁分析

| 编号 | 威胁描述 | 影响 | 触发条件 | 缓解 |
|---|---|---|---|---|
| T-01 | IRQ-like context 调用 flush/cancel_sync/destroy | 调度错误或死锁 | hardirq、serving-softirq、BH-disabled 路径调用 sleepable API | `WorkqueueContextIf` gate 返回 `InvalidContext` |
| T-02 | callback 等待自身 work | 永久自锁 | running callback 调用 flush/cancel_sync 自己 | task-local work key 返回 `SelfWait` |
| T-03 | callback 等待同 bounded pool 中依赖自己推进的 target | pool worker 耗尽 | worker 持有唯一 concurrency slot 时阻塞等待同 pool work | pool key dependency gate 返回 `SelfWait` |
| T-04 | pool 未安装或 CPU 无效 | work 无执行者 | early boot 或无效 CPU target | enqueue 返回 `WorkerUnavailable`/`InvalidCpu`，不提交状态 |
| T-05 | shared pending store 满 | work 丢失或半提交 | pool pending capacity 用尽 | 返回 `QueueFull`，work 保持原状态 |
| T-06 | dynamic destroy 与 producer 竞争 | teardown 后 callback 访问外部对象 | producer 未停止就 destroy/free 外部状态 | destroy gate 拒绝后续 enqueue；调用方必须先停止 producer |
| T-07 | dynamic handle 提前 drop | queue UAF | pending/running/delayed owner 未持引用 | owner/timer/running path 持 `WorkQueueHandle` clone |
| T-08 | stale delayed timer 到期 | 已取消 work 被重新投递 | timer fire 晚于 cancel/mod/disable | generation 和 instance gate |
| T-09 | max_active token 错绑 queue | 其它 queue 被节流或计数泄漏 | active/inactive 操作未按 binding 过滤 | entry `binding_key`；激活/取消/finish 只处理相同 binding |
| T-10 | stale shared-pool entry | active/in-flight 永久泄漏 | work state 与 pool entry 不一致 | take 路径复核 state，修复 queue-pool accounting 并 warning |
| T-11 | running `ScheduledWork` 重新 queue | 调用方误以为同一实例下一轮已安排 | callback 内 queue 同一 scheduled instance | 返回 `AlreadyQueued`；需要并行或后续实例时创建另一个 `ScheduledWork` |
| T-12 | queue flush 漏等其它 CPU | flush 过早返回 | custom/dynamic queue 有多 CPU binding | flush snapshot 覆盖全部 per-CPU queue-pool binding |
| T-13 | CPU-bound callback 长时间运行 | shared pool 中其它 work 延迟 | callback 超过 CPU-intensive threshold | scheduler runtime/tick 打标并触发 kick |
| T-14 | worker 创建失败 tight retry | 资源不足时放大压力 | manager 创建 worker 失败后立即重试 | manager-needed 保留 + retry deadline cooldown |
| T-15 | reclaim-critical work 使用普通 queue | 内存压力下 forward progress 不保证 | 调用方需要 Linux `WQ_MEM_RECLAIM` 语义 | `WQ_MEM_RECLAIM` unsupported，待 rescuer 实现 |
| T-16 | BH callback 睡眠或等待 workqueue | softirq context 死锁 | BH work 调用 sleep/blocking API | BH wait gate 返回 `InvalidContext`，`ktask::sleep*()` fail-fast |
| T-17 | budgeted poller 运行中 notify 丢失 | 网络/设备 backlog 停止推进 | poll callback 正在 running 时 producer 只看到 busy | `RunningPending` missed-wake 状态在 release owner 时转回 `Scheduled` |
| T-18 | budgeted poll callback 长时间独占 worker | 同 pool work 延迟 | callback 不受 budget/max rounds 约束 | 调用方必须在 `poll_once` 内遵守 budget；poller 用 `max_background_rounds` 强制分批 |
| T-19 | budgeted poller backing work 投递失败后挂起 | 数据面停在 `Scheduled` 无 executor | shared pool 满或 worker 暂不可用 | idle publish 和 follow-up requeue 失败恢复为 `Idle`；后续 notify 对 `Scheduled` 重新投递 |
| T-20 | IRQ-like context 调用分配型 schedule helper | IRQ 路径内存分配或失败路径不可控 | hardirq/softirq/BH-disabled 路径调用 `ScheduledWork::new()` 或 `DelayedScheduledWork::new()` | 调用方在 task/init context 先创建 work，IRQ-like 路径只调用 `ScheduledWork::schedule()` 或 `queue_work(&ScheduledWork)` |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 局部影响 | 系统影响 | 恢复方式 |
|---|---|---|---|---|
| F-01 | `QueueFull` | work 未提交 | producer 可能丢失异步动作 | producer 重试或上层背压 |
| F-02 | worker 创建失败 | pool 暂无新增 worker | queued work 延迟 | manager cooldown 后自动重试 |
| F-03 | wait provider 返回错误 | sleepable API 返回 `WaitFailed` | 调用方 teardown/flush 未完成 | caller 保留 handle，可重试 |
| F-04 | delayed timer fire 时 pool 满 | delayed work 保持可重试状态 | delayed callback 延迟 | cancel 可清理，flush/mod 可推动重试 |
| F-05 | stale pending entry | callback 不执行 | 说明状态机被破坏或存在竞态 | 修复 accounting，完成 barrier，输出 warning |
| F-06 | provider 漏 wake | pending work 延迟 | worker pool 不前进 | 后续 enqueue/tick/block/manager 评估可再次 kick；测试需覆盖 |

## 故障管理

错误通过 typed result 返回：enqueue 路径使用 `QueueWorkResult` /
`QueueDelayedWorkResult`，sleepable wait 路径使用 `WorkqueueError`。状态提交与 wake
分离：锁内完成状态和计数，锁外执行 provider wake。wait source 只负责通知，等待方醒来后
重查 work/queue-pool/queue predicate。

内部 stale 状态不 panic；路径会尽量修复 active/in-flight/barrier accounting 并输出 warning。
测试中的 intentional stale case 需要明确断言 warning 场景是预期行为。

## 隐私分析

`kwork` 不保存用户数据，不解析用户输入。queue/work 名称和 opaque key 只用于内核日志、
诊断和测试断言。callback 捕获的数据属于调用方模块，不由 `kwork` 解释或复制。

## 已知限制

- `WORKQUEUE_PENDING_CAP` 是固定容量；pending store 仍需最终性能阶段评估 linked-list 化；
- `WORKQUEUE_WORKERS_PER_POOL` 是固定 worker 上限，尚无 idle culling/self-exit；
- `WQ_MEM_RECLAIM` / rescuer 未实现；
- CPU hotplug drain/rebind 未实现；
- ordered/unbound/NUMA node active 共享未实现；
- custom/dynamic BH queue 未开放；
- Linux BH `cancel_work_sync()` non-hardirq atomic 特例未实现；
- queue flush 尚无完整 Linux flusher queue/overflow 接力。
- `BudgetedPoller` 当前只提供 task-context dynamic queue backing；BH/unbound/CPU-hotplug-aware
  budgeted poller 还未实现。

## 审计清单

- 底层模型是否仍是 `WorkQueue impl WorkQueueRuntime -> WorkQueuePoolBinding -> WorkerPool -> Worker`；
- runtime 名称是否只在 `runtime/` 和 public helper 层出现；
- 每个 pending/running/delayed 状态转换是否带 instance id 校验；
- worker slot 操作是否带 `WorkerExecutionToken` 校验；
- enqueue 失败是否不留下半提交 work/queue-pool/pool 状态；
- active、runnable、running、in-flight 是否守恒；
- inactive 激活是否按 binding 过滤；
- barrier 是否不占 active quota 且计入正确 color；
- flush_workqueue 是否覆盖全部 CPU queue-pool binding，并避免复用 in-flight color；
- provider wake、callback、wait、timer 操作是否都在锁外；
- task-context sleep accounting 是否只覆盖真实 block，不覆盖 yield；
- BH callback 是否始终运行在 softirq 非睡眠上下文；
- UT 是否隔离共享 system pool，避免 live worker 抢跑造成非确定性；
- 新增 flag/API 是否有真实机制支撑，不能静默降级。
