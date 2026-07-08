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
3. `clear_prev_task_on_cpu()` 与 blocked-task 重新入队等待逻辑成对。

### 3) `task.rs`：`TaskContext` 内部可变与当前任务 TLS/CPU-local 指针

- `UnsafeCell<TaskContext>` 的共享读取
- `CurrentTask::init_current/set_current` 写 per-CPU 当前任务指针
- `TaskStack` 手动分配/释放

**不变量**：

1. 任务上下文只在受控调度路径被修改；
2. 当前任务指针在每 CPU 上始终指向有效任务对象；
3. 栈释放发生在任务不再运行且无引用后（由 gc task + `Arc::try_unwrap` 保证）。

### 4) `timers.rs` / `task_registry.rs` / `snapshot`

- `timers` 使用 per-CPU callback 容器原始引用；
- `task_registry` 使用 `Box::into_raw/from_raw` 存放弱引用槽位；
- `snapshot` 使用 `UnsafeCell` 持有 trap frame 快照，并 `unsafe impl Sync`。

**不变量**：

1. `task_registry` 槽位只存 0 或有效 `Box<WeakKtaskRef>` 指针；
2. CAS 成功的一方负责释放；
3. snapshot 读写遵守 session 串行化约束（`begin`/`finish`）。

## 内存与并发不变量

1. **任务状态机单向约束**：`Running/Ready/Blocked/Exited` 转换由 run queue 统一入口执行；`transition_state` 防止重复唤醒重入。
2. **延迟抢占模型**：tick 仅设置 `need_resched`，真实切换在 `enable_preempt` 安全点触发，避免临界区中途切换；若当前 CPU 仍处于 active exception context，则继续延迟抢占直到异常/IRQ trapframe guard 释放。
3. **SMP blocked 唤醒保护**：Blocked 任务重新入队前（`smp`）等待 `task.on_cpu()==false`，防止与远端 CPU `switch_to` 并发。
4. **唤醒抢占请求分离**：waker 只把任务转为 `Ready` 并设置本地或远端 `need_resched` 请求，真实切换仍发生在抢占安全点。
5. **退出回收隔离**：退出任务先进入 `EXITED_TASKS`，由每 CPU `gc_task` 延迟回收，避免切换路径直接 drop。
6. **idle 任务特判**：idle 不入普通调度实体路径，不参与 `task_tick`，避免算法元数据污染。
7. **发布先于 runnable**：需要额外注册对象图的调用方必须先 `prepare_task()`，
   完成外部 publish 后再 `activate_task()`，避免 task 先运行、后补注册。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | per-CPU `&'static mut` 别名导致 UB | 高 | 同 CPU 并发可变借用 | guard + IRQ/preempt 约束；调用点集中封装 |
| T-02 | 任务切换与唤醒并发，导致同任务重复入队 | 高 | 远端 CPU 尚在切换，当前 CPU 立即 unblock | `task.on_cpu()` 自旋等待 + `clear_prev_task_on_cpu` 配对 |
| T-03 | 退出任务过早释放导致 UAF | 高 | 仍有 joiner/切换路径持有引用 | `EXITED_TASKS` + `gc_task` + `Arc::try_unwrap` |
| T-04 | `need_resched` 在错误时机触发重入调度 | 中 | 临界区内或异常/IRQ trapframe guard 未释放时直接抢占切换 | 仅设置 pending，`enable_preempt` 安全点检查，并在 active exception context 内延迟实际切换 |
| T-05 | `task_registry` 指针槽位损坏 | 高 | 非法写入或重复释放 | CAS 协议 + 0/ptr 双态约束 + 弱引用升级校验 |
| T-06 | snapshot 竞态读取错误 trap frame | 中 | 并发 snapshot session | begin/finish 会话串行 + per-CPU 槽位隔离 |
| T-07 | affinity 迁移竞态导致任务丢失 | 中 | 迁移中状态被并发修改 | `migrate_current` 受 run queue 临界区保护 |
| T-08 | tick 回调执行耗时过长拖慢调度 | 中 | callback 滥用 | API 文档约束“回调应短小”；系统仍可抢占恢复 |
| T-09 | 远端唤醒后未及时调度 | 中 | 任务入远端 run queue 但远端 CPU 未到抢占安全点 | `ipi + preempt` 下请求远端 `need_resched`；无 IPI 时仍依赖 tick/安全点 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `RUN_QUEUES` 未初始化访问 | 初始化时序错误 | panic/UB | 调度不可用 | 1 | 在 `init/init_secondary` 后写入，调用路径约束 |
| F-02 | `cpumask` 为空 | 调用方设置非法 affinity | 任务无可运行 CPU | API 失败或 panic | 2 | `set_current_affinity` 判空返回 false |
| F-03 | 长时间持有 NoPreempt 临界区 | 代码路径过重 | 抢占延迟增大 | 延迟抖动/实时性下降 | 3 | 缩短临界区，避免重计算 |
| F-04 | gc task 回收滞后 | 外部长期持有 `Arc` | `EXITED_TASKS` 堆积 | 内存增长 | 2 | `Arc::try_unwrap` 重试，join 语义释放 |
| F-05 | 远端 resched 请求丢失 | IPI 不可用或远端未及时到达安全点 | 被唤醒任务调度延迟 | 吞吐/延迟波动 | 3 | `ipi + preempt` 下发送远端 pending 请求；无 IPI 配置依赖 tick |
| F-06 | 算法 `task_tick` 行为异常 | 调度器实现 bug | 抢占策略失真 | 饥饿/抖动 | 2 | `ksched` 单测覆盖 + trace hook 诊断 |

## 故障管理

- **快速失败策略**：关键不变量处普遍使用 `assert!`（例如任务状态、IRQ 约束、CPU 编号）。
- **延迟恢复策略**：抢占采用 pending + 安全点执行，尽量在一致状态下恢复调度。
- **回收容错策略**：GC 对 `Arc::try_unwrap` 失败重排队，避免误释放。
- **诊断增强**：`snapshot/watchdog` feature 通过共享任务注册表提供锁等待与回溯检查能力。

## 隐私与数据暴露

`ktask` 不直接处理用户隐私数据。可能暴露的信息主要是：

- 任务名、任务 ID、CPU ID（日志与 tracing）
- 回溯信息（snapshot/watchdog 启用时）

这些属于内核诊断输出，受日志通道与构建 feature 控制。

## 已知限制

1. 当前无全局主动负载均衡线程；跨核主要依赖 affinity 与入队选核。
2. `select_run_queue` 采用简单轮询，不基于实时队列负载。
3. 远端唤醒的抢占请求依赖 `ipi + preempt` feature；未启用时仍依赖 tick 或其它安全点推进。
4. `unsafe` 边界仍较多，需持续收敛到更小封装点并补齐 `SAFETY` 说明。

## 审计清单

修改 `ktask` 时建议逐项核对：

- [ ] 新增 `unsafe` 块有清晰 `SAFETY:` 不变量说明。
- [ ] 新增 run queue 访问路径是否保持 guard acquire/release 对称。
- [ ] 新增阻塞/唤醒逻辑是否保持 `TaskState` 转换单调正确。
- [ ] 涉及 `smp` 的路径是否考虑 `on_cpu`/迁移并发窗口。
- [ ] 退出与回收路径是否避免早释放与引用泄漏。
- [ ] `preempt` 开关下行为是否一致（有无 feature 分支遗漏）。
- [ ] 若改动 tick/抢占逻辑，验证不会在临界区中途切换任务。
