# ksched — 安全与可靠性分析

## 信任模型

```
ktask RunQueue（生产调用方，仍走插件）
   │
   │ BaseScheduler safe API
   │
   v
┌──────────────────────────────────┐
│  FIFO / RR / CFS / EEVDF         │
└──────────────────────────────────┘

Scheduler（ktask 尚未调用）
   │  调用方必须已持有目标 RQ 锁
   v
Stop / Deadline / RT（未接线） -> Fair EEVDF -> Idle
```

- 插件调用方必须保证：同一 RQ 上序列化访问；运行任务离开必经 `leave_current`。
- 读写 `RqCell` 的 `Scheduler` 方法是 `unsafe`。`RqLocked::assume()` 只出现在
  这些 `unsafe fn` 内部，前提由调用方在 `# Safety` 里保证：已持有该 RQ 锁。
  `init_attr_unpublished` 仍是 `pub(crate)`，只适用于尚未发布、由创建者独占
  的任务。本 crate 不检查锁本身。
- 调度器信任传入的任务状态已由调用方校验。用户策略只经 `SchedAttr::validate`。

## 外部边界 / 攻击面

`ksched` 不直接接触用户内存、设备或固件输入。攻击面主要是：

- 错误的调度 API 使用顺序（漏 `leave_current`、重复入队）；
- 优先级等参数范围（nice 边界由各实现校验）。

## unsafe 代码清单

各算法实现主要依赖安全集合/`Arc`。FIFO/RR 的 unsafe 集中在 ready_list：
ReadyItem 声明具体字段偏移，ReadyAdapter 完成正反指针投影；按实体删除先核对
非零且永不复用的队列身份，再使用 cursor_mut_from_ptr。不能仅凭 is_linked
判断属于本队列。源队列独占借用阻止检查到摘除之间发生本队列修改。

owner 只在库成功取得链接后写入，并在库释放链接之前清零。错误队列删除返回
None；重复入队由 AtomicLink 拒绝，不覆盖 owner。队列移动不改变身份，Drop
清除所有归属及拥有的 Arc。编号耗尽明确 panic，不复用身份造成误删。
owner 不是链表同步锁；Relaxed 足够用于身份检查，同一队列的操作已有独占访问，
其他队列只能写其自身身份或零，不能伪造本队列身份。跨队列取得链接依赖库的
原子链接操作。所有链接和身份字段均不公开，不添加手写 Send/Sync。

steal 的临时 Arc 视图是额外的原始所有权指针审计点：指针来自游标 get_ptr，
没有经 &T 缩窄后再向前恢复 Arc 头部。列表仍拥有原来的强引用，ManuallyDrop
阻止正常或 panic 路径释放它；谓词仅得到 &Arc，可 clone 但不能消费借来的 Arc。
视图不逃逸，不在游标摘除后访问；真实所有权仍只经库的 remove 返回一次。

EEVDF 新增私有 EntityAdapter 的两个原始指针投影：get_link 将活实体指针加上
本适配器选定字段的 offset_of，get_value 对该适配器先前产出的链接指针做逆运算。
偏移取自同一具体 EevdfEntity<T,S> 类型，不假设 Rust 字段排列或整数地址重建。
树使用库的 DefaultPointerOps<Arc<_>> 管理所有权；无裸借用索引。
适配器只负责字段定位而非树的平衡算法。宏不支持 const 泛型，故这里显式实现。
EEVDF steal 另有原始所有权指针审计点：从游标 get_ptr（不是 &T）取得
树拥有的 Arc 数据指针，ManuallyDrop 只借用该强引用，谓词只接收 &Arc。
树借用覆盖整个谓词调用，阻止摘除和析构；panic 不释放借来的强引用。
只有命中才 clone 真正拥有的 Arc，临时视图结束后走 remove_task；不在
摘除后访问临时视图，也不向调用方转交这份借来的引用。

`FairClass` 另有一套 `FairAdapter`，投影的是 `SchedTask<T>` 里
`fair.deadline_link` / `fair.vruntime_link`，不是插件 `EevdfEntity`。
`get_link` / `get_value` 用同一字段 `offset_of` 互逆，不改 `Arc` 所有权；
两棵树各持一份 `Arc`。`KeyAdapter::get_key` 在安全函数里 `RqLocked::assume()`：
插入、查找和游标只从已持有令牌的 `FairClass` 方法进入；`RBTree` 的 `Drop`
走 `clear()`，不调用 `get_key`，所以非空 `Scheduler` 析构不会在无锁下读
`RqCell`。

`RqCell` 的 `get` / `set` 是 `UnsafeCell` 读写。读要求指针有效、值已初始化、
无并发写；写还要求独占。这三件事都由 `RqLocked` 加上 `new`/`set` 的完整 `Copy`
值保证。`seat` 与 `attr`、Fair 虚拟时间字段走同一套令牌。`class` 仍是
`AtomicU8`，给无锁的 `sched_class()` 读。

## 内存安全不变量

1. EEVDF Ready 任务的两棵树各持有一份 Arc，两个最早缓存和 next_buddy 可额外持有；
   树摘除后再转移 CPU，返还持有覆盖附加引用释放，不能提前最终析构。
2. EEVDF `curr` 不得持有 `Arc`；仅保存调度数值快照。
3. `pick_next_task` 前 `curr` 必须为空（已 `leave_current`）。
4. Exit 不得设置 PLACE_LAG；Block/Migrate 必须设置，供后续 `enqueue_task`。
5. 运行实体调度字段变更后，`curr` 快照必须同步刷新。
6. 两棵 EEVDF 树成员相同；挂树期间排序字段不变。修改优先级先摘两树再更新。
7. 按实体摘除先核对键与指针身份，不用 is_linked 推断属于本树；错误队列中的
   set_priority 不得修改挂在别处的实体。重复入队为调用方契约错误，链接断言会拒绝。

## 线程安全

`BaseScheduler` 本身不是可并发类型；并发安全由 `ktask` 的 per-CPU RQ 锁与
IRQ/preempt 约束提供。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 漏 `leave_current` 导致陈旧 `curr`/统计失真 | 高 | 新 leave 路径未接入统一 API | 统一 `leave_current`；EEVDF `pick_next` 断言 |
| T-02 | 调度器持有运行任务 `Arc` 拖住生命周期 | 高 | `curr` 强引用未释放 | `curr` 改为非 owning 快照 |
| T-03 | Exit 误设 PLACE_LAG | 中 | Exit 与 Block 共用 sleep 记账 | `CurrentDisposition::Exit` 单独分支 |
| T-04 | 迁移后源 RQ 仍计入任务权重 | 中 | Migrate 未清 current 记账 | Migrate 走 `leave_current` 清快照 |
| T-05 | `curr` 快照过期导致错误抢占/放置 | 中 | 改运行实体字段后未刷新 | `update_current`/`set_priority` 刷新；测试辅助显式同步 |
| T-06 | idle-pull 偷走仍 `on_cpu` 的实体 | 高 | yield/preempt 入树后远端 `steal_ready_task` | 调度器只摘 ready waiter；ktask 谓词拒绝 `on_cpu`，不 `pick_next` |
| T-07 | 双树或缓存残留导致错误选取/持有 | 高 | 摘除、改键或 Drop 漏清一份链接 | 同锁摘双树，游标后继更新缓存，正常树 Drop；成员/引用/重入测试 |
| T-08 | `Scheduler` 在未持 RQ 锁时写 `RqCell` | 高 | 调用方违反锁契约 | 文档约定；令牌只在 `Scheduler` 入口和未发布初始化处构造 |
| T-09 | 仲裁器 `curr` 的 `Arc` 在 Block 后成为唯一引用 | 高 | 调用方未另持任务 | `leave` 释放仲裁器引用；调用方必须另持任务 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | pick 时 curr 非空 | 调用顺序错误 | 断言失败 | 调度停止（暴露 bug） | 2 | 强制 leave 契约 |
| F-02 | 唤醒未 PLACE_LAG | Block/Migrate 未记 lag | 放置偏差 | 延迟/公平性劣化 | 3 | disposition 区分 + 单测 |
| F-03 | Block 后无额外强引用 | 调用方未 pin 任务 | `switch_to` 断言失败 | 调度停止 | 2 | `blocked_resched` 要求 `strong_count > 1` |
| F-04 | idle-pull 迁入未 PLACE_LAG | 摘任务不走 `remove_task` | 目的 RQ 放置偏差 | 迁入任务公平性漂移 | 3 | `steal_ready_task` 走 `remove_task`；dest `enqueue_task` PLACE_LAG |

## 故障管理

- 契约破坏优先 `assert!`（编程错误，不应静默恢复）。
- 优先级越界返回 `false`，由上层决定是否向用户返回错误。

## 隐私分析

无用户数据；仅操作内核任务调度元数据。

## 已知限制

- CFS 在 `ktask` feature 接线仍指向历史 `axsched` 名称时不可用；算法本体在本 crate。
- `Scheduler` 尚未替换 `ktask` 的 `BaseScheduler`。Stop、Deadline、RT 没有队列，
  对应 `sched_setscheduler` 策略在 `SchedAttr::validate` 被拒绝。

## 审计清单

- [ ] 新增离开路径是否只调用 `leave_current`。
- [ ] EEVDF `curr` 是否仍保持非 owning。
- [ ] Exit 是否避免 PLACE_LAG。
- [ ] 迁移后源 RQ 的 V/weight 是否不再包含迁出任务。
- [ ] Block 路径调用方是否另持强引用。
- [ ] 单测是否覆盖五种 disposition 与 pick-without-leave。
- [ ] `steal_ready_task` 是否走 `remove_task` 且不安装 `curr`。
- [ ] EEVDF 双树和最早缓存是否一致，所有改键是否先摘树。
- [ ] Adapter 的正反投影是否使用同一字段偏移，树是否持有真实 Arc。
- [ ] `Scheduler` 调用是否仍在 RQ 锁下；`curr` 的 `Arc` 是否在 leave 时释放。
- [ ] 未接线 class 的用户策略是否仍被 `validate` 拒绝。
