# ksched — 设计文档

## 定位

`ksched` 的生产调度器是类仲裁器。`ktask` 在持有运行队列锁时调用
`Scheduler`。仲裁器按 Linux 秩遍历 Stop、Deadline、RT、Fair、Idle：这一层
实现 Deadline（CBS + EDF）、RT（FIFO/RR）、Fair（EEVDF）和 Idle。Stop 仍只占
秩，`pick_next` 返回空、`enqueue` 视为编程错误。用户任务仍由 `SchedTask::new`
建成 Fair：`sched_setscheduler` / `sched_setattr` 只写线程快照，还不调用
`Scheduler::set_attr`，所以生产路径上 `sched_class()` 目前仍是 Fair。
`ktask` 在任务已经是 RT 时按 `find_lowest_rq` 选核，并在 idle 时保留节流定时器。

## 背景

运行任务离开当前 RQ 的原因不止 yield：还包括 preempt、block、migrate、exit。
若这些路径各自清理调度状态，公平调度器的 lag / `curr` 记账会漂移，并可能把
运行任务的强引用困在算法内部。本 crate 用统一状态转换收口该契约。

## 范围

```
task/ksched/
├── src/
│   ├── lib.rs          # Scheduler / SchedTask
│   ├── config.rs       # Fair request、RR 时间片、RT 带宽、DL 预算与准入余量
│   ├── policy.rs       # SchedClass 秩、用户策略、RtPrio、DlParams
│   ├── entity.rs       # SchedTask、FairEntity、RtEntity、DeadlineEntity、RQ 令牌
│   ├── ready_list.rs   # RT 用的 intrusive FIFO 与 O(1) 摘除
│   ├── class.rs        # ClassOps
│   ├── classes/        # deadline.rs（CBS+EDF）、rt.rs、fair.rs（EEVDF）、idle.rs
│   ├── scheduler.rs    # 五档仲裁；Stop 未接线
│   └── clock.rs
├── ut-tests/           # Deadline / RT / Fair / Idle 仲裁器单测
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
           未接线   CBS+EDF    FIFO/RR   EEVDF    永远返回钉住的 idle
```

| 组件 | 职责 |
|------|------|
| `SchedTask` | 调用方内部数据、class、seat、attr，以及每任务一份 `FairEntity`、`RtEntity` 和 `DeadlineEntity` |
| `DeadlineClass` | 就绪 / 节流两棵侵入式 RB 树；CBS 预算；EDF 选人；每 RQ 带宽准入 |
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

排队 Fair 任务修改 nice 时，`Scheduler::set_attr` 已写入新权重。
`attr_changed` 必须按旧 nice 对应的入队权重扣除 `total_weight` / `sum_wv`，
再按新权重重新入队；整个过程在同一 RQ 锁内完成。运行任务离树，不走这条扣账路径。

Stop 只占秩。`SchedAttr::validate` 按 `policy` 校验各档字段是否自洽：
Deadline 必须携带合法 `DlParams`，FIFO/RR 要求优先级 `1..=99`，Fair 策略
必须带 `RtPrio::NONE`；任何非 Deadline 策略都不得携带 `dl_params`。
Idle 任务拒绝 `set_attr`（`KernelClassImmutable`）。

### Deadline 类（CBS + EDF）

每个任务按 `DlParams { runtime_ns, deadline_ns, period_ns }` 预留带宽。
就绪态按**绝对截止期**排序（`deadline_ns`），因此最早截止期先运行；
`account` 扣减 `budget_left_ns`，归零即进入节流态并按 `period_start_ns`
转入节流树，等下一个周期由 `advance` 补满预算后回到就绪树。

- 两棵树的键都是 `(instant_ns, tree_id)`：`tree_id` 由类在每次入队时分配，
  仅用于同刻 tie-break，保证 FIFO 而不引入 id 反查表。
- `enqueue` 的语义按 `EnqueueKind` 区分：`New` 开新请求；`ClassChange` /
  `MigrateIn` 由 `switched_to` 或目的 RQ 负责，不重复补预算；`Wake` 在预算
  仍有剩余时续用当前请求，否则保持节流直到周期开始。
- **准入是每 RQ 的**：`can_admit` 累加**就绪树 + 节流树 + runner**各自的
  `runtime/period`。runner 被 `pick_next` 摘出两棵树，所以 class 用私有字段
  `running`（记 `tree_id` 与预留 ppm）自己记住它：在 `pick_next` / `set_curr`
  / `switched_to(is_curr)` 置位，在 `put_prev` / `switched_from(is_curr)` 清除，
  同类内改参数时由 `attr_changed(is_curr)` 刷新。用本次请求替换被改任务在这份
  和中已计入的旧预留，结果不得超过 `1 - SCHED_DL_BW_MARGIN_PCT`。Block /
  Migrate 之后既不在树上、也不是 runner 的任务不计入。超限返回
  `SchedError::AdmissionFull`，上层映射 `EBUSY`；参数非法仍复用
  `InvalidPolicy`（`EINVAL`）。
- `set_attr` 入口已调用 `validate()`，因此校验发生在任何状态变更之前；
  准入在 `set_attr` 的 `can_admit` 阶段完成，失败不改动任务属性。
- Deadline 不参与 steal：带宽按 home RQ 记账，搬走会脱离准入它的预留。
- `curr_timer_ns` 报告"预算耗尽"与"请求截止期"的较早者，节流时报告
  周期开始；`class_timer_ns` 取两棵树最早键。事件以相对纳秒上报，由
  `Decision::next_event_ns` 交给 `ktask` 编程既有调度槽，**不新增专用定时器**
  （下限与 Fair 共用 `MIN_SCHED_TIMER_NS`，且低于任何合法 `runtime_ns`）。

已知取舍：`can_admit` 会遍历两棵驻留树、`nr_queued` 会遍历就绪树（O(n)），
本步优先正确性；若 deadline 任务规模上来，可用增量求和或缓存最早实体替换。

RT 与 Fair 同级切换时，排队任务先从旧类摘下再进新类；正在运行的任务只改
class，不入队。更高优先级的 RT waiter 通过 `higher_class_runnable` 抢占
Fair。同一 RT 优先级里，更高优先级 waiter 走 `wakeup_preempts`。自愿
`yield`（POSIX `sched_yield`，Linux `yield_task_rt`）把 FIFO 和 RR 都放到
同优先级队尾。被抢占的任务插回队头；只有 `SCHED_RR` 时间片耗尽
（`rr_rotate`）时，被抢占者才改插队尾。
一个 RT 周期内 `used_ns` 达到 `SCHED_RT_RUNTIME_US` 后整类节流，`pick_next`
跳过 RT，让 Fair 运行，直到周期边界 `advance` 清零。

`rt_priority_snapshot` 是 Release/Acquire 的锁外优先级，供 `ktask` 选核。
`Scheduler::rt_preempt_rank` 在 RQ 锁下读这颗 CPU：Fair/Idle 且 RT 类还能被
选出时为 0，正在运行的 RT 为其优先级，Stop/Deadline 为 255。带宽节流在
本 RQ 时钟尚未越过 `period_start_ns + rt_period_ns` 时也记 255（`is_throttled_at`），
因为这段时间里 `pick_next` 不会再选出 RT；选核跳过这颗 CPU，只在没有其他
可抢占 CPU 时退回 hint。队列被摘空后周期边界定时器仍保持武装，到点会清标志
并改写这份快照。这个字节只描述 runner。已经排在 RT
队列里、还没上 CPU 的更高优先级 waiter 不会抬高它，所以中等优先级的 RT
任务可能被放到这颗 CPU 上，排在那个 waiter 后面。这一步不另存“最高排队
RT 优先级”。`ktask` 在会改变这个字节的操作（记账/`charge`、替换 `curr`、
`set_attr`）放下 RQ 锁之前把它写成每 CPU 原子，选核时不拿远端锁。只读的
`next_event_ns` 不重发这份快照。

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
  Deadline 的最小预算与准入余量是 `SCHED_DL_MIN_RUNTIME_US`（默认 100 µs）与
  `SCHED_DL_BW_MARGIN_PCT`（默认 5）。Stop 仍只占秩。
- **Deadline 用两棵独立树，而不是就绪与节流共用一棵/一个链接**。曾考虑的
  备选是单树单链接（省一个 `RBTreeAtomicLink`），被否决，理由有两条：
  1. **`nr_queued` 的语义会被污染**。`ktask` 的 `load_count` 用
     `Scheduler::nr_queued() + 非 idle runner` 得到 `nr_running`，选核
     （`find_idlest_cpu` / SIS / idle-pull）直接读它。共用一棵树时节流任务
     也在树里，`nr_queued` 会把"本周期不能再跑"的任务算成可运行，选核就会
     把新任务放到一个实际没有可运行任务的 CPU 上。分开后
     `nr_queued == ready.len()`，语义与 Fair 一致。
  2. **两态的排序键不同**。就绪态按请求的绝对截止期（`deadline_ns`）排序，
     节流态按下一个周期的起点（`period_start_ns`）排序——后者才是唤醒时刻。
     共用一棵树就必须在同一棵树里混排两种键，并让 `pick_next` 逐项跳过
     节流项；那会把选人路径从"取最小键"退化为 O(树中节流项数)，也会让
     `class_timer_ns` 的最早事件不再等于树的 `front()`。
  代价是每个实体多一个链接（`ready_link` / `throttled_link`），构造时静态
  初始化，运行期不分配。
- `SchedAttr` 保持扁平 `Copy` 结构，各档字段并存、由 `policy` 决定谁有意
  义，`validate()` 是唯一闸门；新增一档只需加字段与分支，不改变既有字段布局。
  非本类参数用哨兵值（`RtPrio::NONE` / `Option::None`）表示。
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
为假，`pick_next` 跳过本队列的 RT。标志只在 `advance` 里清掉。
`class_timer_ns` 在标志仍为真时指向本周期结束，队列已经被 block、migrate、
exit 或 steal 摘空也一样：空闲 CPU 没有 tick，卸掉这个事件就会让节流标志和
已发布的 255 停过周期边界。定时器到点后 `advance` 清标志并重新发布 rank，
之后不再武装。选核读的 255 按 `is_throttled_at(now)` 判定，`now` 已经到达
`period_start_ns + rt_period_ns` 就不再当成节流。
`steal` 不看节流：idle-pull 可以把这些 waiter 摘到另一颗仍有 runtime 的
CPU，占用记在目标 RQ 上。周期推进在 `advance` 里用绝对 `RqClock`，不另采样时钟。

实际执行时间由 `SchedTask::sum_exec_runtime_ns` 保存。
统一 `Scheduler::charge` 消费 `RqClock::advance` 的增量，非 Idle 任务在类算法
处理前累计一次，不受 nice 权重影响。阻塞/迁移间隙不累计；读取只返回已提交值。
