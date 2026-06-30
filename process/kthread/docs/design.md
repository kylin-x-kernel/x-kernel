# kthread — 设计文档

## 定位

`kthread` 提供 x-kernel 的进程侧线程运行时入口。
它把 `ktask` 的内核任务对象扩展为用户线程，并在 `kprocess` 的进程身份关系之上挂接共享运行态。
调用者通过本 crate 访问当前线程、当前进程状态、当前进程 credential 快照、文件资源、futex key、task/process registry、signal 发送和 timer 投递。

`kthread` 不保存 POSIX 进程身份图本体，也不实现地址空间、文件表、credential、signal 和 futex 的核心数据结构。
这些能力分别由 `kprocess`、`memspace`、`kresources`、`kcred`、`ksignal` 和 `kfutex` 维护。

## 背景

x-kernel 中的用户线程需要同时接入调度、进程身份、地址空间、文件系统上下文、信号、timer、futex 和 procfs 展示。
`ktask` 只表达内核任务与调度状态，`kprocess` 只表达 PID、进程组、session、父子关系和线程组成员。
`kthread` 在两者之间提供进程侧运行态 facade，使 syscall、POSIX 子系统、procfs、TTY、TEE 和内核服务可以通过稳定入口访问当前用户线程及其共享进程状态。

## 范围

涉及的源文件：

```text
process/kthread/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── cpu_time.rs
    ├── credentials.rs
    ├── lifecycle_state.rs
    ├── pidfd.rs
    ├── posix_state.rs
    ├── process_state.rs
    ├── registry.rs
    ├── runtime_state.rs
    ├── signal.rs
    ├── stat.rs
    ├── timer_delivery.rs
    └── thread/
        ├── core.rs
        ├── current.rs
        ├── mod.rs
        └── task_ext.rs
```

## 架构

```text
ktask::TaskInner
      │ task_ext: Box<Thread>
      ▼
Thread
  ├─ proc_state: Arc<ProcessState>
  ├─ signal: Arc<ThreadSignalManager>
  ├─ time: CpuTimeStatistics
  ├─ clear_child_tid / robust_list_head
  └─ exit / accessing_user_memory / TEE session

ProcessState
  ├─ proc: Arc<kprocess::Process>
  ├─ resources: Arc<ProcessResources>
  ├─ posix: ProcessPosixState
  ├─ lifecycle: ProcessLifecycleState
  ├─ runtime: ProcessRuntimeState
  ├─ signal: Arc<ProcessSignalManager>
  ├─ futex: Arc<ProcessFutexState>
  └─ credentials / optional TEE state
```

| 组件 | 职责 |
|------|------|
| `Thread` | 保存单线程状态、线程信号、CPU time、robust list、clear-child-tid 和退出标志 |
| `CurrentThread` | 包装当前 `KtaskRef`，通过 `Deref` 访问当前用户线程 |
| `ProcessState` | 聚合进程共享运行态，连接 `kprocess::Process`、资源表、地址空间、信号和 timer |
| `credentials` helper | 暴露当前进程 credential 读写和 access snapshot helper |
| `ProcessRuntimeState` | 保存地址空间、文件系统上下文、heap top 和进程 timer manager |
| `ProcessPosixState` | 保存 exe path、cmdline、exit signal 和 umask |
| `ProcessLifecycleState` | 保存子进程退出事件、进程退出事件和已回收子进程 CPU time |
| `PidFd` | 通过 fd table 暴露 `ProcessState` 生命周期和跨进程 capability 入口 |
| `registry` | 保存 task、process、process group、session 的 weak lookup table |
| `signal` | 提供 thread、process、process group 级信号发送入口 |
| `timer_delivery` | 连接 `ktimer` 过期事件、signal 构造和 alarm task 注册 |
| `TaskStat` | 从 `TaskInner` 与 `Thread` 构造 `/proc/[pid]/stat` 展示数据 |

## 调用约束 / 执行上下文

- 当前线程专用入口依赖 scheduler 已可用，并且当前 task 带有 `Box<Thread>` task extension。
- `current_thread` 返回的 handle 在 deref 时要求当前 task 是用户线程；`current_process_state`、`current_process_fs_context`、`current_resources`、credential helper 和 `current_futex_key` 直接要求用户线程上下文。
- `current_fs_context` 可在内核任务路径使用，内核任务会回退到 kernel 默认文件系统上下文。
- registry、signal、timer 和 futex key helper 面向 task/syscall 生命周期路径，调用点不应位于中断上下文。
- `ProcessState::new` 依赖 `kprocess::Process`、地址空间、文件系统上下文、信号动作表和 credential 已由调用者初始化。
- `init_timer_runtime` 依赖 timer 和 signal 子系统已完成基础初始化，并通过 `Once` 保证重复调用只注册一次。
- TEE 相关入口只在 `tee` feature 下可用，调用方需要保证具体 private runtime state 类型一致。

## 状态机

### 线程运行态标志

```text
Running
  ├─ set_accessing_user_memory(true)  → AccessingUserMemory
  ├─ set_cpu_state(User/Kernel/None)  → CPU accounting state update
  └─ set_exit()                       → Exiting
```

`Thread::exit` 使用 Release/Acquire 发布退出状态。
`accessing_user_memory` 只表达当前线程是否处在用户内存访问窗口，fault 处理和 user-memory 访问路径据此观测线程状态。
CPU time 状态在 `CpuTimeStatistics` 内以 `None`、`User`、`Kernel` 三态切换，切换前先结算上一个状态的 wall-clock delta。

### ProcessState 运行态关系

```text
Created
  → Registered in task/process tables
  → Active while at least one Thread holds Arc<ProcessState>
  → Stale weak registry entry after last strong reference drops
  → Removed by cleanup_task_tables()
```

registry 只保存 weak entry，状态转换由强引用生命周期驱动。
stale entry 不阻止对象释放，lookup 时只返回仍可升级的对象。

## 算法流程

### 线程创建与 task 扩展

```text
clone / init entry
  → kprocess::Process 创建或继承身份关系
  → ProcessState::new 创建共享运行态
  → Thread::new(tid, proc_state) 创建 Box<Thread>
  → ktask task extension 存入 Box<Thread>
  → add_task_to_table 注册 task/process/group/session weak entry
```

`Thread` 作为 `ktask::TaskExt` 挂到任务扩展槽；该扩展槽是 `ktask` 的常规能力，不依赖额外 feature。
后续 `TaskInner::as_thread` 通过扩展槽 downcast 取回 `Thread`。
内核任务没有 `Thread` 扩展，调用进程专用入口会触发 panic 或返回错误。

### 当前线程访问

```text
current_thread()
  → ktask::current()
  → CurrentThread(KtaskRef)
  → Deref
  → TaskInner::as_thread()
```

`current_fs_context` 对用户线程返回进程文件系统上下文，对内核任务回退到 kernel 默认上下文。
`current_process_state`、`current_process_fs_context`、`current_resources`、credential helper 和 `current_futex_key` 属于用户线程专用入口。

### 当前进程 credential helper

```text
with_current_credentials(_)
  → current ProcessState
  → credentials.read()

with_current_credentials_mut(_)
  → current ProcessState
  → credentials.write()
```

这层 helper 只负责在当前用户线程上下文下暴露 `ProcessState::credentials` 的读写入口。
credential 模型和视图转换规则仍由 `kcred` 维护，
调用侧如果需要具体 snapshot，应基于这里的基础入口自行组合。

### registry 查询

```text
add_task_to_table(task)
  ├─ TASK_TABLE[tid] = weak task
  ├─ PROCESS_TABLE[pid] = weak ProcessState
  ├─ PROCESS_GROUP_TABLE[pgid] = weak ProcessGroup
  └─ SESSION_TABLE[sid] = weak Session

get_task(0) / get_process_state(0)
  → 当前 task / 当前 ProcessState
```

registry 使用 weak table，不拥有 task、process state、process group 或 session。
对象生命周期由 `ktask`、`kprocess` 和外部强引用决定。
`cleanup_task_tables` 清理过期 weak entry。

### signal 发送

```text
send_signal_to_thread(tgid, tid, sig)
  → get_task(tid)
  → try_as_thread()
  → 校验 tgid
  → ThreadSignalManager::send_signal
  → task.interrupt()

send_signal_to_process(pid, sig)
  → get_process_state(pid)
  → ProcessSignalManager::send_signal
  → get_task(selected_tid)
  → task.interrupt()
```

process group 信号遍历 `kprocess::ProcessGroup` 当前可升级成员，并复用 process 级入口。
`sig == None` 表示只做目标存在性检查。

### timer 投递

```text
init_timer_runtime()
  → register_expired_task_handler(poll_timer)
  → register_signal_observer(SIGALRM/SIGVTALRM/SIGPROF/RT)
  → ktimer::spawn_alarm_task()

poll_timer(pid)
  → ProcessTimerManager::poll_wall_clock()
  → dispatch_timer_delivery()
  → signal helper

poll_cpu_timers()
  → process_cpu_time_ns()
  → ProcessTimerManager::poll_cpu_timers()
  → dispatch_timer_delivery()
```

timer signal dequeue 时，observer 调用 `ProcessTimerManager::on_timer_signal_dequeued` 校验 timer id 与 signal sequence，决定投递或丢弃。

### futex key 构造

```text
current_futex_key(address)
  → current ProcessState address_space
  → FutexKey::new(aspace, address)
```

`kthread` 只负责从当前用户线程上下文构造 `FutexKey`。
private/shared futex table 的实例管理、shared identity 路由和 stale table 清理由 `kfutex::ProcessFutexState` 维护。

## 并发模型

- `Thread` 内的 clear-child-tid、robust list、OOM score、退出标志和 user-memory 访问标志使用 atomic 字段。
- `Thread::time` 使用 `Mutex<CpuTimeStatistics>`，采样和状态切换在同一锁内更新。
- `ProcessState::credentials`、TEE TA state 和 TEE private state 使用 `RwLock`。
- `ProcessRuntimeState::heap_top` 使用 Acquire/Release atomic，地址空间、文件系统上下文和 timer manager 使用 `Arc<Mutex<...>>`。
- registry 表使用全局 `RwLock<WeakMap<...>>`，查询返回可升级强引用。
- signal helper 在目标查找后调用 signal manager，并在有新 pending signal 时 interrupt 对应 task。

## 设计决策

### 作为进程运行态 facade

`kthread` 当前公开面较宽，包含 thread/process state、当前上下文 helper、registry、signal、timer、pidfd 以及 resource/rlimit re-export。
这保持了 syscall、POSIX、procfs、TTY 和 TEE 调用点的稳定导入路径。
代价是 crate 边界呈 facade 形态，后续新增 API 需要先验证外部调用者，内部 helper 优先保持私有或 `pub(crate)`。

### `kprocess` 与 `kthread` 分层

`kprocess` 保存进程身份关系和 job-control 图。
`kthread` 保存运行时状态和当前线程访问入口。
这种分层让进程关系可在不依赖地址空间、资源表、signal manager 的情况下独立使用，也避免进程身份对象持有过多运行态资源。

### registry 使用 weak entry

全局 registry 是查询索引，不承担生命周期所有权。
weak entry 避免 task/process/session 之间形成全局强引用环。
代价是 lookup 需要处理对象已释放的情况，并通过 cleanup 路径回收 stale entry。

### 当前线程 helper 区分用户任务和内核任务

`current_fs_context` 支持内核任务回退到 kernel 文件系统上下文。
进程专用 helper 依赖当前 task 具有 `Thread` 扩展。
这种区分让共享路径可以安全解析路径，也让 syscall/POSIX 路径在误用内核任务上下文时快速失败。

### timer 通过 signal 桥接

进程 timer 过期结果统一转换为 signal。
这种设计复用 `ksignal` 的 pending 队列、目标选择和中断唤醒机制。
代价是 timer signal dequeue 需要额外 sequence 校验，避免陈旧 signal 被错误投递。

## Drop / 资源释放

- `Thread` 没有自定义 `Drop`，随 task extension 中的 `Box<Thread>` 释放。
- `ProcessState` 通过 `Arc` 引用计数管理，共享给同进程全部线程和外部查询者。
- registry、process group 和 session 索引只持有 weak entry，不阻止对象释放。
- TEE private runtime state 通过 `Arc<dyn Any + Send + Sync>` 保存，清理时直接置空 slot。
