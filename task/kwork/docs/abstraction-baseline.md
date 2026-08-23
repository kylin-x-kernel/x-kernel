# kwork — 抽象边界基线

## 目的

本文记录 `kwork` 抽象借鉴工作的 W0 基线。它用于约束后续重构：先明确现有类型职责，再逐步调整内部边界；任何步骤都不得绕开当前 Linux workqueue 风格的内核语义。

## 当前公开语义对象

| 类型 / API | 角色 | 兼容性要求 |
|---|---|---|
| `ScheduledWork` | 调用方持有的 callback work 对象 | 保存 resolved binding、保持 clone handle、单实例 queue、flush/cancel 语义 |
| `DelayedScheduledWork` | timer reservation + inner work | 保持 generation / instance 防 stale timer |
| `WorkQueue` | static logical queue | 保持 IRQ-safe enqueue 和 sleepable flush |
| `WorkQueueHandle` | dynamic logical queue owner | 保持 destroy gate 和 owner `Arc` 生命周期 |
| system queue helpers | 内建 task-context queue | 保持现有 runtime 名称和 per-CPU pool binding |
| BH queue helpers | 内建 bottom-half queue | 保持 softirq context、non-sleepable callback |

## 当前内部职责分层

| 层 | 类型 | 职责 |
|---|---|---|
| work state | `WorkState`, `WorkInstanceId`, `WorkerExecutionToken` | work 生命周期、stale instance 防护、running worker slot 校验 |
| queue object | `WorkQueue`, `QueueOwner`, `WorkQueueAttrs` | 逻辑 queue、owner 生命周期、policy attrs、queue wait source |
| pending storage | `WorkEntry`, `PendingWorkStore` | pool 中 pending entry 存储、runnable/inactive lane、pending barrier |
| queue-pool binding | `WorkQueueRuntime`, `WorkQueuePoolBinding`, `WorkQueuePoolState` | pool selection、`(queue, pool)` binding、active limit、flush color、in-flight accounting |
| pool execution | `WorkerPool`, `Worker` | shared pending store、worker slot、manager wake、sleep/tick accounting |
| runtime/provider | `runtime/*`, `provider.rs` | system/BH 实例和外部 scheduler/IRQ/timer 能力桥接 |

## 第一阶段边界调整

W1 的第一步是让 pending entry 显式携带 queued-instance 身份：

- `WorkEntry` 保存 work、queue owner record、binding key、flush color 和 `WorkInstanceId`；
- `WorkEntry` 继续作为 runnable/inactive lane 中的 entry，并保存 linked flush barrier；
- worker claim runnable entry 时同时校验 pool key、binding key 和 entry 携带的 `WorkInstanceId`。

该调整不改变 public API，也不改变 queue、cancel、flush、delayed work 或 BH 语义。它只是让 pool entry 的实例身份与 `WorkState` 中的 pending instance 显式对齐，减少旧 pending entry 被误认领的空间，同时避免为同一 entry 身份再引入一层无独立生命周期的私有包装。

## 不变量

- enqueue 成功后，`WorkState::Pending` 的 instance id 必须等于 pending `WorkEntry` 携带的 instance id；
- delayed timer fire 将 reserved instance 转入 pending entry 时必须复用原 instance id；
- worker take runnable entry 时必须复核 work state、pool key、binding key 和 instance id；
- stale entry 不能执行 callback，只能完成 linked barrier、修复 accounting 并发出 warning；
- pending entry 的 barrier storage 仍归 `WorkEntry`，不归 `WorkState`。

## 后续审计点

- `remove_work_for_key()` 后续可演进为 remove by `(work, binding, instance)`，但需要先确认 cancel/flush 调用点是否都持有 observed instance；
- `attach_barrier_to_work_for_key()` 后续可增加 instance 校验，避免 barrier 挂到同一 `ScheduledWork` 的后续实例；
- `PendingWorkStore` 需要继续封装具体 ring/Vec 操作，为 intrusive/list 替换留边界；
- `DeferredWake`、`QueueWake`、`WorkerWakePlan` 后续应收敛为更明确的 wake source / wake action 分层。
