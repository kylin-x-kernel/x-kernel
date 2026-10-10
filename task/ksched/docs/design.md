# ksched — 设计文档

## 定位

`ksched` 的生产调度器是类仲裁器。`ktask` 在持有运行队列锁时调用
`Scheduler`。仲裁器按 Linux 秩遍历 Stop、Deadline、RT、Fair、Idle：这一层
实现 RT（FIFO/RR）、Fair（EEVDF）和 Idle。Stop 与 Deadline 仍只占秩，
`pick_next` 返回空、`enqueue` 视为编程错误。用户任务仍由
`SchedTask::new` 建成 Fair：`sched_setscheduler` 只写线程快照，还不调用
`Scheduler::set_attr`，所以生产路径上 `sched_class() == Rt` 目前不成立。
`ktask` 选核、实体锁和 idle 定时器都不在这次改动里。

## 背景

运行任务离开当前 RQ 的原因不止 yield：还包括 preempt、block、migrate、exit。
若这些路径各自清理调度状态，公平调度器的 lag / `curr` 记账会漂移，并可能把
运行任务的强引用困在算法内部。本 crate 用统一状态转换收口该契约。

## 范围

```
task/ksched/
├── src/
│   ├── lib.rs          # Scheduler / SchedTask
│   ├── config.rs       # Fair request、RR 时间片、RT 带宽
│   ├── policy.rs       # SchedClass 秩与用户策略
│   ├── entity.rs       # SchedTask、FairEntity、RtEntity、RQ 令牌
│   ├── ready_list.rs   # RT 用的 intrusive FIFO 与 O(1) 摘除
│   ├── class.rs        # ClassOps
│   ├── classes/        # rt.rs、fair.rs（EEVDF）、idle.rs
│   ├── scheduler.rs    # 五档仲裁；Stop/Deadline 未接线
│   ├── clock.rs
│   └── tests.rs        # Fair / RT / Idle 仲裁器单测
└── docs/
```

## 架构

```
Ready --pick_next_task--> Running
Ready --steal_ready_task--> Migrating (off source RQ; PLACE_LAG armed)
Running --leave_current(Yield|Preempt)--> Ready
Running --leave_current(Block)--> Blocked (off RQ)
Running --leave_current(Migrate)--> Migrating (off source RQ)
Running --leave_current(Exit)--> Exited (off RQ)
Blocked|Migrating --enqueue_task--> Ready
```

| 组件 | 职责 |
|------|------|
| `LeaveReason` | 当前任务离开原因的封闭枚举 |
| `Scheduler::leave` / yield / preempt / block / migrate / exit | 运行任务离开执行槽的唯一转换 |
| `Scheduler::enqueue` | 非当前 Ready 任务入队（唤醒 / 迁入） |
| `Scheduler::steal` | 摘一颗匹配谓词的 ready waiter（idle-pull）；不 `pick_next`、不安装 `curr` |
| `Scheduler::curr` | 运行任务的 `Arc`，`leave` 时交还 |

## 类仲裁器

`ktask` 在持有运行队列锁时调用 `Scheduler`。

```
pick_next: Stop -> Deadline -> RT -> Fair -> Idle
           未接线返回 None         FIFO/RR   EEVDF    永远返回钉住的 idle
```

| 组件 | 职责 |
|------|------|
| `SchedTask` | 调用方内部数据、class、seat、attr，以及每任务一份 `FairEntity` 和 `RtEntity` |
| `RtClass` | 优先级 `1..=99` 的侵入式 FIFO；RR 时间片；RT 带宽节流 |
| `FairClass` | 双侵入式 RB 树、lag、NEXT_BUDDY、request |
| `IdleClass` | 每 RQ 一个钉住的 idle；不入队 |
| `Scheduler::curr` | 运行任务的 `Arc`，`leave` 时交还 |

公开方法是安全的。调用方必须已持有该 RQ 锁；本 crate 不检查锁本身，
也不采样时钟、不加锁、不切栈。`now_ns` 由调用方传入。

Fair 的放置与插件 EEVDF 对齐：`vruntime = V - lag`，离树的 runner 计入
`min_vruntime` 和平均 **V**，Block/Migrate 记 lag，Exit 不记，yield/preempt
再入队。`migrate_current` 是迁出入口；目的 RQ 用 `EnqueueKind::MigrateIn`
入队。Idle 的 `has_runnable` 恒为真，所以 `pick_next` 在 Fair 为空时落到
idle，不会返回空。

Stop / Deadline 只占秩。`SchedAttr::validate` 拒绝 Deadline。FIFO/RR
要求优先级 `1..=99`，Fair 策略必须带 `RtPrio::NONE`。Idle 任务拒绝
`set_attr`（`KernelClassImmutable`）。

RT 与 Fair 同级切换时，排队任务先从旧类摘下再进新类；正在运行的任务只改
class，不入队。更高优先级的 RT waiter 通过 `higher_class_runnable` 抢占
Fair。同一 RT 优先级里，更高优先级 waiter 走 `wakeup_preempts`。自愿
`yield`（POSIX `sched_yield`，Linux `yield_task_rt`）把 FIFO 和 RR 都放到
同优先级队尾。被抢占的任务插回队头；只有 `SCHED_RR` 时间片耗尽
（`rr_rotate`）时，被抢占者才改插队尾。
一个 RT 周期内 `used_ns` 达到 `SCHED_RT_RUNTIME_US` 后整类节流，`pick_next`
跳过 RT，让 Fair 运行，直到周期边界 `advance` 清零。

`rt_priority_snapshot` 是 Release/Acquire 的锁外优先级。这次没有选核，
也没有把 runner 优先级写成每 CPU 原子。

## 调用约束 / 执行上下文

- 所有 `BaseScheduler` 方法由 `ktask` 在持有**目标** run queue 的 scheduler 锁、
  IRQ/preempt 已按路径禁用的上下文中调用。idle-pull 的 `steal_ready_task` 持的是
  **源** RQ 锁，且不得同时持有 dest 调度锁。
- `leave_current` 必须在 `pick_next_task` 之前完成；EEVDF 对残留 `curr` 直接断言失败。
- 插件 EEVDF 不得成为任务生命周期 owner：`EevdfScheduler::curr` 只缓存调度数值。
  `Scheduler::curr` 持有运行 `Arc`，在 leave 时释放，见「类仲裁器」。
- 修改运行实体的 `vruntime` / `deadline` / `weight` 后，必须同步刷新 `curr` 快照
  （生产路径由 `update_current` / `set_priority` 负责）。
- 未经 `pick_next_task` 上 CPU 的运行任务在 `switch_to_local` 入场时
  `sync_running_curr`，否则空 `curr` 会把任意非空 ready 队列当成可抢占。
  idle 从不 sync，空 `curr` 对它反而是正确语义。

## 算法流程

1. `add_task`：新任务按算法初始放置进入 ready 队列。
2. `pick_next_task`：选出下一个运行任务；EEVDF 同时写入 `curr` 快照。
3. `steal_ready_task`：按谓词摘一颗 ready waiter，走与 `remove_task` 相同的
   lag 快照，不安装 `curr`。FIFO/RR 在首个匹配处停止，剩余侵入式链表原地保留
   （O(匹配下标)）；CFS/EEVDF 为 find 后 `remove_task`。ktask idle-pull 用它
   拒绝仍 `on_cpu` 的任务。
4. `update_current(elapsed_ns)`：推进运行任务记账；EEVDF 同步刷新快照。
5. `next_preemption_ns`：返回距下次必须重评估抢占的相对纳秒，lone task 为 `None`。
6. 抢占探测分两步，对齐 Linux `check_preempt_tick` 与 `__schedule`：
   仅 schedule tick（`account_sched_tick`）调用不消费标记的 `check_preempt_tick`；
   IRQ 尾 `peer_preempts_curr` 才消费 WF_SYNC 并决定是否 `leave_current(Preempt)`
   + pick。`update_current` 只在本 request 用完时要求 resched，不因同伴更早
   deadline 打标（否则 WF_SYNC 换上 later-deadline wakee 会被立刻抢回）。
   off-tree 助手在 `switch_to_local` 入场时 `sync_running_curr`；idle 保持
   `curr` 为空。
7. `leave_current`：
   - Yield：重置 request/slice 并再入队；
   - Preempt：保留剩余 request 并再入队；
   - Block/Migrate：公平调度器保存 lag 并标记 PLACE_LAG，不入队；
   - Exit：清除 current 记账，不设置 PLACE_LAG。
8. `enqueue_task`：消费 PLACE_LAG（若有）后入队；EEVDF 提名 NEXT_BUDDY
   （Linux `set_next_buddy`）。`curr` 为空时也要提名：远端 WF_SYNC 常落在
   leave→pick 窗口，否则随后 pick 会留下更早 deadline 的 runner。
9. EEVDF `min_vruntime` 按 Linux `update_min_vruntime` 更新：离树但仍
   runnable 的 `curr` 参与水位（与 ready 最小 vruntime 取 min 后再单调抬升）。
   `leave_current` 必须在清 `curr` **之前**更新（Linux `put_prev` /
   `update_curr`）；清掉后再入队时不要按 ready-only 树更新水位。
   `place_entity` 是 `vruntime = V - lag`，不把 wakee 额外钳到 V 或 `min_vruntime`。
10. `update_current` 对齐 Linux `update_deadline`：request 完成后赋新
   `vd = ve + r/w`，仅在 ready 队列非空时要求 resched。

## 设计决策

- 生产路径走 `Scheduler`。Fair 默认 request 来自 Kconfig
  `SCHED_FAIR_REQUEST_US`。RR 时间片是 `SCHED_RR_SLICE_US`（默认 100 ms）。
  RT 带宽是 `SCHED_RT_RUNTIME_US` / `SCHED_RT_PERIOD_US`（默认 0.95 s / 1 s）。
  Stop / Deadline 仍只占秩。
- `Scheduler::curr` 持有运行任务的 `Arc`，和插件 EEVDF 的数值快照不是同一套
  所有权。`leave` 释放仲裁器这份引用；Block 路径仍要求调用方另持一份。
- 用 `LeaveReason` 替代布尔 `preempt` + 散落的 `account_sleep`，避免调用方
  记住隐藏不变量。
- 不再维护未接线的 metadata 分离调度器；生产与单测统一走 entity 内嵌实现。
- EEVDF ready 索引使用 intrusive-collections 0.10.2 的两棵 RBTree，分别按
  `(deadline,id)` 和 `(vruntime,id)` 排序，直接索引同一实体，不再维护 id 反查表。
  每实体预置两个 RBTreeAtomicLink；入队、摘除和查询不申请索引内存。RT
  的 `1..=99` 队列复用 `ready_list`：`intrusive-collections` 的
  `LinkedListAtomicLink`，按位图找最高优先级，按队列身份 O(1) 摘除。
  Fair 不使用这条链表。
  依赖启用 nightly 特性以保留 scheduler 的 const 构造，
  由仓库现有 RUSTC_BOOTSTRAP 构建环境支持。
- 每棵树缓存最早实体，常见最小值查询 O(1)；插入比较更新缓存，摘除最小值时
  用游标后继更新，增删仍为 O(log N)。库的 front 本身不是 O(1)。
- eligible pick 在 deadline 首节点有资格时直接返回，否则遍历 vruntime 的
  有资格前缀，O(log N + E)，不收集临时 Vec。保留 id 次序、lag、权重记账、
  curr、buddy/WF_SYNC、抢占和时间片算法。steal 保留 deadline 顺序和既有谓词签名。
- EEVDF 虚拟时间（`vruntime` / `deadline` / `vlag`）用固定宽度 `i64`，墙钟
  （`slice_ns` / `request_ns` / `elapsed_ns`）用 `u64`。两边只经
  `vruntime_delta` / `vruntime_to_wall_ns` 转换；`Σ w·v` 乘除用 `i128`。
  不用 `isize`（32 位上 vruntime 会溢出）。nice 仍走 `BaseScheduler::set_priority`
  的 `isize`。

## Drop / 资源释放

`FairClass` 的两棵树和最早缓存、`next_buddy` 持有排队任务的 `Arc`。
`Scheduler::curr` 持有运行任务的 `Arc`，leave 后不再持有。Idle 槽钉住每 RQ
的 idle 任务，随 `Scheduler` 一起释放。树 Drop 走库的 `clear()`，不调用
`get_key`，因此不在无锁路径上读 `RqCell`。

插件调度器 ready 队列持有 Ready 任务的 `Arc`。运行任务的 owner 在 `ktask` 侧
（current-task 指针、wait/waker、migration helper、exited list）。算法内部的
`curr` 快照在 `leave_current` 时清除，不延长任务存活。

EEVDF 两棵树各持一个已有 Arc，两个最早缓存按需各持一份；摘除两索引和更新
缓存期间保留返还引用，释放附加引用不会触发最终实体析构。非空 scheduler Drop
由树正常清理链接和持有，不能使用 fast_clear。迁移前两链接都必须摘除；排队期间
deadline、vruntime、id 不变，set_priority 先摘树再改键。

索引受既有 scheduler 锁串行保护，AtomicLink 不意味着无锁树。按实体删除先查键
再核对地址；同键的另一实体不被删除。错误 scheduler 的 set_priority 对仍挂树的
外来实体返回 false，避免修改别的树的排序字段。

库的 intrusive_adapter! 宏不能表达 EevdfEntity 的 const 泛型，因此私有
EntityAdapter 用同一模板的两种编译期字段投影实现 Adapter；不新增运行期模式。
字段偏移使用 core::mem::offset_of!，Arc 转换由 DefaultPointerOps 完成，缓存引用
由安全 cursor.clone_pointer 取得，不手写引用计数恢复。

EEVDF steal 逐节点检查时，以游标 get_ptr 构造 ManuallyDrop<Arc<_>>
临时借用视图，避免未命中节点的原子引用计数操作。谓词只接收 &Arc，
命中后才 clone 一份真实强引用，结束游标借用，再走既有 remove_task。
扫描期间树始终持有原有强引用；正常返回和 panic 都不释放借来的引用。

## RT 队列

`RtClass` 为优先级 `1..=99` 各放一条 `ready_list::ReadyList`。`occupied`
位图标出非空级，最高级是 `leading_zeros`。同优先级的入队顺序就是 FIFO。
自愿 yield 把当前任务放到该优先级队尾，FIFO 与 RR 相同。被抢占的任务插回
队头；`SCHED_RR` 时间片耗尽后的抢占才插回队尾。

任务还没挂在别的链表上时，才把归属字设成这条 `ReadyList` 的地址，然后入链；
已经挂着的不改归属、不入队。摘除前清零。另一颗 CPU
或另一个优先级的链表地址不同，核对失败就返回 `None`，不会按指针去摘。不能
只凭 `is_linked` 判断属于本队列。队列对象在运行队列公布之后不再移动，否则
地址对不上。`Drop` 逐个 `pop_front`，清归属并释放链表持有的 `Arc`。

节流是每 RQ 的：`account` 累加 runner 的墙钟，达到 runtime 后 `has_runnable`
为假，`pick_next` 跳过本队列的 RT。`class_timer_ns` 只在仍节流且队列非空时
指向本周期结束。队列被摘空后不再武装周期定时器；新的 waiter 由入队返回的
`next_event_ns` 重新编程。
`steal` 不看节流：idle-pull 可以把这些 waiter 摘到另一颗仍有 runtime 的
CPU，占用记在目标 RQ 上。周期推进在 `advance` 里用绝对 `RqClock`，不另采样时钟。
