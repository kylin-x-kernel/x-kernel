# kwork — 设计文档

## 定位

`kwork` 是 X-Kernel 的 deferred-work 子系统。它提供 Linux workqueue 风格的异步
work 执行、delayed work、flush/cancel 同步、queue-level flush、worker-pool 并发控制，
以及内建 task-context / bottom-half runtime queue。crate-root 公开接口仍然是
callback workqueue API，并为 NAPI-like 场景提供非 Future 的 `BudgetedPoller`。
`kwork::raw` 作为隐藏导入边界保留给 scheduler、IRQ 和迁移 glue。

依赖关系上，`kwork` 位于调度器和中断子系统之上，但不直接拥有 task 或 softirq：

- `ktask` 提供 worker/manager task、wait、timer、sleep/resume/tick accounting；
- `kirq` 提供 bottom-half softirq raise/drain 和 interrupt-like context 判定；
- 普通内核模块通过 `ScheduledWork`、`DelayedScheduledWork`、`WorkQueue` 和 system queue helper
  投递异步任务；
- 需要 Linux NAPI-like coalesced/budgeted 轮询语义的模块使用 `BudgetedPoller`；
- provider/glue crate 可以通过 `kwork::raw` 明确标识 scheduler/IRQ glue 依赖。

## 背景

内核中大量路径不能在当前上下文直接完成工作：中断路径不能睡眠，热路径不能执行耗时
任务，teardown 需要可靠等待异步 callback 结束。Linux 用 `workqueue_struct ->
pool_workqueue -> worker_pool -> worker` 解决这类问题：逻辑 queue 表示使用方式和策略，
pool 表示执行资源，pool_workqueue 表示 queue 到 pool 的绑定。

`kwork` 的 queue/pool 执行层按 Linux workqueue 分层组织，同时保持 Rust 实现的显式
状态和所有权边界。普通 work 的 public object model 以 `ScheduledWork` 为用户持有对象：
它保存 callback、disable gate、queued/running lifecycle、completion/barrier wake source，
cancel/flush 作用在该实例上。

- scheduled instance 状态用 `WorkState` 显式保存，而不是把状态完全编码进指针 flag；
- dynamic queue owner 持 `Arc` handle，防止 pending/running work 悬空；
- delayed timer 和 worker slot 使用 `WorkInstanceId` / `WorkerExecutionToken` 防 stale
  timer、tick、finish 操作；
- system/BH/long 等名字只作为 runtime 实例和 public helper 出现，不进入核心
  work/queue-pool/pool 数据模型。

## 范围

涉及的源文件：

```text
task/kwork/
├── src/
│   ├── lib.rs
│   ├── budgeted_poller.rs
│   ├── control.rs
│   ├── provider.rs
│   ├── work/
│   │   ├── item.rs
│   │   ├── delayed.rs
│   │   ├── state.rs
│   │   └── barrier.rs
│   ├── queue/
│   │   ├── workqueue.rs
│   │   ├── owner.rs
│   │   ├── sync.rs
│   │   ├── attrs.rs
│   │   ├── entry.rs
│   │   └── entry_queue.rs
│   ├── wq_pool/
│   │   ├── mod.rs
│   │   ├── accounting.rs
│   │   ├── binding.rs
│   │   ├── handle.rs
│   │   ├── ops.rs
│   │   └── outcome.rs
│   ├── pool/
│   │   ├── worker.rs
│   │   └── worker_pool.rs
│   └── runtime/
│       ├── system.rs
│       ├── bh.rs
│       └── mod.rs
└── docs/
    ├── design.md
    └── security.md
```

相关 provider 实现在：

```text
task/ktask/src/workqueue.rs
task/ktask/src/run_queue.rs
arch/kirq/src/bottom_half/workerqueue.rs
core/kruntime/src/lib.rs
```

## 架构

核心对象对照 Linux：

| X-Kernel 类型 | Linux 对位 | 职责 |
|---|---|---|
| `ScheduledWork` | `work_struct`-like work object | callback、disable gate、状态入口、completion/barrier wake source、cancel/flush handle |
| `BudgetedPoller` | NAPI-like poller | coalesced notify、missed-wake 防护、budget/max-round poll、assist |
| `DelayedScheduledWork` | `delayed_work` | timer-pending 状态和 inner work |
| `WorkQueue` | `workqueue_struct` | 逻辑 queue、destroy gate、per-CPU queue-pool state、flush/idle wait source |
| `QueueOwner` | work-data owner | 区分 static queue 和 dynamic queue handle |
| `WorkQueueRuntime` | pool selection policy | 由具体 workqueue 实现，为自身选择目标 worker pool |
| `WorkQueuePoolBinding` | `pool_workqueue` | 一个 `(queue, execution pool)` 绑定的操作视图 |
| `WorkQueuePoolState` | `pool_workqueue` 热状态 | `max_active`、active/running/in-flight/color 记账 |
| `WorkerPool` | `worker_pool` | execution attrs、shared pending store、runnable count、worker slot、manager state |
| `Worker` | `worker` | pool-local execution slot、current work、sleep/CPU-intensive 状态 |
| `WorkqueueTaskContext` | `current_wq_worker()` | task-local current work/pool/worker token |
| `BottomHalfPoolBinding` | `bh_worker_pools` | softirq 执行域中的 per-CPU pool binding |

Task-context 拓扑：

```text
每个 CPU：

  system_percpu_wq(cpu) ---- binding --+
  custom static queue ------- binding --+--> WorkerPool(Default, cpu)
  dynamic queue ------------- binding --+
  system_long_wq(cpu) ------- binding --+
```

Bottom-half 拓扑：

```text
每个 CPU：

  system_bh_wq(cpu) --------- binding ----> WorkerPool(BH Default, cpu)
  system_bh_highpri_wq(cpu) - binding ----> WorkerPool(BH HighPri, cpu)
                                         drained by kirq softirq action
```

`WorkerPoolAttrs` 描述执行资源属性：execution domain、scheduling policy 和 CPU
affinity。当前 task-context system pool 是 pinned per-CPU task-worker pool；
`ktask` provider 创建 worker/manager task 时把 cpumask 设置为 pool attrs 中的 CPU。
BH pool 是 pinned per-CPU bottom-half pool，执行域由 softirq drain 提供；它没有
provider-owned task worker，内部只保留一个 drain slot 复用通用 pending/claim accounting。

关键分层：

- `work/` 描述普通 work 模板、scheduled instance、delayed/barrier 状态，不知道 runtime queue 名字；
- `queue/` 只描述逻辑 queue、owner、entry storage 和 wait source，不拥有 worker；
- `wq_pool/` 保存 per-binding accounting 和入队/取消/完成操作；
- `pool/` 只管理 shared pending store 和 worker 并发，不调用 public queue API；
- `runtime/` 拥有内建 system/BH/long 实例；`WorkQueueRuntime` 由 `WorkQueue`/`WorkQueueHandle`
  实现，把自身解析到具体 pool；
- `provider.rs` 定义外部能力接口，避免 `kwork` 直接依赖 `ktask` 或 `kirq`。
- `budgeted_poller.rs` 暴露 `BudgetedPoller` 和 `BudgetedPollProgress`。该层封装
  `Idle/Scheduled/Running/RunningPending` 状态机，适合网络数据面这类 NAPI-like 场景：
  producer 只 publish work，worker 或 assist owner 按 budget 轮询，运行中到达的 notify
  转换为 follow-up round。内部仍使用两个 backing `ScheduledWork` 实例规避当前 running
  instance 不能直接 self-requeue 的限制，但不把该实现细节暴露给调用方。

## 调用约束 / 执行上下文

Preallocated immediate enqueue：

- 可从 hardirq、serving-softirq、BH-disabled 或普通 task context 调用；
- 对象实例必须已经由 task/init context 创建完成；
- 不执行 callback；
- 不创建 worker task；
- 不分配 `ScheduledWork`；
- 正常路径不要求 sleep。

Sleepable API：

- `flush`、`cancel_sync`、queue flush、dynamic destroy 只能在可睡眠 task context 调用；
- interrupt-like context 返回 `InvalidContext`；
- worker callback 等待自己或等待同 bounded pool 中无法推进的 target 返回
  `SelfWait`。

Delayed work：

- zero delay 等价 immediate queue；
- non-zero delayed schedule 要求 sleepable context，因为 provider timer registration 需要
  创建 cancel handle；
- timer fire 路径本身不执行 callback，只把 inner work 转入目标 queue/pool。

Bottom-half queue：

- callback 在 softirq context 中执行；
- callback 不能 sleep、不能阻塞、不能调用 wait 类 workqueue API；
- BH drain 有 restart/time budget，超预算后重新 raise softirq。

Budgeted poller：

- `BudgetedPoller::notify_irq_safe()` 可从 IRQ-adjacent producer 调用；它只 publish pending
  bit 并在需要时 queue backing work，不执行 poll callback；
- 同一个 poller 同时只能有一个 executor owner。后台 worker 和 `assist_once()` 使用同一
  ownership 状态，不能并发 poll；
- poll callback 返回 `BudgetedPollProgress { has_more }`。当 `has_more` 为 true 或运行中
  收到 notify 时，poller 保留 scheduled 状态并投递后续 round；
- backing work 投递因 pool 满或 worker 暂不可用失败时，idle publish 和 follow-up
  requeue 会恢复为可重新 notify 的 idle 状态；已经 scheduled 的状态会在后续 notify
  中重新尝试投递；
- `max_background_rounds` 限制一个 worker callback 内连续 poll 的轮数，避免数据面长时间
  占用 worker；
- `destroy()` 永久关闭 poller，拒绝后续 `notify_irq_safe()` 和 `start()`，并等待已经
  queued/running 的 poll callback 完成；
- `BudgetedPoller` 不拥有业务 wait source。网络 TX waiter、协议 timer、poll reason 等仍由
  `knet` 等调用方维护。

早期启动：

- queue 对象可 `const` 构造；
- 内建 system pool 只有 provider 安装后才能 drain；
- pool 未 ready 时 enqueue 返回 `WorkerUnavailable`，不提交 work 状态。

重入性：

- callback 期间不持 `kwork` 内部 spin lock；
- 每个 `ScheduledWork` 是独立实例；需要多个并行或独立生命周期的 work 时，调用方创建多个
  `ScheduledWork`，callback 自身的业务互斥由调用方维护；
- `ScheduleAttrs` 保存一次调度的投递目标：default system、long system、BH、
  BH high-priority 或 custom queue，以及可选 CPU 绑定；custom queue 可以来自
  `'static WorkQueue` 或 `WorkQueueHandle`，差异只表示 owner 生命周期；
  无 CPU 绑定表示按 enqueue 当时的 current CPU 选择 per-CPU pool binding；
- 同一个 `ScheduledWork` 实例不能同时 running 和 pending；running
  callback 重新 queue 自身返回 `AlreadyQueued`。

## 状态机

### `ScheduledWork`

`ScheduledWork` 直接由调用方创建并持有：

```text
ScheduledWork::new
  -> allocate ScheduledWork

ScheduledWork::schedule / ScheduledWork::schedule_with / WorkQueue::queue_work
  -> queue existing ScheduledWork

DelayedScheduledWork::new
  -> allocate DelayedScheduledWork

DelayedScheduledWork::schedule_after(delay) / schedule_after_with(delay, attrs)
  -> reserve timer state
  -> queue inner ScheduledWork when the timer expires
```

`ScheduledWork::schedule()` 是默认 system queue 的便利入口。
需要选择 long system、BH 或 custom workqueue 时，调用
`ScheduledWork::schedule_with()` 并传入 `ScheduleAttrs`，或用
`ScheduledWork::schedule_on_queue()` 直接投递到 custom queue。queue kind、CPU target 和
custom queue 都属于 schedule-time 选择。

`ScheduledWork::new()` 和 `DelayedScheduledWork::new()` 是分配型 API，只能用于允许内存分配的上下文。
IRQ、softirq、BH-disabled 等 producer 必须提前创建实例，然后调用
`ScheduledWork::schedule()` 或 `queue_work(&ScheduledWork)`；这些入队路径不分配。

每个 `ScheduledWork` 维护自己的状态机：

```text
Idle
 ├─ queue_work / zero-delay delayed ───────────────► Pending
 ├─ non-zero delayed schedule ─────────────────────► DelayedPending
 │                                                    │
 │                         timer fire / zero-delay mod│
 │                                                    ▼
 ◄──────── cancel / disable cleanup ◄────────────── Pending
                                                      │
                                                      │ pool take
                                                      ▼
                                                   Running
                                                      │
                                                      │ finish
                                                      ▼
                                                    Idle
```

允许转换：

| 从 | 到 | 触发 |
|---|---|---|
| `Idle` | `Pending` | immediate queue 或 zero-delay delayed queue |
| `Idle` | `DelayedPending` | non-zero delayed schedule |
| `DelayedPending` | `Pending` | timer fire、zero-delay `mod_delayed_work` |
| `DelayedPending` | `Idle` | cancel、disable cleanup、timer enqueue 不可恢复失败 |
| `Pending` | `Running` | pool worker/BH drain claim entry |
| `Pending` | `Idle` | pending cancel |
| `Running` | `Idle` | callback finish |

每个 queued/running/delayed 实例都有 `WorkInstanceId`。timer fire、cancel、flush、finish
都必须带 instance 校验，旧实例只能 no-op 或走 stale 修复。

### `Worker`

```text
Empty ── reserve ──► Creating ── install ──► Idle
  ▲                    │                     │
  │                    │ create failed       │ take work
  └────────────────────┘                     ▼
                                          Preparing
                                             │ wait loop
                                             ▼
                                           Running
                                             │ block
                                             ▼
                                          Sleeping
                                             │ resume
                                             ▼
                                           Running
                                             │ finish
                                             ▼
                                            Idle
```

`Running` execution 可被 tick/runtime accounting 标记为 CPU-intensive；该标志不是独立
worker state，而是 per-execution accounting bit，finish 时清除。

## 算法流程

### Enqueue

```text
queue_work(queue, work)
  -> workqueue runtime trait selects target CPU and execution pool binding
  -> lock queue.state -> work.gate -> pool.state -> queue-pool.state -> work.state
  -> reject destroying / disabled / non-idle / invalid target
  -> allocate WorkInstanceId
  -> sample queue-pool work_color
  -> append Runnable or Inactive WorkEntry to pool pending store
  -> publish owner, pool key, color, instance id to WorkState
  -> update active/runnable/in-flight counters
  -> collect wake plan
  -> return Queued(out-of-lock wake plan) or Rejected(reason)
  -> unlock
  -> execute provider wake outside locks
```

失败返回 typed result。`QueueFull`、`WorkerUnavailable`、`Disabled` 等失败不留下半提交
work state。`finish_workqueue_pool_enqueue()` 只消费成功 enqueue 收集到的 deferred wake：
唤醒选中的 worker / manager，并通知 `ScheduledWork` state-change waiters；失败 outcome
直接返回 rejection reason。

### Execute / finish

```text
pool worker or BH drain
  -> pop runnable candidate from shared pool store
  -> validate WorkState binding/pool/instance
  -> Pending -> Running
  -> record worker id and execution token
  -> run callback without internal locks
  -> Running -> Idle
  -> release queue-pool active and in-flight counters
  -> activate next inactive entry from same binding
  -> wake barriers, flush/idle waiters and selected worker outside locks
```

### Delayed work

```text
schedule(delay > 0)
  -> Idle -> DelayedPending
  -> register provider timer with generation

timer fire
  -> check generation / instance
  -> bind target queue/pool
  -> queue inner work as same instance

mod_delayed_work
  -> timer-pending: replace timer
  -> queued pending: remove pending entry and re-arm
  -> running: AlreadyQueued
```

### Work flush / queue flush

`flush_work()` 对 pending work 挂 barrier，对 running work 挂 running barrier。barrier 不占
普通 pending capacity、不占 active quota，但计入目标 color 的 in-flight。

`flush_workqueue()` 对目标 queue 的全部 per-CPU queue-pool binding 建立统一 color snapshot：先完成
deadlock 检查，再按 CPU 顺序推进 color。本轮只等待 snapshot color，后续 enqueue 使用
next color，不延长本轮 flush。

### Dynamic destroy

```text
destroy(handle)
  -> reject invalid/self wait context
  -> set queue.is_destroying
  -> wait all per-CPU queue-pool binding pending/running/in-flight become empty
  -> return; shared pool and workers keep running
```

调用方必须先停止 producer；destroy gate 只能拒绝 gate 之后的新 enqueue。

## 并发模型

主要锁顺序：

```text
enqueue:       queue.state -> pool.state -> queue-pool.state -> work.state
cancel:        pool.state -> queue-pool.state -> work.state
barrier attach:pool.state -> queue-pool.state -> work.state
take:          pool.state -> work.state; 成功后更新 queue-pool.state
finish:        work.state; 释放后 pool.state -> queue-pool.state
queue flush:   binding[0] -> binding[1] -> ... -> binding[N]
```

所有 callback、provider wake、provider wait、timer registration/cancel 都在 spin lock 外。
状态和计数在锁内提交，wake source 在锁内收集并在锁外触发。

关键守恒关系：

- runnable count 等于 pool pending store 中 `Runnable` entry 数；
- `WorkQueuePoolState` 的 active token 只归属于同一 binding；
- inactive 激活必须按 `binding_key` 过滤；
- in-flight color 计数包含普通 work 和 linked barrier；
- `nr_running` 只统计仍参与 bounded concurrency 的 running worker；
- Sleeping 和 CPU-intensive worker 不计入 `nr_running`。

`yield_now()` 不触发 worker sleep accounting。yield 是 Running -> Ready，worker 仍持有
current work 并继续计入 `nr_running`；长时间 CPU/yield-heavy callback 由 scheduler
runtime/tick CPU-intensive accounting 释放并发槽位。

## 设计决策

### runtime 名称不进入基础模型

`system_percpu_wq`、`system_long_wq`、`system_bh_wq` 表示内建 runtime 实例和使用方式。
核心 work/queue-pool/pool 层只接受中性的 execution binding，避免把 system/BH/long 变成底层
状态字段。

### runtime 负责 pool 选择

`WorkQueueRuntime` 是逻辑 queue 到 execution pool 的选择接口，由 `&'static WorkQueue`
和 `WorkQueueHandle` 实现。调用者不把 owner 传进 trait；`self` 就是待调度的逻辑 queue。
trait 返回的 `WorkQueuePoolBinding` 固化了 queue owner 和 execution pool，pending/running
`ScheduledWork` 状态保存这份 binding。后续 cancel、flush、finish 直接使用状态里的 binding，
不再按 owner/pool key 反向解析。

WorkerPool 的 pending `WorkEntry` 只记录对应的 workqueue owner、binding key、color 和
instance id。它不保存 resolved pool binding；worker claim 时以 `ScheduledWork` 状态里的
binding 为准。

### 显式 instance id

Linux 通过 `work_struct::data` 和 list/pool_workqueue 状态隐式表达实例身份。X-Kernel 的 Rust 对象
拆得更显式，timer、task-local context、worker slot 都可能晚到；因此使用
`WorkInstanceId` 和 `WorkerExecutionToken` 防止 stale 操作。

### bounded store 先保留，linked-list 后置优化

pending store 使用固定容量 ring/Vec，保证 IRQ-safe enqueue 不分配。Linux 使用 list；
最终性能阶段应评估 intrusive/linked list，以降低 wrap 后删除和按 binding 激活的移动成本。
该优化不改变 workqueue 语义。

### BH 只开放 builtin queue

`WorkQueueFlags::BH` 不对 custom/dynamic queue 生效。BH work 的不可睡眠约束非常强，
调用方通过 `system_bh_wq()` / `system_bh_highpri_wq()` 使用 BH queue，避免把 BH 执行
契约混入普通 queue attrs。

### 未实现能力显式拒绝

unbound、ordered、MEM_RECLAIM/rescuer、custom BH 等 Linux policy 不静默降级。没有真实
机制支撑的 flag 返回 unsupported，避免调用方误以为获得了 Linux 语义。

## Drop / 资源释放

`ScheduledWork` 和 `DelayedScheduledWork` 由 `Arc` 持有。pending entry、running callback、timer waker
都会持有 work handle，因此 work 对象不会在内部路径仍引用时释放。

dynamic queue 使用 `WorkQueueHandle`。pending/running/delayed owner 会持有 handle clone，
确保 queue 对象在 work 实例完成前存活。`destroy()` 只关闭 enqueue gate 并等待该 queue
相关 queue-pool binding drain；实际释放由最后一个 `Arc` drop 决定。

`kwork` 不拥有 callback 捕获对象。调用方释放 callback 依赖的外部资源前，必须停止
producer，并用 cancel/flush/destroy 等 API drain 已提交 work。

shared worker pool 和 provider worker task 不随 dynamic queue destroy 释放。worker idle
culling/self-exit 尚未实现，因此 pool 生命周期跟随内建 runtime/provider 生命周期。
