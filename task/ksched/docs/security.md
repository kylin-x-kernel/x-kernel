# ksched — 安全与可靠性分析

## 信任模型

```
ktask RunQueue（生产调用方）
   │  持有 RQ 锁后调用 Scheduler
   v
Stop（每 RQ 单槽） -> Deadline CBS+EDF -> RT FIFO/RR -> Fair EEVDF -> Idle
```

- 调用方必须保证：同一 RQ 上序列化访问；运行任务离开必经
  `yield_current` / `preempt_current` / `block_current` / `migrate_current` /
  `exit_current`。
- `Scheduler` 的公有方法是安全的。`RqLocked::assume()` 只出现在私有
  `token()` 里：`ktask` 在持有该 RQ 锁时调用这些方法。
  `init_attr_unpublished` 仍是 `pub(crate)`，只适用于尚未发布、由创建者独占
  的任务。本 crate 不检查锁本身。
- 调度器信任传入的任务状态已由调用方校验。本 crate 只通过
  `SchedAttr::validate` 检查结构：FIFO/RR 的优先级必须在 `1..=99`。
  凭据校验不在这里做。调用 `set_attr` 放入 RT 之前，调用方必须已经确认
  调用者具备实时调度权限（Linux `CAP_SYS_NICE`；当前内核用 `euid == 0` 代替）。
  现在的 `sys_sched_setscheduler` 只校验 policy 和 priority 的取值，然后把
  结果写进线程快照，既不查权限，也不调用 `Scheduler::set_attr`，所以非特权
  进程可以让这次系统调用返回 0，但任务不会进入 `RtClass`。接线时如果漏掉
  权限检查，FIFO 99 就能一直抢在全部 Fair 任务前面。

## 外部边界 / 攻击面

新增 `SharedOwner<T>` 是 unsafe 实现边界：实现必须保持同一值的稳定
地址，Clone 创建独立 owner，raw 往返既不释放也不增加计数，有效指针的
恢复不能 panic。调用方仍须保证 owner 的释放适合 RQ 锁和执行上下文。
默认 Arc 的行为不变；DeferredArc 的末引用交接遵循 kfinalize 契约。

Ready/Fair/Deadline 的 OwnerPointerOps 只转交这份责任，不从借用引用
恢复控制块。侵入式库可能用临时 ManuallyDrop owner 克隆节点引用，
临时视图不消费队列保留的 count，真实 owner 覆盖其全部使用。适配器
必须保留 into_raw 的完整指针 provenance，字段投影逆运算才可恢复
DeferredArc 的 payload 头部。缓存、双索引和 current 的每份 owner 都
独立计数；非空队列析构逐个摘除，不 fast_clear，也不现场析构 Deferred
payload。真实三类队列的宿主/Miri 和 guest 测试覆盖该边界。

RT 按任务摘除必须使用 &Owner 的原始指针，不能用 &SchedTask 派生
指针跨越 payload 头部；Miri 曾在该路径报告越出借用权限。修正为
Owner::as_ptr 后，RT 与核心 raw 案例通过 strict-provenance Miri。
Fair/Deadline 所用 intrusive-collections 0.10.2 存在 parent/color
整数指针编码，仅使用默认 provenance 模式验证并保留警告，不能称为
严格 provenance 通过；不关闭 alias/leak 检查。

`ksched` 不直接接触用户内存、设备或固件输入。攻击面主要是：

- 错误的调度 API 使用顺序（漏 `leave_current`、重复入队）；
- 优先级等参数范围（nice 边界由各实现校验）；
- **Deadline 参数三元组**：`DlParams { runtime_ns, deadline_ns, period_ns }` 最终来自
  `sched_setattr` 的用户缓冲区。本 crate 的入口是 `SchedAttr::validate` →
  `DlParams::new`，校验非零、`runtime <= deadline <= period` 以及
  `runtime >= SCHED_DL_MIN_RUNTIME_US`；非法组合一律返回 `InvalidPolicy`；
- **带宽总量**：单 RQ 上任务 `runtime/period` 之和（就绪树 + 节流树 + runner）
  不得超过 `1 - SCHED_DL_BW_MARGIN_PCT`。这是资源公平性约束而非安全边界：
  越限返回 `AdmissionFull`，不会导致越界或状态损坏。

## unsafe 代码清单

Deadline 类新增 `DlAdapter<T, const READY: bool>`，与 `FairAdapter` 同构：
`get_link` / `get_value` 用 `core::mem::offset_of!` 在同一 `SchedTask<T>` 上做
正反字段投影，分别指向 `deadline.ready_link` 与 `deadline.throttled_link`。

不变量：

1. 每个实例化只投影一个真实存在的 `RBTreeAtomicLink` 字段；
2. `link` 必须由同一适配器的 `get_link` 产出，且实体存活；
3. 反向投影只做字段偏移减法，不构造独占引用，不改变所有权；
4. 树的键在链接期间不可变：`deadline_ns` / `period_start_ns` / `tree_id` 只在
   摘除后修改；
5. `KeyAdapter::get_key` 里的 `RqLocked::assume()` 只在持有 RQ 锁的类方法
   （insert / find / 游标遍历）中被库调用；树析构不调用 `get_key`。

各算法实现主要依赖安全集合/`Arc`。FIFO/RR 的 unsafe 集中在 ready_list：
ReadyItem 声明具体字段偏移，ReadyAdapter 完成正反指针投影；按实体删除先核对
这条 `ReadyList` 的地址，再使用 cursor_mut_from_ptr。不能仅凭 is_linked
判断属于本队列。源队列独占借用阻止检查到摘除之间发生本队列修改。

已经链接的任务不会改写 owner。未链接的先记下本链表地址再入链，并在库
释放链接之前清零。错误队列删除返回 None。运行队列公布之后，含有
节点的链表对象不能再移动，否则地址不再等于 owner。Drop 清除所有归属及
拥有的 Arc。
owner 不是链表同步锁；Relaxed 足够用于身份检查，同一队列的操作已有独占访问，
其他队列只能写其自身地址或零，不能伪造本队列地址。跨队列取得链接依赖库的
原子链接操作。所有链接和身份字段均不公开，不添加手写 Send/Sync。

ReadyList::steal 通过游标 get 借用任务，谓词只接收 &T，不恢复临时 Arc，
也不增加或减少强引用。游标借用保护谓词访问；命中后先清除归属标记，
结束任务借用，再由 remove 转出列表原有的拥有型引用。拒绝或谓词 panic
不会转移列表所有权。

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
值保证。`seat`、`attr`、Fair 虚拟时间和 `rr_left_ns` 走同一套令牌。

`class` 与 `rt_prio` 是 `AtomicU8`。`sched_class()` / `rt_priority_snapshot()`
用 Acquire，配对 `set_class` / `store_rt_prio` 的 Release，给不持 RQ 锁的选核
读。`seat` 仍走 `RqCell`，只在锁下读。

RT 按地址摘除在 `ready_list`。`ReadyItem` 给出 `ReadyLinks` 的字段偏移，
`ReadyAdapter` 用同一偏移做正反投影。摘除先核对归属字是否等于这条链表的
地址，再调用 `cursor_mut_from_ptr`。不能只凭 `is_linked` 判断属于本队列。
队列的独占借用盖住从检查到摘除的窗口。

已经链接的任务不会改写归属。未链接的先记下本链表地址再入链，并在库释放
链接之前清零。错误队列删除返回
`None`；重复入队由 `AtomicLink` 拒绝，不覆盖归属。`Drop` 逐个摘除并清归属。
归属不是链表锁；Relaxed 足够，因为同一条链表的操作已经独占，别的链表只能
写自己的地址或零。

steal 用游标 `get_ptr` 构造 `ManuallyDrop<Arc<_>>`，不经 `&T` 再向前恢复
`Arc` 头。链表仍持有原来的强引用。谓词只拿到 `&Arc`。命中后 `cursor.remove`
转交那份 `Arc`，临时视图不再使用。

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
8. RT 任务同一时刻只挂在一条优先级链表上。归属字等于该链表的身份。摘除
   失败不得减少 `queued`。节流期间任务仍可留在链表里，但 `has_runnable`
   为假，`pick_next` 不得选出它们。
9. 每个 RQ 至多一个 Stop 任务。waiter 槽至多持有它排队时的一份 `Arc`；runner
   在 `Scheduler::curr`，不在槽里。第二个 `enqueue`，或在 waiter 已在时
   `install_current` 另一个 Stop 任务，都是契约破坏，必须断言失败，不能覆盖
   前一个 `Arc`。调用方在创建和唤醒时自行保证每 RQ 只有一个 Stop 任务。

## 线程安全

`BaseScheduler` 本身不是可并发类型；并发安全由 `ktask` 的 per-CPU RQ 锁与
IRQ/preempt 约束提供。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 漏 `leave_current` 导致陈旧 `curr`/统计失真 | 高 | 新 leave 路径未接入统一 API | 统一 `leave_current`；EEVDF `pick_next` 断言 |
| T-02 | 调度器持有运行任务 `Arc` 拖住生命周期 | 高 | `curr` 强引用未释放 | `curr` 改为非 owning 快照 |
| T-03 | Exit 误设 PLACE_LAG | 中 | Exit 与 Block 共用 sleep 记账 | `LeaveReason::Exit` 单独分支 |
| T-04 | 迁移后源 RQ 仍计入任务权重 | 中 | Migrate 未清 current 记账 | Migrate 走 `leave_current` 清快照 |
| T-05 | `curr` 快照过期导致错误抢占/放置 | 中 | 改运行实体字段后未刷新 | `update_current`/`set_priority` 刷新；测试辅助显式同步 |
| T-06 | idle-pull 偷走仍 `on_cpu` 的实体 | 高 | yield/preempt 入树后远端 `steal_ready_task` | 调度器只摘 ready waiter；ktask 谓词拒绝 `on_cpu`，不 `pick_next` |
| T-07 | 双树或缓存残留导致错误选取/持有 | 高 | 摘除、改键或 Drop 漏清一份链接 | 同锁摘双树，游标后继更新缓存，正常树 Drop；成员/引用/重入测试 |
| T-08 | `Scheduler` 在未持 RQ 锁时写 `RqCell` | 高 | 调用方违反锁契约 | 文档约定；令牌只在 `Scheduler` 入口和未发布初始化处构造 |
| T-09 | 仲裁器 `curr` 的 `Arc` 在 Block 后成为唯一引用 | 高 | 调用方未另持任务 | `leave` 释放仲裁器引用；调用方必须另持任务 |
| T-10 | 同一 RQ 上出现第二个 Stop 任务 | 高 | 重复 activate、误入队，或在已有 waiter 时 `install_current` | `enqueue` 与 `set_curr` 在槽已被占用时断言失败 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | pick 时 curr 非空 | 调用顺序错误 | 断言失败 | 调度停止（暴露 bug） | 2 | 强制 leave 契约 |
| F-02 | 唤醒未 PLACE_LAG | Block/Migrate 未记 lag | 放置偏差 | 延迟/公平性劣化 | 3 | disposition 区分 + 单测 |
| F-03 | Block 后无额外强引用 | 调用方未 pin 任务 | `switch_to` 断言失败 | 调度停止 | 2 | `blocked_resched` 要求 `strong_count > 1` |
| F-04 | idle-pull 迁入未 PLACE_LAG | 摘任务不走 `remove_task` | 目的 RQ 放置偏差 | 迁入任务公平性漂移 | 3 | `steal_ready_task` 走 `remove_task`；dest `enqueue_task` PLACE_LAG |
| F-05 | 第二个 Stop 任务 | 同一 RQ 再次入队，或 waiter 已在时安装 Stop runner | 断言失败 | 调度停止（暴露 bug） | 2 | `enqueue` 与 `set_curr` 断言；`steal` 不搬走 Stop |

## 故障管理

- 契约破坏优先 `assert!`（编程错误，不应静默恢复）。
- 用户策略不合法时 `SchedAttr::validate` 返回 `SchedError`，由 `ktask` 传给调用方。

## 隐私分析

无用户数据；仅操作内核任务调度元数据。

## 已知限制

- Stop 队列已在仲裁器里，但 `ktask` 还不创建 Stop 任务，也没有 stop-machine。
  `SCHED_DEADLINE` 需携带合法 `DlParams`，否则 `SchedAttr::validate` 以 `InvalidPolicy` 拒绝；每 RQ 准入失败以 `AdmissionFull` 拒绝（非权限检查）。
- `sched_setscheduler` 尚未把 FIFO/RR 送进 `RtClass`，也没有
  `CAP_SYS_NICE` 检查。接线前必须先补上，否则非特权调用者可以长期占住 CPU。
- `SCHED_RESET_ON_FORK` 能通过策略字校验并记在线程快照里，fork 尚未按该标志
  把子任务打回 `SCHED_NORMAL`。
- RT 带宽按每个 RQ 计算，没有跨 CPU 的根域带宽池。本 RQ 节流后不再
  `pick_next` 这些任务，但 idle-pull 仍可把它们摘到另一颗 CPU，改记那颗
  CPU 的 runtime。选核读 `rt_preempt_rank`：本周期尚未结束的节流 RQ 记为
  255，与 Stop/Deadline 一样不视为可抢占，只在没有其他可抢占 CPU 时退回
  hint。判定用 `is_throttled_at`（`now >= period_start_ns + rt_period_ns`
  即不再节流）。队列被摘空后仍武装周期边界，空闲 CPU 到点会清标志并改写
  已发布的 255，避免这颗核在低负载下一直被选核跳过。
  没有 Linux `cpupri` push/pull。`sched_setscheduler` 仍不把任务送进这个类。

## 审计清单

- [ ] 新增离开路径是否只调用 `leave_current`。
- [ ] EEVDF `curr` 是否仍保持非 owning。
- [ ] Exit 是否避免 PLACE_LAG。
- [ ] 迁移后源 RQ 的 V/weight 是否不再包含迁出任务。
- [ ] Block 路径调用方是否另持强引用。
- [ ] 单测是否覆盖五种 disposition 与 pick-without-leave。
- [ ] `steal_ready_task` 是否走 `remove_task` 且不安装 `curr`。
- [ ] EEVDF 双树和最早缓存是否一致，所有改键是否先摘树。
- [ ] 排队任务改 nice 是否先扣除旧权重贡献，再按新权重入队；摘除后是否无残账。
- [ ] Adapter 的正反投影是否使用同一字段偏移，树是否持有真实 Arc。
- [ ] `Scheduler` 调用是否仍在 RQ 锁下；`curr` 的 `Arc` 是否在 leave 时释放。
- [ ] Stop / Idle 的 `set_attr` 是否仍返回 `KernelClassImmutable`，且第二个 Stop `enqueue` 或在 waiter 已在时 `install_current` 是否断言失败。
- [ ] `DlParams` 的零值 / 次序颠倒 / 低于最小预算是否都被拒（不截断、不归一化）。
- [ ] 准入失败是否在改动任务属性之前返回（拒绝后 `attr` 保持不变）。
- [ ] Deadline 的两个链接在 `advance` / `dequeue` / `put_prev` 各路径是否**至多一个**处于链接状态（不得同时挂在就绪树与节流树上）。
- [ ] RT 摘除是否先核对队列身份，再 `cursor_mut_from_ptr`。

实际运行时间的写入由所属 RQ 锁串行化；原子字段仅支持跨 CPU 安全读取，
不发布其他任务状态。迁移须交接 RQ 所有权后再更新，累计溢出采用饱和语义。
