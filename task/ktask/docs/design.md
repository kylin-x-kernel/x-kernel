# ktask — 设计文档

## 定位

`ktask` 是 x-kernel 的**内核任务管理与调度编排层**：提供任务创建/退出、阻塞与唤醒、每 CPU 运行队列、抢占控制、定时事件驱动，以及（可选）快照与 watchdog 诊断能力。

它不实现具体硬件中断派发（由 `khal`/`kruntime` 负责），也不实现调度算法本体（由 `ksched` 提供），而是把这些能力在运行期组装成统一的任务模型。

目标读者：需要理解任务切换路径、抢占触发时机、SMP run queue 行为，或准备在 `ktask` 增加调度/诊断能力的开发者。

## 背景

x-kernel 将“**调度算法**”与“**任务运行时**”分离：

- `ksched`：提供 FIFO/RR/CFS/EEVDF 等算法 trait 与实体。
- `ktask`：维护任务生命周期、当前任务上下文、每 CPU run queue、切换与阻塞语义。

`ktask` 通过 `kiface` 实现 `kspin::KernelGuardIf`，把 `kspin` 的 guard acquire/release 与任务抢占计数挂接，形成“临界区退出时再检查抢占”的延迟抢占模型。

## 范围

涉及的源文件：

```text
task/ktask/
├── src/
│   ├── lib.rs                # 模块导出、feature 总览
│   ├── api.rs                # 对外 API（spawn/yield/sleep/affinity/exit/tick）
│   ├── task.rs               # TaskInner/CurrentTask/状态与上下文
│   ├── run_queue.rs          # per-CPU run queue、resched、切换、GC task
│   ├── future/
│   │   ├── mod.rs            # block_on、waker 到 unblock 的桥接
│   │   ├── poll.rs           # poll 辅助
│   │   └── time.rs           # sleep timer future
│   ├── wait_queue.rs         # 事件驱动阻塞等待抽象
│   ├── timers.rs             # tick 回调与 timer event 分发
│   ├── tracing_hooks.rs      # 调度 trace hook 注册与触发
│   └── snapshot/             # 可选：任务/CPU 快照与回溯导出
├── Cargo.toml
├── Kconfig
└── docs/
    └── design.md
```

## 架构

```text
kruntime::init_scheduler()/init_scheduler_secondary()
        │
        ▼
┌─────────────────────────────────────────────────────────────────┐
│ per-CPU RUN_QUEUE + Scheduler(ksched) + IDLE_TASK + GC task     │
└─────────────────────────────────────────────────────────────────┘
        │                         │
        │ on_timer_fire()         │ block/wake/exit/yield
        ▼                         ▼
update_current() → pending      blocked_resched / unblock_task / resched
        │                         │
        └─────── 延迟到抢占安全点（enable_preempt）───────┘
                                  │
                                  ▼
                             switch_to()
```

| 组件 | 职责 |
|------|------|
| `TaskInner` / `CurrentTask` | 任务元数据、状态机、上下文、抢占计数 |
| `RunQueue` | 每 CPU 调度容器；维护算法实例与切换过程 |
| `Scheduler` (`ksched`) | `add_task/pick_next/update_current/next_preemption_ns/enqueue_task/leave_current/steal_ready_task` 算法策略（运行任务统一经 `leave_current` 离开；Block/Migrate 记 vlag，唤醒/迁入经 `enqueue_task` PLACE_LAG；idle-pull 经 `steal_ready_task` 再 PLACE_LAG；EEVDF `curr` 仅为数值快照；slice/request 以 ns 计） |
| `future::block_on` | 将 `Future` pending 映射为任务阻塞/唤醒 |
| `WaitQueue` | 事件型阻塞等待 API |
| `timers` | 显式周期回调（与调度 timer 解耦） |
| `snapshot/task_registry` | 可选诊断：snapshot/watchdog/NMI 共享的任务遍历视图 |

## 创建与发布模型

`ktask` 现在显式区分两段用户可见生命周期：

- `prepare_task()`：只把 `TaskInner` 转成 `Arc`，task 还没有进入 run queue；
- `activate_task()`：把 prepared task 放入 run queue，使其变成 runnable；
- `spawn_task()`：保留为通用快捷入口，等价于 `prepare_task() + activate_task()`。

这样需要额外发布步骤的调用方可以先完成 registry 或其它外部可见状态更新，
再让 task 对调度器可见。用户进程创建路径必须遵守这个顺序。

PID 1 不再有特殊的 bootstrap 转换路径：late-init 线程通过
`posix-process::spawn_init_process` 构造一个全新的 `User` 身份任务，runtime 在
`new_user()` 构造时一次性就绪。`UserRuntimeSlot` 因此简化为构造时填充的不可变容器
（`ready()` 建造，`get()` 只读），不再有 `EMPTY -> INSTALLING -> READY` 状态机或
事后 `install_user_runtime()` 补装。页表由调度器在首次 switch-in 时通过
`switch_page_table_root` 自动激活，无需 `activate_current_user_page_table` 这类
"运行中任务热切页表"的特例接口。

## 核心流程

### 1) 初始化流程

主核：

```text
init_scheduler()
  → run_queue::init()
  → 创建 per-CPU idle task（绑定本 CPU）
  → 创建 per-CPU run queue（含 gc task）
  → 初始化 current 为 main task

kruntime::init_interrupt()
  → kirq::softirq::init()
  → ktask::init_softirqd_current_cpu()
  → 创建并激活绑定 boot CPU 的 ksoftirqd/0
  → ktask::init_system_workqueue_worker()
  → 创建并激活绑定 boot CPU 的 kworker/system_wq
  → enable_local_irq()
```

从核：

```text
init_scheduler_secondary()
  → run_queue::init_secondary()
  → 初始化 current 为 idle task
  → 创建本 CPU run queue（含 gc task）

rust_main_secondary()
  → ktask::init_softirqd_current_cpu()
  → 创建并激活绑定当前 CPU 的 ksoftirqd/N
  → enable_local_irq()
```

`ksoftirqd/N` 是 `kirq::softirq::SoftirqDaemonIf` 的 `ktask` provider。`kirq`
只维护 per-CPU softirq pending bit 并在需要退避执行时唤醒当前 CPU daemon；
daemon 任务由 `ktask` 创建、pin 到对应 CPU，并用 IRQ-safe `PollSet` 阻塞等待
唤醒。daemon 在普通任务上下文调用 `kirq::softirq::run_pending_softirqs()`，
用于承接 hardirq-exit/BH-enable 直跑未覆盖或 restart budget 之外的 softirq work。
daemon 每完成一轮实际 softirq work 后会主动 `yield_now()`，再回到外层 pending
检查/等待循环；这对应 Linux `run_ksoftirqd()` 在 `__do_softirq()` 后执行
`cond_resched()` 的调度友好语义。若等待注册因内存压力等原因失败，daemon 也会
先让出 CPU 再重试，避免无 waiter 的紧循环。

`kworker/system_wq` 是 `kirq::workerqueue::WorkerqueueHostIf` 的 `ktask`
provider。`kirq` 拥有 `system_wq` 的队列状态、work 状态和 enqueue/requeue
语义；`ktask` 只提供一个 sleepable task context 来 drain 队列。当前 workerqueue
foundation 只有一个 system worker，因此保持 single-consumer queue invariant。
provider 使用 `PollSet` 等待 `kirq::workerqueue::system_wq_has_runnable_work()`，
唤醒路径可从 hardirq/softirq/BH-disabled context 调用；worker callback 在普通
任务上下文执行，可以睡眠，但不继承排队方的进程上下文。
`WorkerqueueTaskContextIf` 在当前 `TaskInner` 上保存 opaque work key 和 queue key，
供 KIRQ 识别 worker callback 中的 `flush_work(self)` / `cancel_work_sync(self)`
self-wait，以及 `flush_work()` 同一 single-consumer queue 上其它 pending work 的
self-deadlock；这个状态跟随任务迁移，不依赖 per-CPU slot。work key 表示 KIRQ
`WorkItem` 的底层 allocation identity；`ktask` 不拥有 work 生命周期，也不保存 work
handle。当前 provider 只保存单层 context，因此 M4 的 `run_one_work()` 不支持
callback 内嵌套 drain。
`WorkqueueSyncWaitIf` 复用 ktask 的 completion wait helper；`kirq` 在进入 provider
前完成 context/self-wait gate，provider 只负责 `try_wait/register/recheck` 阻塞协议。

### 2) 调度与切换

主动切换路径：

- `yield_now()` → `yield_current()` → `resched()`
- `exit()` → `exit_current()` → `resched()`
- `blocked_resched()`（阻塞）→ `resched()`

`resched()` 在当前 CPU 的调度器上 `pick_next_task()`；无就绪任务时先做 idle-pull
（从 `nr_running >= 2` 的核偷 Ready 且 `!on_cpu` 的 waiter），再回退到本 CPU `IDLE_TASK`。

### 3) 硬件定时器驱动与动态 schedule timer

`ktask` 拥有全部硬件定时器驱动逻辑：`kruntime` 只注册时钟中断向量，中断处理直接调用
`ktask::on_timer_fire()`。调度与 soft timer 共用每 CPU oneshot 硬件，按绝对 ns deadline
仲裁，**不再**依赖固定 `TICKS_PER_SECOND` 调度节拍：

1. IRQ 入口先调用 `register_timer_irq_note` 钩子（watchdog 硬锁心跳按任意
   timer IRQ 计，不只 4s sample），再按 `now - last_accounted_ns` 对当前任务
   `update_current(elapsed_ns)`（idle 跳过；只在 request 用完时要求 resched）。
   **仅到期的 schedule slot** 再跑 Linux `entity_tick` / `check_preempt_tick`：
   pick 会变（eligible NEXT_BUDDY，或更早 deadline）才置 `need_resched`。
   soft/periodic IRQ 只记账，避免 WF_SYNC 刚换上 later-deadline wakee 就被抢回。
   ineligible buddy 由 `next_preemption_ns` 武装 until-eligible，不对其它
   waiter 做 10µs 轮询。IRQ 尾探测抢不过仍立即返回。
2. 运行到期的显式周期回调（`register_timer_callback(period, ...)`）；唤醒批次内推迟
   硬件 rearm，排空 soft-timer wheel 并 wake，再重读 soft earliest。
3. 用 `next_preemption_ns(current)` 重算可选的 `NEXT_SCHED_DEADLINE`，再
   `rearm_local_timer` 到

```text
min(next_sched_deadline?, earliest_soft?, earliest_periodic?)
```

   三者皆空时 `disarm_timer()`。过期/陈旧 IRQ 只补账并重装，不做 catch-up tick 循环。
   在 `sched_stat` 下按是否命中 schedule / soft / periodic deadline 分类计数
   （`timer_irq_sched` / `timer_irq_soft` / `timer_irq_periodic` / `timer_irq_stale`），
   便于核对取消固定 10ms tick 后的 IRQ 构成；分类在 drain/run 之后从 schedule slot、
   非空 soft wakers、`run_due_and_earliest` 的 due 标志推导，默认 release 不额外查询
   timer wheel。

schedule deadline 经 `program_sched_deadline` 写入：对齐 Linux `hrtick_start`，
正的相对延迟下限 10μs；`next_preemption_ns == 0` 或 slot 已到期时先打
`need_resched` 并消费该 one-shot schedule slot，绝不武装硬件 interval=0，也不把
已到期 slot 每 10μs 循环后推（避免 timer IRQ 活锁）。该下限只作用于 schedule 源，
不影响 soft/periodic 精度；pending 抢占在临界区退出后的安全点消费。

该路径 soft deadline 以 `MonotonicInstant` 贯穿 timer wheel 到
`rearm_local_timer`；装入硬件前再转换为绝对 ns / ticks。上下文切换、
block/exit/migrate、优先级变更与本地 `request_resched` 也会补账并刷新
schedule deadline。远端 wake 的 IPI 只置 `need_resched`（与改周期 tick 之前相同）；
IRQ 尾 `peer_preempts_curr` 决定是否切换，探测失败才在 `preempt_resched` 里武装
剩余 request 的 backup hrtick。不要在 IPI 里 `account`/`refresh` 目标 RQ：那会与
waker 持有的远端 `&mut RunQueue` 别名，并把 schedule slot 重编程或 disarm，导致
busy home 上空等一整段 request。
由于 oneshot/NOHZ CPU 可能已彻底停表，`smp` 不带 `ipi` 无法可靠推进远端 runnable
任务，`ktask` 在编译期拒绝该配置，不提供虚假的本地 fallback。

`register_timer`/`sleep_until` 在关抢占下读取 `this_cpu_id`、入队，并（仍持有 wheel 的
`SpinNoIrq` 锁、IRQ 关闭）立即调 `rearm_local_timer()`；`cancel_timer`/`TimerFuture::drop`
在本地移除最早项时同样重装。远程取消最早项不发 IPI，由对方 CPU 至多一次陈旧 IRQ 自纠正。

`on_timer_fire` 在唤醒批次内推迟硬件 rearm（`DEFER_LOCAL_TIMER_REARM`），避免每次 wake
用过期 soft earliest 覆盖刚刷新的 schedule slot；批次结束后重读 soft earliest 再仲裁。
`switch_to` 经 `program_sched_deadline_for(incoming, …)` 写入：立即请求的 pending 落在
入场任务上（此时 `current()` 仍是出场任务）。NOHZ 下 lone runner 墙钟时间在 add/wake/
priority 变更前经 RQ `flush_running_runtime` 注入 EEVDF `curr`，避免 PLACE_LAG 用陈旧 V。
`last_accounted_ns` 只在持有该 RQ 的 scheduler `SpinRaw` 时更新，这样远端 wake flush
与本地 timer `account_current_runtime` 不会对同一段墙钟重复或漏记。

显式周期回调在 hard IRQ/本地 IRQ 关闭上下文执行，必须短小且不阻塞。回调调用前释放
callback vector 的 Rust 借用并以 `Arc` 固定闭包生命周期，因此回调内追加注册安全；
每次 deadline 从**回调完成时刻 + period** 重算，超期只执行一次，不 catch-up、不立即
重入。注册新周期源时重装仍携带当前 soft deadline，不能把已有 sleep/timeout 推迟；
超出 `u64` ns 的 `Duration` 饱和到最远期限而不是截断成短周期。

`ktask` 仍采用延迟抢占：timer 路径通常只打 pending，真正 `preempt_resched()` 在抢占
重新允许的安全点触发。

### 4) 抢占与 kspin 的接口耦合

`kspin` guard acquire/release 会调用 `KernelGuardIf::disable_preempt/enable_preempt`。`ktask` 提供该接口实现：

- `disable_preempt()`：当前任务 `preempt_disable_count += 1`
- `enable_preempt(true)`：计数降为 0 时检查 `need_resched`；若当前 CPU 不在异常/IRQ
  trapframe 作用域内，才进入 `preempt_resched()`

该设计将“临界区边界”与“抢占检查点”绑定，避免在持锁/临界区中途切换任务。异常/IRQ
入口通过 `in_exception_context()` 暴露当前 CPU 是否仍在 trapframe guard 作用域内；在该
guard 尚未释放时，抢占只保留 `need_resched` pending，不实际切换任务，避免普通任务观察到
仍属于异常路径的 per-CPU trapframe。

IRQ 分发完成并向中断控制器回写完成状态后，`khal` 在释放 IRQ handler 的 `NoPreempt`
guard 前暂时挂起 active-exception 标记。这样 `enable_preempt` 可在 IRQ 尾部、IRQ 仍关闭
的安全窗口消费 pending；被中断任务恢复后再还原标记并返回异常。若不做该尾部补查，
IPI wake 在 EL1 exception 内只会留下 pending，可能一直等到下一次 schedule timer
或安全点。

异常路径可能进入会阻塞的后端（例如用户缺页处理），任务也可能在 trap handler
返回前被调度到其它 CPU。为避免原 CPU 上的 active-exception slot 变成 stale 状态，
`switch_to()` 在切离当前任务、更新 `CurrentTask` 之前会挂起并清空当前 CPU 的
active exception context；当该任务被切回并从底层上下文切换返回时，再把挂起的
active exception context 恢复到当前 CPU。否则旧 CPU 会一直认为自己仍在异常上下文，
或迁移后的任务在 trap handler 后半段错误允许抢占。

### 5) 阻塞/唤醒与 WaitQueue/Future

- `future::block_on` 在 `Poll::Pending` 下走 `blocked_resched()`，把当前任务置为 `Blocked`。
- 若 waker 在 `Poll::Pending` 与提交阻塞之间触发，`block_on` 清除 wake 标志后立即重新 poll；不能先 yield，否则满载 CPU 会把已完成的 wake-before-block 竞态转换为无关 runnable task 的排队延迟。
- waker 触发时通过 `select_wake_run_queue(...).unblock_task(..., true)` 将任务恢复为 `Ready`。
- SMP 下唤醒按 Linux `select_idle_sibling`：`prev_cpu`（`task.cpu_id()`）空闲则回家；否则在 cpumask 里找 `nr_running == 0` 的核；都忙则留在 prev；home 不在 cpumask 且无 idle 时才走 `find_idlest_cpu`。
- `unblock_task(..., true)` 对本 CPU 设置 `need_resched`；对远端 CPU 在 `ipi + preempt` 可用时请求远端设置 `need_resched`。
- `WaitQueue` 基于 `event_listener` 封装等待与通知，支持超时与条件等待。
- `TaskInner::join()` 使用 `kpoll::Completion` 作为 per-task exit wait source。任务退出时先发布
  `exit_code`，再把 state 切到 `Exited` 并 `complete_all()`；joiner 的真实完成条件仍是
  `TaskState::Exited`，completion 只负责避免丢失 wake 并支持 late joiner。
- `ktask` 通过 `kirq::IrqSyncWaitIf` 提供 completion-backed blocking wait，使
  `kirq::synchronize_irq()` / `free_irq()` 能阻塞当前 task；IRQ descriptor 生命周期和
  `in_flight` predicate 仍由 `kirq` 拥有。

### 6) 退出回收（GC task）

每 CPU run queue 在创建时自动加入一个 `gc` 任务，循环执行 `poll_gc`：

- 消费 `EXITED_TASKS` 列表
- `Arc::try_unwrap` 成功则立即回收
- 否则放回队列等待外部引用释放
- 通过 `WAIT_FOR_EXIT` waker 休眠/唤醒

这是一种延迟回收模型，避免在退出与切换关键路径直接执行慢释放。
`WAIT_FOR_EXIT` 保持 `PollSet` 事件语义，因为同一 CPU 的 GC task 会连续观察多批退出任务；
它不同于单个 task 的 sticky exit completion。

## SMP 语义

- 每 CPU 一个 `RUN_QUEUE`、`IDLE_TASK`、`EXITED_TASKS`、`WAIT_FOR_EXIT`。
- `task.cpu_id` 表示 **runqueue ownership**，不是业务路径各自维护的辅助字段：
  - runnable / running：任务当前归属的 run queue CPU
  - blocked：保留上一次归属 CPU，供 `select_wake_run_queue` 做 wake affinity
- 所有 ownership 更新收敛到 `RunQueue` 封装入口，调用方不得直接 `Scheduler::add_task` /
  `enqueue_task` / `leave_current` 或散落调用 `set_cpu_id`：
  - `publish_task`：新任务首次入队（`spawn` / per-CPU gc）
  - `enqueue_task`：非当前 Ready 任务入队（unblock / affinity migrate-in / idle-pull）
  - `leave_current`：当前运行任务离开执行槽（Yield / Preempt / Block / Migrate / Exit）
  - `switch_to_local`：不经 ready 队列、直接 `switch_to` 的本地 helper（如 migration task）
  - `set_owner_cpu`：仅用于 run queue 尚未建立时的 boot bring-up（main / idle）
- `switch_to` 在 SMP 下检查 `next_task.cpu_id() == rq.cpu_id`，防止错队列切换。
- `select_run_queue` 对齐 Linux fork 的 `find_idlest_cpu`：**idle-first**
  （`nr_running == 0`，睡觉的核也算 idle），再比 `nr_home`。SIS 已验证后，
  clone 落到 sleeper 核上安全——被挤走的 worker 下次还能找 idle。空闲核之间仍
  优先 `nr_home` 更低的（空核优于有人睡觉的核）。并列 `prefer_local`，再 RR。
- `select_wake_run_queue` 对齐 Linux `select_idle_sibling`：prev 空闲则回家，
  否则找一颗 `nr_running == 0` 的核。曾经「始终粘 home」时，home 忙 → idle
  溢出会把 schbench 锁成 ~3 路（RPS ~450→~325）；SIS 下被挤走的 worker 下次
  仍可再找 idle（抢椅子，平均仍约 4 路）。无 idle 时留在 prev。
- 当前无周期 balancer。跨核除 affinity 与上述 wake SIS 外，即将 idle 时
  做 Linux 风格 idle-pull：只从 `nr_running >= 2` 的核偷 **Ready && `!on_cpu`**
  的 waiter，经源 RQ `steal_ready_task`（内部 `remove_task`，记 PLACE_LAG）再
  本核 `enqueue_task`。只锁 src，再锁 dest 入队，避免 dest-then-src 与远端 wake
  死锁。入队后复查 cpumask：steal 到 enqueue 之间 affinity 可能已排除 dest，
  不含本核则立刻 `migrate_entry`，不在非法 CPU 上 pick。曾经把仍 `on_cpu` 的
  任务偷到另一核（yield/preempt 入树到 `switch_to` 完成之间），两核同跑一份栈；
  现必须拒绝 `on_cpu`。steal 会改 `cpu_id`，SIS 下这是预期的抢椅子：下次唤醒
  prev 空闲则回家，否则再找 idle。
  spawn / `activate_task` / affinity migrate-in 仍走 `find_idlest_cpu`。
- `add_task` 首次入队后总是 `request_resched`（远端 IPI / 本地 pending 并刷新
  schedule deadline）。动态 timer 在 lone task 时已 disarm，busy 本核也不能依赖不存在的
  周期 tick 日后发现新 peer。
- EEVDF（`ksched`）对齐 Linux `pick_eevdf` / `place_entity`：
  - 唤醒 `PLACE_LAG`：`vruntime = V - lag`（lag 先按 `(W+w)/W` 膨胀），deadline 用**完整 request**
    （`vd = ve + r/w`，request 以 ns 计，默认 `DEFAULT_SLICE_NS` = 2ms），不造假短 deadline，
    也不把 wakee 额外钳到 V / `min_vruntime`（负 lag 允许暂时 ineligible）；
    `min_vruntime` 按 Linux `update_min_vruntime` 计入离树 `curr`；
  - **非自愿** `preempt_resched` 先 `peer_preempts_curr()`（`curr` 离树比 deadline），
    同伴更早或 WF_SYNC 的 eligible buddy 才 `leave_current(Preempt)`+`pick`；否则清 pending
    直接返回（避免无意义再入队，也保证 `switch_to` 前 ready 队列持有 prev 的 `Arc`）。
    探测失败路径在同一次 scheduler 锁内完成 account + probe + `next_preemption_ns`，
    出锁后再 `program_sched_deadline` / rearm，避免三次加锁拉长关中断窗口；
  - `update_current` 对齐 Linux `update_deadline`：request 完成后赋新 `vd = ve + r/w`，
    仅在有等待同伴时 resched；lone task 不装 schedule timer；
  - 唤醒按 Linux `NEXT_BUDDY` / `set_next_buddy` 提名 wakee（保留更早
    deadline 的既有 buddy），**不要求** `curr` 已安装：远端 futex 常在
    目标 `leave` 之后、`pick` 之前入队；`curr` 为空时跳过提名，随后 pick
    会把刚放回的 runner 再跑完一段 request（schbench p99.9 ≈ 2ms）。
    `pick` 优先 eligible buddy；WF_SYNC 再设 `prefer_sync_buddy`；
  - futex 等路径用 `with_wake_sync`（Linux `WF_SYNC`）：eligible next buddy 可
    sync-preempt 半截 curr（即使 deadline 更晚）。`mark_sync_wake_preempt` 当时
    就设 `prefer_sync_buddy`，且失败的 `peer_preempts_curr` 不得清掉 sync 标记，
    否则远端 slice-expire pick / 随后 IPI 探测看不到 buddy。account + PLACE +
    提名必须在同一把 scheduler 锁里完成。
  - `TaskInner::interrupt()` 对 `interrupt_waker` 走 `with_wake_sync`（信号发送方
    随后往往会 block）。
- 当前任务离开统一走 `leave_current`：
  - `yield_current` → `Yield`（重置 request 并再入队；`Running -> Ready` 必须成功）
  - `preempt_resched` → `Preempt`（保留剩余 slice 并再入队；同上）
  - `blocked_resched` → `Block`（记 vlag，不入队；into_raw 当前槽位计 1，调用方须另持强引用以满足 `strong_count > 1`；缺则 panic，见 `# Panics` 与 unittest）
  - `migrate_current` → `Migrate`（源 RQ 记 vlag 供目的端 PLACE_LAG，不入队）
  - `exit_current` → `Exit`（清除 current 记账，不设置 PLACE_LAG）
- 所有离开路径（含 `yield_current`）均在 `leave_current` / requeue **之前**按当前时间
  `update_current`；`switch_to` 不再对已经入树的 prev 补账，避免修改有序队列 key 后破坏
  调度器索引与权重聚合。
- idle 不进入调度器 `curr`：yield/preempt 只做 `Running -> Ready`，不
  `leave_current` / 不入队。EEVDF 下 idle 保持 `curr == None`，`peer_preempts`
  在空 `curr` 时把「ready 非空」视为可抢占即可。
- 经 `switch_to_local` 上 CPU 的助手在入场时 `sync_running_curr`；`preempt_resched`
  只探测、不再 sync（避免把 idle 写进 `curr`）。
- EEVDF `curr` 只保存 identity/vruntime/deadline/weight 快照，不持有任务 `Arc`；
  `pick_next` 要求此前已 `leave_current`，不再静默擦除陈旧状态。
- `set_task_affinity`：写 `cpumask` 后 `enforce_affinity_placement`——current 立即
  `migrate_current`；ready 从旧 RQ `remove_task` 再 `migrate_entry`（目的 RQ 入队后
  `request_resched_on`，与 `add_task` 相同）；idle-pull 在 steal 与 dest 入队之间
  会出现 Ready、不在任何队列、`cpu_id` 仍为源 RQ 的窗口，此时 `remove_task` 会 miss，
  `enforce_affinity_placement` 短暂自旋等到 dest 入队或任务变成 Running，再按 ready /
  running 路径迁移（失败仍仅限远端 running 迁不走）。远端 running
  在 `preempt`+`ipi` 下 `request_resched`，由 `preempt_resched`/`yield_current`
  发现 mask 不含本 CPU 后强制迁移，调用方自旋等到离开非法 CPU；迁不走返回
  `false`（syscall 侧 `EBUSY`）。idle-pull dest 入队后复查 cpumask，不含本核则
  立刻 `migrate_entry`，不在非法 CPU 上 pick。
- 对 `Blocked` 任务重新入队时，SMP 唤醒方先发布目标 RQ 与 wake flags；若
  `task.on_cpu()` 仍为真，则由原 CPU 在 switch-out 清零 `on_cpu` 后原子认领并完成入队。
  该 handoff 防止重复入队，也避免唤醒方持 `NoPreemptIrqSave` 关中断自旋等待远端 CPU。
  `arm_wake_enqueue` / `on_cpu` / `set_on_cpu(false)` / `take_wake_enqueue` 四处
  均为 SeqCst，并且在 store 与对侧 load 之间加 `SeqCst` fence（x86 `MFENCE`，
  AArch64 `DMB ISH`）。这是两变量 Dekker 的内存模型要求，不是模拟器兜底：
  SeqCst store 再 SeqCst load **另一个**地址并不构成 store→load 屏障
  （C++20 P0668，Rust 跟随）。Release/Acquire 在真机上就会双边漏看；去掉
  fence 后 LLVM 可把 SeqCst 降为普通 store/load，x86 TSO 同样允许漏看。
  不可把 fence 降为 acquire/release（x86 上只有 SeqCst fence 会发出 `MFENCE`）。
  QEMU TCG（IK9KW6）只是最先观测到失效的环境；任务会停在 `Ready` 却不在任何
  run queue。
  现有 unittest `wake_of_switching_task_defers_enqueue_without_spinning` 只覆盖
  单线程 Deferred 路径，打不出跨 CPU store-buffering。guest 内再写 block/wake
  对打也不能在无 fence 时被预期为超时：IK9KW6 单 VM 连跑 20 次全过，要靠宿主机
  上多 VM 并发拉长 TCG 交错窗口。确定性回归因此记手工复现，不塞一条假绿压力单测。

  **IK9KW6 手工复现（aarch64 QEMU TCG，`NR_CPUS=4`）**

  1. 并行启动 3 个隔离 TCG VM，各跑 harness `process-ipc-smoke,getrusage-reentry`
     （不要串行、不要单 VM 连刷）。
  2. 无 `SeqCst` fence 时，首轮即可有 VM 卡在
     `shm_deadlock::shm_deadlock_shmget_vs_shmat`：现场
     `state=Ready`、`on_cpu=0`、`wake_enqueue_flags=0x05`（`PENDING|RESCHED`），
     原 CPU 跑 idle，后续 wake 不再走 `Blocked→Ready`。
  3. 对照：同一二进制 KVM、或仅在 store 与对侧 load 之间保留 `SeqCst` fence，
     24/24 通过。完整撤掉 wake handoff 同样 24/24，但会回到 IRQ-off 自旋。
- 远端 run queue 唤醒/首次入队都不会直接切换远端 CPU；在支持 IPI 和抢占时，通过远端 pending-resched 请求把切换推迟到远端安全点。
- `TaskInner::on_cpu_mask()` 现在只表示 `ktask` 自己的调度驻留快照；用户地址空间
  的 TLB shootdown 目标集合由 `memspace::MmCpuResidency` 持有。当前实现只在
  switch-in 时保守聚合设置该 mask，允许 over-target，但不保证在 switch-away
  后立即回收旧 CPU footprint。

## 调度点模型（何时可能发生切换）

| 场景 | 是否立即切换 |
|------|--------------|
| `yield_now()` | 是（主动 `resched`） |
| `blocked_resched()` | 是 |
| `exit_current()` | 是 |
| schedule timer 触发 `update_current` | 否（通常先设置 pending） |
| 本 CPU waker 唤醒任务 | 否（设置 pending，安全点抢占） |
| 远端 waker 唤醒任务 | 否（可通过 IPI 请求远端 pending 并刷新 schedule deadline） |
| `add_task` / `migrate_entry`（本地 busy/idle 或远端） | 否（`request_resched`；远端 IPI） |
| `enable_preempt` 且 `need_resched` | 可能（触发 `preempt_resched`） |

因此 `ktask` 不是“固定节拍必切换”模型，而是“动态 schedule timer + 安全点执行”的抢占模型。

## Cargo Features

| Feature | 作用 |
|---------|------|
| `preempt` | 启用抢占逻辑与 `kspin` preempt 接口 |
| `smp` | 启用多核 run queue 与迁移相关语义；必须同时启用 `ipi` |
| `sched_fifo` | FIFO 协作式调度 |
| `sched_rr` | RR 抢占式调度（隐含 `preempt`） |
| `sched_cfs` | CFS 抢占式调度（隐含 `preempt`） |
| `sched_eevdf` | EEVDF 抢占式调度（隐含 `preempt`） |
| `snapshot` | 任务快照与回溯基础能力 |
| `watchdog` | watchdog 诊断（依赖 `snapshot`） |
| `ipi` | SMP 远端唤醒/重调度的必需能力 |
| `tls` | 可选扩展能力 |

`UserTaskRuntime` 是 `ktask` 的用户运行时接口。其 scheduler hook 可在关闭抢占的切换上下文
调用，因此实现不得阻塞、递归调度或依赖普通的 current-process 上下文。`TaskIdentity::User`
将 `PidHandle` 与 `UserRuntimeSlot` 放在同一分支；任何用户 task（包括 PID 1 init）都由
`TaskInner::new_user(..., user_runtime)` 在构造时一次性填充 runtime，不存在事后补装路径。
内核 task（`KernelThread` / `Internal`）不带 runtime。需要用户执行上下文
的 unittest 通过正常的 `TaskInner::new_user` 构造并启动独立用户 task；测试完成后由调用方等待
该 task 退出，不再向 non-user task 临时注入 runtime。

调度算法默认由 Kconfig 选择（默认 EEVDF）。

## 亚 tick / 竞争唤醒测试约定

`future/time.rs` 中的 unittest 拆成两类：

- **Idle**：无同 CPU 自旋负载，断言 median `< 6ms`，验证 soft-timer 不会被取整到旧的
  10ms tick。
- **Contended（EEVDF）**：同 CPU `spin_loop` spinner，上限为
  `request + DEFAULT_SLICE_NS + 2ms`。这测的是调度唤醒延迟预算，不是 timer 取整。

默认 request 长度见 `ksched::DEFAULT_SLICE_NS`（2ms）。
RR 不共用这个 EEVDF request：其历史 quantum 由
`ksched::DEFAULT_RR_SLICE_NS` 保持为 50ms，避免把 RR 上下文切换率放大 25 倍。

## 设计决策

### 为何采用 per-CPU run queue

降低全局锁竞争与缓存抖动；调度路径以本 CPU 局部状态为主，便于 SMP 扩展。

### 为何 tick 不直接切换

中断处理主体与临界区中直接切换会放大并发复杂度。调度延迟到 `enable_preempt` 安全点；
IRQ handler 仅在设备完成、active-exception 标记已暂时挂起后提供一个尾部安全点。

### 为何唤醒路径优先选择当前 CPU

wait/future 唤醒通常发生在释放资源或发送事件的线程上下文中。若目标任务 affinity 允许当前 CPU，将其放入当前 run queue 可减少跨核唤醒延迟，并让 `need_resched` 在本 CPU 的抢占安全点生效；当 affinity 不允许时仍回退到普通选队逻辑。

### 为何引入 `KernelGuardIf` 接口

`kspin` 作为基础库不应直接依赖 `ktask`。通过接口反向注入，打破潜在循环依赖并保持模块边界清晰。

### 为何使用 GC task 回收退出任务

任务退出时可能仍被 joiner 或切换路径持有引用；延迟回收可避免 use-after-free 与路径上的慢释放开销。

## 与 `kruntime` 的边界

- `kruntime` 负责注册时钟（及 IPI/PMU）中断向量；定时器中断处理只调用 `ktask::on_timer_fire()`。
- `ktask` 负责全部硬件定时器驱动：动态 schedule deadline、soft timer wheel、显式周期回调、
  硬件重装，并将到期事件转换为调度记账、抢占请求、定时事件唤醒。
- `kruntime` 不感知具体调度算法；`ktask` 不负责硬件 IRQ 路由。

## 调度统计

启用 `sched_stat` feature 后，`ktask` 维护每 CPU 基础计数（选核、唤醒、
本地/远端 resched、tick/抢占跳过、switch）。`/proc/sched_stat` 提供进程上下文
快照；EEVDF 构建额外输出 `wake_handoff` / `wake_sync_preempt` 等算法计数。
watchdog 触发时调用 `dump_sched_stats()` 做不分配内存的基础输出。

## 冗余与过载

- **冗余设计**：每 CPU 独立 idle/gc 与 run queue，避免单点队列。
- **过载控制**：由算法层（`ksched`）与任务阻塞模型共同承担；`ktask` 在
  spawn / affinity migrate-in 上做 `find_idlest_cpu` 选队，即将 idle 时
  idle-pull 一颗 Ready `!on_cpu` waiter。没有周期 load-balance 守护线程。
