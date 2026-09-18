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
│   ├── ready_list.rs   # FIFO/RR shared intrusive queue
│   ├── cfs.rs
│   ├── eevdf.rs
│   └── tests.rs
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
| `CurrentDisposition` | 当前任务离开原因的封闭枚举 |
| `BaseScheduler::leave_current` | 运行任务离开执行槽的唯一转换 |
| `BaseScheduler::enqueue_task` | 非当前 Ready 任务入队（唤醒 / 迁入） |
| `BaseScheduler::steal_ready_task` | 摘一颗匹配谓词的 ready waiter（idle-pull）；不 `pick_next`、不安装 `curr` |
| `EevdfScheduler::curr` | 非 owning 的运行实体数值快照 |

## 调用约束 / 执行上下文

- 所有 `BaseScheduler` 方法由 `ktask` 在持有**目标** run queue 的 scheduler 锁、
  IRQ/preempt 已按路径禁用的上下文中调用。idle-pull 的 `steal_ready_task` 持的是
  **源** RQ 锁，且不得同时持有 dest 调度锁。
- `leave_current` 必须在 `pick_next_task` 之前完成；EEVDF 对残留 `curr` 直接断言失败。
- 调度器不得成为任务生命周期 owner：EEVDF `curr` 只缓存调度数值。
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

- 用 `CurrentDisposition` 替代布尔 `preempt` + 散落的 `account_sleep`，避免调用方
  记住隐藏不变量。
- EEVDF `curr` 使用值快照而非 `Arc`/`Weak`：O(1) 统计、无原子 upgrade、不参与
  生命周期。
- 不再维护未接线的 metadata 分离调度器；生产与单测统一走 entity 内嵌实现。
- EEVDF ready 索引使用 intrusive-collections 0.10.2 的两棵 RBTree，分别按
  `(deadline,id)` 和 `(vruntime,id)` 排序，直接索引同一实体，不再维护 id 反查表。
  每实体预置两个 RBTreeAtomicLink；入队、摘除和查询不申请索引内存。FIFO/RR
  统一使用同版本的 LinkedList，ksched 不再依赖 linked_list_r4l。
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

调度器 ready 队列持有 Ready 任务的 `Arc`。运行任务的 owner 在 `ktask` 侧
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

## FIFO/RR 侵入式队列

ReadyList 只收口 FIFO/RR 共有的队列操作，不持有调度策略。FifoTask 保留
new/inner/into_inner/Deref，RRTask 保留 new/inner/Deref 和原时间片逻辑。
移除旧容器专用的 GetLinks 实现，不向调用方暴露新链接。

每个实体内嵌一个 LinkedListAtomicLink 和一个队列归属字。队列首次入队时取得
全局唯一、永不复用的 usize 标识，构造仍是 const 且不分配内存；编号耗尽会在
修改链表前明确 panic，不回绕。队列移动不改变标识，按任务摘除保持 O(1)。
编号只在队列首次使用时访问全局原子计数，不在每次入队时竞争该计数。

库的 AtomicLink 已负责拒绝重复入链。入链成功后才写入 owner，拒绝重复入队
不会覆盖原归属；摘除时在释放 AtomicLink 之前清除 owner，避免覆盖另一个队列
刚写入的新归属。owner 是原子身份标记而非发布锁，采用 Relaxed；实际链表访问
由队列独占借用和外部 RQ 锁保护，链接跨队列占用由库的 acquire/release 维护。

steal 使用游标逐个检查，命中只摘目标节点，未命中不改动链表。保留 Arc 谓词
签名，以游标的原始数据指针构造 ManuallyDrop<Arc<T>> 临时视图，不增减引用计数。
视图只以 &Arc 传给谓词，不能被移出；其存活期间游标独占借用列表，列表持有
原有强引用。谓词 panic 不释放借用的引用；命中后的摘除转交列表的真实 Arc，
此后不再使用临时视图。相比 clone_pointer，这避免每个被检查节点的原子计数成本。

ReadyList Drop 正常逐个摘除并清除归属，外部仍持有的实体可以重新入队。
队列拥有一个 Arc，摘除通过库转交所有权；不使用 fast_clear，不手写引用计数。
ReadyItem 私有 unsafe trait 只约束字段偏移与字段封装，支持 RR const 泛型；
ReadyAdapter 的正反投影使用具体字段 offset_of，不自写链表算法。
