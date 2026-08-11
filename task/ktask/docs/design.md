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
task_tick() → set_preempt_pending   blocked_resched / unblock_task / resched
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
| `Scheduler` (`ksched`) | `add_task/pick_next/task_tick/put_prev/account_sleep` 算法策略（EEVDF 阻塞前 `account_sleep` 记 vlag；放置用的系统虚拟时间 V 含已离队的当前运行任务 curr） |
| `future::block_on` | 将 `Future` pending 映射为任务阻塞/唤醒 |
| `WaitQueue` | 事件型阻塞等待 API |
| `timers` | tick 回调与定时事件检查 |
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

### 2) 调度与切换

主动切换路径：

- `yield_now()` → `yield_current()` → `resched()`
- `exit()` → `exit_current()` → `resched()`
- `blocked_resched()`（阻塞）→ `resched()`

`resched()` 在当前 CPU 的调度器上 `pick_next_task()`；无就绪任务时回退到本 CPU `IDLE_TASK`。

### 3) 硬件定时器驱动与时钟 tick

`ktask` 拥有全部硬件定时器驱动逻辑：`kruntime` 只注册时钟中断向量，中断处理直接调用
`ktask::on_timer_fire()`。`on_timer_fire()` 采用 tick 限定的 NOHZ 风格：

1. 每次硬件触发都执行 `timers::check_events()`：分发 tick 回调、排空定时 future 的
   timer wheel（两者都是 ns 驱动，与触发频率无关）。
2. **仅**当周期 tick 截止时间已到（或首次触发的懒初始化）时，才调用
   `scheduler_timer_tick()` → 算法 `task_tick(current)`，并把下一个 tick 截止时间推进
   `now + PERIODIC_INTERVAL`。调度器按 tick 计数（slice/vruntime 以 tick 为单位），
   因此子 tick 的 soft-timer 触发不会调用 `task_tick`，保证调度记账仍以固定 `TICKS_PER_SECOND`
   速率推进。
3. 重新装填硬件定时器到 `min(下一个周期 tick, wheel 最早 soft deadline)`。

该路径从 timer wheel 到 `MonotonicTimerIf::arm_timer` 始终传递 `MonotonicInstant`。
具体 timer backend 仅在写 SBI、APIC 或体系结构 timer 寄存器前把 typed deadline
钳制并转换为硬件 ticks。周期 deadline 的 per-CPU 槽直接保存
`Option<MonotonicInstant>`，不使用整数编码或特殊数值作为未初始化哨兵。

子 tick 精度的关键：`register_timer`/`sleep_until` 在入队后（仍持有 wheel 的 `SpinNoIrq`
锁、IRQ 关闭）立即调 `rearm_local_timer()` 把硬件截止时间拉到新的最早 deadline；
`cancel_timer`/`TimerFuture::drop` 在本地 CPU 移除最早项时同样重装。远程 CPU 上取消最早项
不发 IPI，由对方 CPU 下次（已过期的）触发自纠正（至多一次多余中断）。

`ktask` 采用延迟抢占：tick 里通常只打 pending 标记，真正 `preempt_resched()` 在抢占重新允许的安全点触发。

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
IPI wake 在 EL1 exception 内只会留下 pending，可能一直等到下一个周期 tick。

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
- SMP 下唤醒优先入任务阻塞时保留的 owner CPU（`task.cpu_id()`）；若 cpumask 已不含该 CPU，则回退到普通 `select_run_queue` 轮询选队。
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
  `put_prev_task` 或散落调用 `set_cpu_id`：
  - `publish_task`：新任务首次入队（`spawn` / per-CPU gc）
  - `enqueue_task`：yield / preempt / unblock / affinity migrate 重新入队
  - `switch_to_local`：不经 ready 队列、直接 `switch_to` 的本地 helper（如 migration task）
  - `set_owner_cpu`：仅用于 run queue 尚未建立时的 boot bring-up（main / idle）
- `switch_to` 在 SMP 下检查 `next_task.cpu_id() == rq.cpu_id`，防止错队列切换。
- `select_run_queue` 依据任务 `cpumask` 在允许 CPU 集内做轮询选队。
- `select_wake_run_queue` **始终粘 home**（cpumask 不含 home 时才 fallback 轮询）。
  任何 “home 忙 → idle” 溢出都会把 schbench RPS 从 ~450 打到 ~325。
- `add_task` 首次入队后：若目标是远端 CPU，或本 CPU 当前为 idle，则 `request_resched`
  （远端 IPI / 本地 pending）；本核忙碌时不额外 kick，交给 tick/yield。避免 RR 到远端
  idle 后卡在 WFI，进而把 sticky wake 锁进坏布局。
- EEVDF（`ksched`）对齐 Linux `pick_eevdf` / `place_entity`：
  - 唤醒 `PLACE_LAG` 后 vruntime **钳到系统 V**，deadline 用**完整 request**
    （`vd = ve + r/w`），不再造 1-tick 假短 deadline；
  - **非自愿** `preempt_resched` 先 `peer_preempts_curr()`（`curr` 离树比 deadline），
    仅当同伴更早才 `put_prev`+`pick`；否则清 pending 直接返回（避免无意义
    再入队，也保证 `switch_to` 前 ready 队列持有 prev 的 `Arc`）；
  - `task_tick` 仅在有等待同伴时对到期 deadline 抢占；
  - 唤醒到 busy rq 时按 Linux `NEXT_BUDDY` / `set_preempt_buddy` 提名 wakee
    （保留更早 deadline 的既有 buddy）；waker block 后 `pick` 优先 eligible buddy；
  - futex 等路径用 `with_wake_sync`（Linux `WF_SYNC`）：eligible next buddy 可
    sync-preempt 半截 curr（即使 deadline 更晚），缩短 “等 waker block” 的 p50，
    仍不造 1-tick 假短 deadline / idle-seeking。
- `exit_current` / `migrate_current` 在离开源 RQ 前调用 `account_sleep`，清除 EEVDF
  `curr` 并（迁移场景）为目的端 PLACE_LAG 保存 `vlag`；`pick_next` 另有防御性清除。
- 当前实现无主动负载均衡器；任务跨核迁移主要由 affinity 变化触发。
- `set_task_affinity`：写 `cpumask` 后 `enforce_affinity_placement`——current 立即
  `migrate_current`；ready 从旧 RQ `remove_task` 再 `migrate_entry`；远端 running
  在 `preempt`+`ipi` 下 `request_resched`，由 `preempt_resched`/`yield_current`
  发现 mask 不含本 CPU 后强制迁移，调用方自旋等到离开非法 CPU；迁不走返回
  `false`（syscall 侧 `EBUSY`）。
- 对 `Blocked` 任务重新入队时，SMP 下会等待 `task.on_cpu()` 清零，避免与远端 CPU 的切换过程并发冲突。
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
| tick 触发 `task_tick` | 否（通常先设置 pending） |
| 本 CPU waker 唤醒任务 | 否（设置 pending，安全点抢占） |
| 远端 waker 唤醒任务 | 否（可通过 IPI 请求远端 pending） |
| `add_task` 到远端 / 本核 idle | 否（`request_resched`；远端 IPI） |
| `enable_preempt` 且 `need_resched` | 可能（触发 `preempt_resched`） |

因此 `ktask` 不是“每个时钟中断必切换”模型，而是“tick 驱动 + 安全点执行”的工程化抢占模型。

## Cargo Features

| Feature | 作用 |
|---------|------|
| `preempt` | 启用抢占逻辑与 `kspin` preempt 接口 |
| `smp` | 启用多核 run queue 与迁移相关语义 |
| `sched_fifo` | FIFO 协作式调度 |
| `sched_rr` | RR 抢占式调度（隐含 `preempt`） |
| `sched_cfs` | CFS 抢占式调度（隐含 `preempt`） |
| `sched_eevdf` | EEVDF 抢占式调度（隐含 `preempt`） |
| `snapshot` | 任务快照与回溯基础能力 |
| `watchdog` | watchdog 诊断（依赖 `snapshot`） |
| `ipi` / `tls` | 可选扩展能力 |

`UserTaskRuntime` 是 `ktask` 的用户运行时接口。其 scheduler hook 可在关闭抢占的切换上下文
调用，因此实现不得阻塞、递归调度或依赖普通的 current-process 上下文。`TaskIdentity::User`
将 `PidHandle` 与 `UserRuntimeSlot` 放在同一分支；任何用户 task（包括 PID 1 init）都由
`TaskInner::new_user(..., user_runtime)` 在构造时一次性填充 runtime，不存在事后补装路径。
内核 task（`KernelThread` / `Internal`）不带 runtime。需要用户执行上下文
的 unittest 通过正常的 `TaskInner::new_user` 构造并启动独立用户 task；测试完成后由调用方等待
该 task 退出，不再向 non-user task 临时注入 runtime。

调度算法默认由 Kconfig 选择（默认 EEVDF）。

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
- `ktask` 负责全部硬件定时器驱动：周期 tick 簿记、timer wheel 排空、硬件重装，并将 tick
  转换为调度记账、抢占请求、定时事件唤醒（含子 tick 精度）。
- `kruntime` 不感知具体调度算法；`ktask` 不负责硬件 IRQ 路由。

## 调度统计

启用 `sched_stat` feature 后，`ktask` 维护每 CPU 基础计数（选核、唤醒、
本地/远端 resched、tick/抢占跳过、switch）。`/proc/sched_stat` 提供进程上下文
快照；EEVDF 构建额外输出 `wake_handoff` / `wake_sync_preempt` 等算法计数。
watchdog 触发时调用 `dump_sched_stats()` 做不分配内存的基础输出。

## 冗余与过载

- **冗余设计**：每 CPU 独立 idle/gc 与 run queue，避免单点队列。
- **过载控制**：由算法层（`ksched`）与任务阻塞模型共同承担；`ktask` 不实现全局负载均衡守护线程。
