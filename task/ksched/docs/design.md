# ksched — 设计文档

## 定位

`ksched` 提供可插拔的调度算法实现（FIFO / RR / CFS / EEVDF）。`ktask` 通过
`BaseScheduler` 驱动算法，不嵌入具体公平性策略。

## 背景

运行任务离开当前 RQ 的原因不止 yield：还包括 preempt、block、migrate、exit。
若这些路径各自清理调度状态，公平调度器的 lag / `curr` 记账会漂移，并可能把
运行任务的强引用困在算法内部。本 crate 用统一状态转换收口该契约。

## 范围

```
task/ksched/
├── src/
│   ├── lib.rs          # BaseScheduler / CurrentDisposition
│   ├── fifo.rs
│   ├── round_robin.rs
│   ├── cfs.rs
│   ├── eevdf.rs
│   └── tests.rs
└── docs/
```

## 架构

```
Ready --pick_next_task--> Running
Running --leave_current(Yield|Preempt)--> Ready
Running --leave_current(Block)--> Blocked (off RQ)
Running --leave_current(Migrate)--> Migrating (off source RQ)
Running --leave_current(Exit)--> Exited (off RQ)
Blocked|Migrating --enqueue_task--> Ready
```

| 组件 | 职责 |
|------|------|
| `CurrentDisposition` | 当前任务离开原因的封闭枚举 |
| `BaseScheduler::leave_current` | 运行任务离开执行槽的唯一转换 |
| `BaseScheduler::enqueue_task` | 非当前 Ready 任务入队（唤醒 / 迁入） |
| `EevdfScheduler::curr` | 非 owning 的运行实体数值快照 |

## 调用约束 / 执行上下文

- 所有 `BaseScheduler` 方法由 `ktask` 在持有当前 CPU run queue 锁、IRQ/preempt
  已按路径禁用的上下文中调用。
- `leave_current` 必须在 `pick_next_task` 之前完成；EEVDF 对残留 `curr` 直接断言失败。
- 调度器不得成为任务生命周期 owner：EEVDF `curr` 只缓存调度数值。
- 修改运行实体的 `vruntime` / `deadline` / `weight` 后，必须同步刷新 `curr` 快照
  （生产路径由 `task_tick` / `set_priority` 负责）。
- 未经 `pick_next_task` 上 CPU 的运行任务在 `switch_to_local` 入场时
  `sync_running_curr`，否则空 `curr` 会把任意非空 ready 队列当成可抢占。
  idle 从不 sync，空 `curr` 对它反而是正确语义。

## 算法流程

1. `add_task`：新任务按算法初始放置进入 ready 队列。
2. `pick_next_task`：选出下一个运行任务；EEVDF 同时写入 `curr` 快照。
3. `task_tick`：推进运行任务记账；EEVDF 同步刷新快照。
4. 抢占探测：`ktask` 调用 `peer_preempts_curr`（不在探测里 sync）；仅 peer
   胜出时才 `leave_current(Preempt)` + pick。off-tree 助手在 `switch_to_local`
   入场时 `sync_running_curr`；idle 保持 `curr` 为空。
5. `leave_current`：
   - Yield：重置 request/slice 并再入队；
   - Preempt：保留剩余 request 并再入队；
   - Block/Migrate：公平调度器保存 lag 并标记 PLACE_LAG，不入队；
   - Exit：清除 current 记账，不设置 PLACE_LAG。
6. `enqueue_task`：消费 PLACE_LAG（若有）后入队；EEVDF 可提名 NEXT_BUDDY。

## 设计决策

- 用 `CurrentDisposition` 替代布尔 `preempt` + 散落的 `account_sleep`，避免调用方
  记住隐藏不变量。
- EEVDF `curr` 使用值快照而非 `Arc`/`Weak`：O(1) 统计、无原子 upgrade、不参与
  生命周期。
- 不再维护未接线的 metadata 分离调度器；生产与单测统一走 entity 内嵌实现。

## Drop / 资源释放

调度器 ready 队列持有 Ready 任务的 `Arc`。运行任务的 owner 在 `ktask` 侧
（current-task 指针、wait/waker、migration helper、exited list）。算法内部的
`curr` 快照在 `leave_current` 时清除，不延长任务存活。
