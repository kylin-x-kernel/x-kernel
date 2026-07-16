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
        │ on_timer_tick()         │ block/wake/exit/yield
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
| `Scheduler` (`ksched`) | `add_task/pick_next/task_tick/put_prev` 算法策略 |
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

## 核心流程

### 1) 初始化流程

主核：

```text
init_scheduler()
  → run_queue::init()
  → 创建 per-CPU idle task（绑定本 CPU）
  → 创建 per-CPU run queue（含 gc task）
  → 初始化 current 为 main task
```

从核：

```text
init_scheduler_secondary()
  → run_queue::init_secondary()
  → 初始化 current 为 idle task
  → 创建本 CPU run queue（含 gc task）
```

### 2) 调度与切换

主动切换路径：

- `yield_now()` → `yield_current()` → `resched()`
- `exit()` → `exit_current()` → `resched()`
- `blocked_resched()`（阻塞）→ `resched()`

`resched()` 在当前 CPU 的调度器上 `pick_next_task()`；无就绪任务时回退到本 CPU `IDLE_TASK`。

### 3) 时钟 tick 与延迟抢占

`kruntime` 的定时器中断处理会调用 `ktask::on_timer_tick()`：

1. `timers::check_events()`：分发 tick 回调、检查定时 future。
2. `scheduler_timer_tick()`：调用算法 `task_tick(current)`。
3. 若算法返回应抢占，设置 `need_resched`（`set_preempt_pending(true)`）。

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

### 5) 阻塞/唤醒与 WaitQueue/Future

- `future::block_on` 在 `Poll::Pending` 下走 `blocked_resched()`，把当前任务置为 `Blocked`。
- waker 触发时通过 `select_wake_run_queue(...).unblock_task(..., true)` 将任务恢复为 `Ready`。
- SMP 下唤醒优先入任务阻塞时的 home CPU（`task.cpu_id()`）；若 cpumask 已不含该 CPU，则回退到普通 `select_run_queue` 轮询选队。
- `unblock_task(..., true)` 对本 CPU 设置 `need_resched`；对远端 CPU 在 `ipi + preempt` 可用时请求远端设置 `need_resched`。
- `WaitQueue` 基于 `event_listener` 封装等待与通知，支持超时与条件等待。

### 6) 退出回收（GC task）

每 CPU run queue 在创建时自动加入一个 `gc` 任务，循环执行 `poll_gc`：

- 消费 `EXITED_TASKS` 列表
- `Arc::try_unwrap` 成功则立即回收
- 否则放回队列等待外部引用释放
- 通过 `WAIT_FOR_EXIT` waker 休眠/唤醒

这是一种延迟回收模型，避免在退出与切换关键路径直接执行慢释放。

## SMP 语义

- 每 CPU 一个 `RUN_QUEUE`、`IDLE_TASK`、`EXITED_TASKS`、`WAIT_FOR_EXIT`。
- `select_run_queue` 依据任务 `cpumask` 在允许 CPU 集内做轮询选队。
- `select_wake_run_queue` 用于 wait/future 唤醒路径，优先选择任务阻塞时的 home CPU，避免 waker 继续占用 CPU 导致 wakee 在 waker 核排队。
- 当前实现无主动负载均衡器；任务跨核迁移主要由 affinity 变化触发。
- 对 `Blocked` 任务重新入队时，SMP 下会等待 `task.on_cpu()` 清零，避免与远端 CPU 的切换过程并发冲突。
- 远端 run queue 唤醒不会直接切换远端 CPU；在支持 IPI 和抢占时，通过远端 pending-resched 请求把切换推迟到远端安全点。
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
将 `PidHandle` 与非可选 `Box<dyn UserTaskRuntime>` 放在同一分支；
`TaskInner::new_user(..., user_runtime)` 返回后不存在没有 runtime 的用户 task。内核 task 可以不带
runtime。需要用户执行上下文的 unittest 通过正常的 `TaskInner::new_user` 构造并启动独立用户
task；测试完成后由调用方等待该 task 退出，不再向 non-user task 临时注入 runtime。

调度算法默认由 Kconfig 选择（默认 EEVDF）。

## 设计决策

### 为何采用 per-CPU run queue

降低全局锁竞争与缓存抖动；调度路径以本 CPU 局部状态为主，便于 SMP 扩展。

### 为何 tick 不直接切换

中断上下文与临界区中直接切换会放大并发复杂度。延迟到 `enable_preempt` 安全点执行可减少重入风险并保持响应性。

### 为何唤醒路径优先选择当前 CPU

wait/future 唤醒通常发生在释放资源或发送事件的线程上下文中。若目标任务 affinity 允许当前 CPU，将其放入当前 run queue 可减少跨核唤醒延迟，并让 `need_resched` 在本 CPU 的抢占安全点生效；当 affinity 不允许时仍回退到普通选队逻辑。

### 为何引入 `KernelGuardIf` 接口

`kspin` 作为基础库不应直接依赖 `ktask`。通过接口反向注入，打破潜在循环依赖并保持模块边界清晰。

### 为何使用 GC task 回收退出任务

任务退出时可能仍被 joiner 或切换路径持有引用；延迟回收可避免 use-after-free 与路径上的慢释放开销。

## 与 `kruntime` 的边界

- `kruntime` 负责安装时钟中断并在 tick 时回调 `ktask::on_timer_tick()`。
- `ktask` 负责将 tick 转换为调度记账、抢占请求、定时事件唤醒。
- `kruntime` 不感知具体调度算法；`ktask` 不负责硬件 IRQ 路由。

## 冗余与过载

- **冗余设计**：每 CPU 独立 idle/gc 与 run queue，避免单点队列。
- **过载控制**：由算法层（`ksched`）与任务阻塞模型共同承担；`ktask` 不实现全局负载均衡守护线程。
