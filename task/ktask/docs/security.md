# ktask — 安全与可靠性分析

## 概述

`ktask` 处于内核并发与调度核心路径，包含多处 `unsafe`、per-CPU 原始引用、上下文切换裸指针、以及跨 CPU 任务状态同步。错误的不变量可能导致：

- use-after-free（任务退出与回收竞态）
- 数据竞争（per-CPU 可变引用别名）
- 调度状态损坏（同一任务被重复入队或并发切换）
- NMI/诊断路径读取非法对象

本模块本身不直接处理用户态输入，但通过 `sleep/wait/join/affinity` 等 API 间接承载用户态行为结果；其安全边界属于“内核内部并发正确性与内存安全”。

## 信任模型

```text
kruntime timer irq / irq manager / kspin guards
        │
        ▼
┌───────────────────────────────────────────────────┐
│ ktask                                             │
│  - task.rs: TaskInner/CurrentTask/context         │
│  - run_queue.rs: per-CPU runqueue + switch_to     │
│  - future/wait_queue: block/wake bridge           │
│  - timers: tick callbacks                         │
│  - snapshot/task_registry (optional)              │
└───────────────────────────────────────────────────┘
        │
        ▼
ksched algorithms / karch context switch / allocator
```

- 信任 `karch` 的上下文切换原语与栈切换 ABI。
- 信任 `kspin` guard 的 acquire/release 顺序与 preempt 接口接线。
- 信任 `percpu` 原语在 CPU 本地访问语义上正确。
- 信任 `ksched` 不会返回非法调度实体状态。

## unsafe 代码清单（按模块）

### 1) `run_queue.rs`：per-CPU 原始可变引用与全局数组

关键点：

- `RUN_QUEUE.current_ref_mut_raw()` 生成 `&'static mut RunQueue`
- `RUN_QUEUES[index].assume_init_mut()` 读取 `MaybeUninit` 全局表

**不变量**：

1. 每 CPU 的 `RUN_QUEUE` 在 `init/init_secondary` 后才可访问；
2. 不存在同一 CPU `RUN_QUEUE` 的并发可变别名（由 guard/IRQ/preempt 约束）；
3. `RUN_QUEUES` 在读前完成初始化写入。

### 2) `run_queue.rs`：上下文切换与 `CurrentTask` 置换

`switch_to` 路径通过原始上下文指针切换，切换前后依赖 `on_cpu`/`PREV_TASK` 维护跨 CPU 可见状态。

**不变量**：

1. 切换时本地 IRQ 已关闭；
2. `prev`/`next` 上下文指针有效，且栈未被回收；
3. `clear_prev_task_on_cpu()` 与 blocked-task 唤醒 handoff 成对：SeqCst
   store/load 加上 store 与对侧 load 之间的 `SeqCst` fence（Dekker 的内存模型
   要求，不可当作 TCG 兜底删除），保证至少一方完成入队；禁止 IRQ-off 自旋
   等待远端 `on_cpu`。

### 3) `task.rs`：`TaskContext` 内部可变与当前任务 TLS/CPU-local 指针

- `UnsafeCell<TaskContext>` 的共享读取
- `CurrentTask::init_current/set_current` 写 per-CPU 当前任务指针
- `TaskStack` 手动分配/释放

**不变量**：

1. 任务上下文只在受控调度路径被修改；
2. 当前任务指针在每 CPU 上始终指向有效任务对象；
3. 栈释放发生在任务不再运行且无引用后（由 gc task + `Arc::try_unwrap` 保证）。

### 4) `task.rs`：用户 runtime 的不可变 slot

`UserRuntimeSlot` 使用 `UnsafeCell<Option<Box<dyn UserTaskRuntime>>>` 保存 runtime。runtime 在
`TaskInner::new_user()` 构造时一次性写入，之后不再修改，因此 slot 不再需要发布状态机
（旧的 `EMPTY -> INSTALLING -> READY` 协议与 `install_user_runtime()` 已移除）。

**不变量**：

1. `UserRuntimeSlot::ready()` 是唯一的构造路径，runtime 在 task 共享前就已填入；
2. task 发布（`publish_user_task` / `activate_task`）提供 readers 的 happens-before 边界；
3. runtime 在 task 生命周期内不可变，因此 `get()` 返回的共享引用稳定有效。

### 5) `timers.rs` / `task_registry.rs` / `snapshot`

- `timers` 使用 per-CPU callback 容器原始引用；
- `task_registry` 使用 `Box::into_raw/from_raw` 存放弱引用槽位；
- `snapshot` 使用 `UnsafeCell` 持有 trap frame 快照，并 `unsafe impl Sync`。

**不变量**：

1. `task_registry` 槽位只存 0 或有效 `Box<WeakKtaskRef>` 指针；
2. CAS 成功的一方负责释放；
3. snapshot 读写遵守 session 串行化约束（`begin`/`finish`）；
4. 周期 callback 调用期间不保留 callback vector 的引用：先以 `Arc` 固定闭包并把当前
   entry 临时置为 inactive，回调完成后再按稳定 index 写入新 deadline；注册只允许 append，
   不提供删除，因此回调内注册造成的 vector reallocate 不会悬空引用；
5. 周期 deadline 从回调完成时间向后推进一个 period；超长 `Duration` 向 `u64::MAX`
   饱和，禁止窄化截断和 overrun catch-up IRQ 循环。仲裁时跳过 `u64::MAX` 哨兵，
   避免把 in-flight 项当成最远期 deadline 装进 32-bit TVAL。
6. `register_timer_irq_note` 只写入 `'static fn()`；timer IRQ 路径 Acquire 后
   transmute 调用，钩子必须短小且不阻塞。

## 内存与并发不变量

1. **任务状态机单向约束**：`Running/Ready/Blocked/Exited` 转换由 run queue 统一入口执行；`transition_state` 防止重复唤醒重入。
2. **延迟抢占模型**：schedule timer / wake 仅设置 `need_resched`，真实切换在 `enable_preempt` 安全点触发，避免临界区中途切换；若当前 CPU 仍处于 active exception context，则继续延迟抢占直到异常/IRQ trapframe guard 释放。
3. **SMP blocked 唤醒保护**：Blocked 任务若仍在远端 `switch_to`，唤醒方发布
   target RQ/wake flags 后立即返回；原 CPU 清除 `on_cpu` 时通过 atomic swap
   唯一认领并完成入队。四处握手操作使用 SeqCst，并在 store 与对侧 load 之间加
   `SeqCst` fence（Dekker 内存模型要求；不可降为 acquire/release）。没有这道
   fence，真机 x86 与 QEMU TCG 都允许双方跳过入队；也不在 IRQ-off 区等待远端进展。
4. **唤醒抢占请求分离**：waker 只把任务转为 `Ready` 并设置本地或远端 `need_resched` 请求，真实切换仍发生在抢占安全点；远端 IPI 只置 pending，不 account/refresh 目标 RQ。立即 schedule 请求消费 one-shot slot 而不循环重装 10μs timer；`smp` 缺少 `ipi` 在编译期拒绝。
5. **退出回收隔离**：退出任务先进入 `EXITED_TASKS`，由每 CPU `gc_task` 延迟回收，避免切换路径直接 drop。
6. **idle 任务特判**：idle 不入普通调度实体路径，不参与 `update_current`，避免算法元数据污染。
7. **发布先于 runnable**：需要额外注册对象图的调用方必须先 `prepare_task()`，
   完成外部 publish 后再 `activate_task()`，避免 task 先运行、后补注册。PID 1 同样
   遵守此约束：它由 `new_user()` 一次性构造完整 runtime，再经 `publish_user_task()`
   发布后才激活，不存在 runnable 后补装 extension 的路径。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | per-CPU `&'static mut` 别名导致 UB | 高 | 同 CPU 并发可变借用 | guard + IRQ/preempt 约束；调用点集中封装 |
| T-02 | 任务切换与唤醒并发，导致同任务重复入队、丢失唤醒或 IRQ-off 死锁 | 高 | 远端 CPU 尚在切换，当前 CPU 立即 unblock | SeqCst Dekker handoff（flags store/swap × `on_cpu` store/load + store/load 之间不可降级的 `SeqCst` fence）+ `clear_prev_task_on_cpu` 唯一认领；禁止等待远端 `on_cpu` |
| T-03 | 退出任务过早释放导致 UAF | 高 | 仍有 joiner/切换路径持有引用 | per-task exit `Completion` 唤醒 joiner；`EXITED_TASKS` + `gc_task` + `Arc::try_unwrap` 延迟回收 |
| T-04 | `need_resched` 在错误时机触发重入调度 | 中 | 临界区内或异常/IRQ trapframe guard 未释放时直接抢占切换 | 仅设置 pending，`enable_preempt` 安全点检查，并在 active exception context 内延迟实际切换 |
| T-05 | `task_registry` 指针槽位损坏 | 高 | 非法写入或重复释放 | CAS 协议 + 0/ptr 双态约束 + 弱引用升级校验 |
| T-06 | snapshot 竞态读取错误 trap frame | 中 | 并发 snapshot session | begin/finish 会话串行 + per-CPU 槽位隔离 |
| T-07 | affinity 迁移竞态导致任务丢失 | 中 | 迁移中状态被并发修改 | `migrate_current` 受 run queue 临界区保护；`enforce_affinity_placement` 对 ready 持 RQ 锁 `remove_task`，idle-pull 中间态自旋等到再入队，对 running 等远端 preempt 完成 |
| T-07a | `setaffinity` 静默成功但任务仍在非法 CPU | 高 | 非 current 只写 mask | 成功路径要求 placement 合法，否则 `false`/`EBUSY`；`preempt_resched`/`yield` 强制 affinity migrate |
| T-07b | 运行任务离开路径漏 deactivate 导致调度器残留状态 | 高 | 新 leave 路径绕过统一 API | 全部经 `leave_current`；EEVDF `curr` 非 owning；`pick_next` 断言 |
| T-07c | `blocked_resched` 无额外强引用导致切换时任务被释放 | 高 | 调用方未 clone current | rustdoc/`# Panics` 约定；`#[track_caller]` + `strong_count > 1` 硬断言；`block_on` 先 clone；unittest `blocked_resched_survives_with_caller_owned_ref` |
| T-08 | 周期回调执行耗时过长拖慢调度或形成 IRQ 重入循环 | 高 | callback 滥用或执行时间超过 period | API 约束 hardirq 回调短小且不阻塞；一次 IRQ 最多调用每个到期 callback 一次，下一期限从完成时间计算 |
| T-09 | 远端唤醒后未及时调度 | 中 | 任务入远端 run queue 但远端 CPU 未到抢占安全点 | SMP 强制依赖 IPI；远端 IPI 只置 `need_resched`；探测失败才在目标 CPU `preempt_resched` 武装 backup hrtick |
| T-09b | IRQ teardown 等待在错误上下文阻塞当前任务 | 高 | hardirq/softirq/BH-disabled 路径间接调用 `IrqSyncWaitIf` provider | `kirq` 在进入 provider 前执行 context gate；`ktask` 只提供阻塞机制，不放宽 IRQ 同步 API 约束 |
| T-09c | workerqueue 等待绕过 kwork 生命周期谓词 | 高 | `ktask` provider 直接阻塞而不让 `kwork` 重查 work 状态 | `ktask` 只实现 `kwork::WorkqueueSyncWaitIf` 的 completion wait；work 状态、cancel/flush 谓词和 deadlock gate 仍由 `kwork` 持有 |
| T-09d | custom/dynamic workqueue 错误创建专属 task | 中 | provider API 重新引入 queue-level host/stop/wake 或保存 `WorkQueueHandle` | `WorkqueueHostIf` 只暴露 per-CPU pool ready/worker/manager wake；所有逻辑 queue 共享 pool task，destroy 不进入 ktask |
| T-09e | interrupt-like context 直接调用 `ktask::sleep*()` | 高 | BH workqueue callback、softirq action、hardirq 或 BH-disabled path 绕过上层 context gate | `sleep()`、`sleep_until()`、`interruptible_sleep_until()` 统一检查 `kirq::context::is_in_interrupt_context()` 并 fail-fast；`yield_now()` 不作为 sleep/blocking API |
| T-10 | 绝对 timer deadline / 周期在整数转换时截断 | 高 | `as_nanos()` 的 `u128` 结果直接窄化为 `u64` | HAL 接口保持 `MonotonicInstant`；backend 在寄存器边界钳制；周期 `Duration` 转换向 `u64::MAX` 饱和 |
| T-11 | 已到期 schedule slot 在不可抢占区反复重装 | 高 | immediate request 被改写为周期性 10μs hrtick | 到期 slot 只设置 pending 并立即清零；soft/periodic deadline 独立保留 |
| T-12 | idle-pull 偷走仍 `on_cpu` 的任务 | 高 | yield/preempt 入树后、`switch_to` 完成前远端 steal | 只偷 Ready && `!on_cpu`；`can_idle_pull_task` 拒绝 `on_cpu` |
| T-12a | steal 改 `cpu_id` 把 prev 搬走 | 中 | idle-pull 迁入后唤醒跟到新核 | 唤醒走 `select_idle_sibling`（prev 空闲则回家，否则再找 idle）；与 Linux 抢椅子一致 |
| T-12b | idle-pull 中间态被 `setaffinity` 漏迁 | 高 | steal 后、dest 入队前 Ready 不在任何 RQ | dest 入队后复查 cpumask，不含 dest 则 `migrate_entry`；`enforce_affinity_placement` 对该窗口自旋等到再入队/Running |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `RUN_QUEUES` 未初始化访问 | 初始化时序错误 | panic/UB | 调度不可用 | 1 | 在 `init/init_secondary` 后写入，调用路径约束 |
| F-02 | `cpumask` 为空 | 调用方设置非法 affinity | 任务无可运行 CPU | API 失败或 panic | 2 | `set_task_affinity` 判空返回 false |
| F-02a | 远端 running affinity 迁不走 | 无 preempt/IPI 或安全点过晚 | 调用方见失败 | 调用方可重试；mask 可能已更新 | 3 | `enforce_affinity_placement` 返回 false → syscall `EBUSY` |
| F-03 | 长时间持有 NoPreempt 临界区 | 代码路径过重 | 抢占延迟增大 | 延迟抖动/实时性下降 | 3 | 缩短临界区，避免重计算 |
| F-04 | gc task 回收滞后 | 外部长期持有 `Arc` | `EXITED_TASKS` 堆积 | 内存增长 | 2 | `Arc::try_unwrap` 重试，join 语义释放 |
| F-05 | 远端 resched 请求丢失/延迟消费 | IPI 发送失败或 EL1 IRQ 退出前没有安全点 | 被唤醒任务调度延迟 | 吞吐/延迟波动 | 3 | SMP 配置强制启用 IPI；发送 pending（不在 IPI 里 account/refresh 目标 RQ）；探测失败才武装 backup hrtick；记录发送失败；IRQ 完成后挂起 exception 标记以执行尾部补查 |
| F-06 | 算法 `update_current` / `next_preemption_ns` 行为异常 | 调度器实现 bug | 抢占策略失真 | 饥饿/抖动 | 2 | `ksched` 单测覆盖 + trace hook 诊断 |
| F-07 | soft timer deadline 截断为已过期硬件时间 | deadline 超过硬件纳秒表示范围时发生截断 | timer 被立即重复触发 | IRQ 风暴、任务和网络路径停顿 | 1 | typed deadline 贯穿 HAL；backend 使用 checked conversion，超范围钳制到最远可表示期限 |
| F-08 | 周期 callback overrun 后立即再次到期 | deadline 按旧周期逐次追赶或按 IRQ 入口时间重算 | hardirq 连续回调 | CPU 活锁、调度/soft timer 饥饿 | 1 | callback 前临时置 inactive；完成后以 fresh monotonic time + period 一次性重装 |
| F-09 | idle-pull 与远端 wake 双锁顺序颠倒 | dest 持锁再锁 src | 死锁 | 调度停住 | 1 | 只锁 src 再 dest 入队；`resched` 已放下 dest 调度锁 |
| F-10 | idle-pull 窗口 affinity 漏迁 | steal 后 dest 入队前 `setaffinity` | 任务在新掩码外运行 | 隔离违约 | 2 | dest 入队后复查 cpumask；enforce 对 Ready 不在队中间态自旋 |

## 故障管理

- **快速失败策略**：关键不变量处普遍使用 `assert!`（例如任务状态、IRQ 约束、CPU 编号）。
- **延迟恢复策略**：抢占采用 pending + 安全点执行，尽量在一致状态下恢复调度。
- **回收容错策略**：GC 对 `Arc::try_unwrap` 失败重排队，避免误释放。
- **诊断增强**：`snapshot/watchdog` feature 通过共享任务注册表提供锁等待与回溯检查能力。

## 隐私与数据暴露

`ktask` 不直接处理用户隐私数据。可能暴露的信息主要是：

- 任务名、任务 ID、CPU ID（日志与 tracing）
- 回溯信息（snapshot/watchdog 启用时）
- 每 CPU 聚合调度计数（`sched_stat` 启用时通过 `/proc/sched_stat` 暴露，不含任务身份）

这些属于内核诊断输出，受日志通道与构建 feature 控制。

## 已知限制

1. 当前无周期 load-balance 守护线程。跨核依赖 affinity、
   wake `select_idle_sibling`、spawn idle-first + `nr_home`，以及即将 idle 时
   从 `nr_running >= 2` 的核 idle-pull 一颗 Ready `!on_cpu` waiter。
   必须拒绝 `on_cpu`，只锁 src。
   `add_task` 总会 `request_resched`（含 busy 本核），避免动态 timer 已 disarm
   时新 peer 永不被发现。
   EEVDF：唤醒 PLACE_LAG + 完整 request deadline；非自愿抢占先 peer_preempts_curr
   再决定是否 `leave_current(Preempt)`；唤醒提名 NEXT_BUDDY（`curr` 为空也提名，
   覆盖远端 leave→pick 窗口）；
   `with_wake_sync`（futex 与 `Task::interrupt`）可对 eligible buddy sync-preempt
   （仍要求 eligibility，不绕过 EEVDF）；NEXT_BUDDY 与 sync 标记在同一次
   scheduler 锁内武装，避免远端 CPU 在两次加锁之间抽走 buddy。
   运行任务必须经统一 `leave_current` 离开，EEVDF `curr` 不为任务生命周期 owner。
2. `select_run_queue` idle-first（`nr_running == 0`）再比 `nr_home`。`select_wake_run_queue`
   对齐 `select_idle_sibling`（prev 空闲则回家，否则找 `nr_running == 0`；无 idle
   留 prev；home 不在 cpumask 且无 idle 才 `find_idlest`）。不要在 SIS 之前单独加
   「粘 home 的 idle 溢出」。
3. SMP 必须同时启用 `ipi`：远端唤醒的抢占请求通过 IPI 置 `need_resched`；
   `smp && !ipi` 在编译期拒绝。IPI 回调不得 account/refresh 目标 RQ。
4. `on_timer_fire` 在唤醒批次内推迟硬件 rearm，结束后重读 soft earliest；仅
   schedule slot 到期时 `account_sched_tick`（`check_preempt_tick`），否则只
   account。`switch_to` 的立即抢占 pending 落在入场任务上。NOHZ 下 add/wake
   前会 flush 运行实体墙钟时间。
5. `unsafe` 边界仍较多，需持续收敛到更小封装点并补齐 `SAFETY` 说明。

## 审计清单

修改 `ktask` 时建议逐项核对：

- [ ] 新增 `unsafe` 块有清晰 `SAFETY:` 不变量说明。
- [ ] 新增 run queue 访问路径是否保持 guard acquire/release 对称。
- [ ] 新增阻塞/唤醒逻辑是否保持 `TaskState` 转换单调正确。
- [ ] 涉及 `smp` 的路径是否考虑 `on_cpu`/迁移并发窗口。
- [ ] 退出与回收路径是否避免早释放与引用泄漏。
- [ ] `preempt` 开关下行为是否一致（有无 feature 分支遗漏）。
- [ ] 若改动 tick/抢占逻辑，验证不会在临界区中途切换任务。
- [ ] 改 wake handoff / 内存序时按 `docs/design.md` 的 IK9KW6 步骤做 3 路 TCG
      复现，不要用单 VM 连跑或 guest 压力单测代替。
- [ ] 不要在 `on_cpu` 仍为真时把 ready 任务迁到别的 RQ（idle-pull 必须拒绝）。
- [ ] idle-pull 是否只锁 src、经 `steal_ready_task`/`enqueue_task` PLACE_LAG。
- [ ] idle-pull dest 入队后是否复查 cpumask；不含 dest 是否 `migrate_entry`。
- [ ] `setaffinity` 遇到 Ready 但不在任何 RQ 时是否等待再入队，而不是直接 `false`。
- [ ] `nr_running` 是否只在 publish/enqueue 记账、block/migrate/exit 销账。
- [ ] `nr_home` 是否在 block 时保持，仅 exit / 换核时销账。
- [ ] spawn 是否 idle-first（`nr_running == 0`），再比 `nr_home`。
- [ ] wake 是否 prev 空闲回家、否则 `select_idle_sibling`、无 idle 留 prev。
