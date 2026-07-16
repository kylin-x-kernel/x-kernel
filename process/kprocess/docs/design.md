# kprocess — 设计文档

## 定位

`kprocess` 提供 x-kernel 的进程身份、线程对象和 POSIX job-control 基础对象。
在最新设计决议中，它是唯一的 process domain owner crate。
它维护 process、thread、process group、session、父子关系、线程组成员、退出状态、
publication/registry、signal targeting、timer delivery glue、controlling terminal 绑定。
当前 `Process` 自身保留一个强类型的弱 runtime 引用
（`Weak<ProcessRuntime>`），并直接对外提供 fs/mm/futex/signal/timer/credential
等 capability 方法；但外部 `live process` 语义不再以弱 runtime 引用为判据，
而是以 `Process` 生命周期状态是否已经进入 zombie 为准。
`current_user_*` helpers 显式要求当前 task 必须已经安装 user-thread runtime。
`posix/process`、`ksyscall`、`ktty`、procfs 等模块只通过 `Process` 公开 API
或 `process_exit` / `wait_reap` 语义模块查询或调整进程关系与能力。

## 背景

x-kernel 需要在内核中表达 POSIX 进程层级、进程组、session 和终端控制关系。
这些关系跨线程调度、信号、wait、TTY job control 和 procfs 展示共用。
`kprocess` 当前已经同时拥有：

- `Process`
- `ProcessRuntime`
- `Thread`
- publication / registry / signal / timer glue
- lookup / domain-facing query modules

## 范围

涉及的源文件：

```text
process/kprocess/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── capability.rs
    ├── credentials.rs
    ├── job_control.rs
    ├── lookup.rs
    ├── pidfd.rs
    ├── process.rs
    ├── process_exit.rs
    ├── process_signals.rs
    ├── process_group.rs
    ├── process_runtime/
    │   ├── mod.rs
    │   ├── posix_state.rs
    │   └── runtime_state.rs
    ├── procfs.rs
    ├── publication.rs
    ├── resource_limits.rs
    ├── scheduler.rs
    ├── session.rs
    ├── signal.rs
    ├── stat.rs
    ├── system_view.rs
    ├── thread/
    │   ├── cpu_time.rs
    │   ├── core.rs
    │   ├── current.rs
    │   └── task_ext.rs
    ├── timer_delivery.rs
    └── wait_reap.rs
    
```

## 架构

```text
Process
  ├─ parent: Weak<Process>
  ├─ children: StrongMap<Pid, Arc<Process>>
  ├─ thread_group: ThreadGroup
  ├─ lifecycle: ProcessLifecycleState
  ├─ group: Arc<ProcessGroup>
  └─ runtime_ref: Option<Weak<ProcessRuntime>>
              │
              v
        ProcessGroup
          ├─ processes: WeakMap<Pid, Weak<Process>>
          └─ session: Arc<Session>
                          │
                          v
                    Session
                      ├─ process_groups: WeakMap<Pid, Weak<ProcessGroup>>
                      └─ terminal: Option<Arc<dyn Any + Send + Sync>>
```

| 组件 | 职责 |
|------|------|
| `Process` | 保存 leader `PidHandle`、父子关系、线程组状态、lifecycle 事件、已退出线程/已回收 child CPU time、zombie 标志、当前进程组、弱 runtime 引用，以及对外 capability 入口 |
| `ProcessPublication` | 保存 published task/process/group/session 的全局可观测目录，并承载 publish / unpublish / lookup / iteration；目录更新在单次 publication 事务内完成，避免 task/process/group/session lookup 读到跨表半发布状态；其中 `published` 覆盖 zombie 未 reap 的稳定身份，`live` 仅表示非 zombie 的外部可操作进程 |
| `kidentity` | 作为 process domain 的 identity owner，维护底层 `PidHandle` 与 `PidNamespace`；`kprocess` 当前对外仍暴露 root/global `Pid/Tid` 语义 |
| `lookup` | `kprocess` 内部目录原语层，负责 `published/live` 合约下的 task/process/group 查找 |
| `procfs` / `scheduler` / `job_control` / `pidfd` / `process_signals` / `resource_limits` / `process_exit` / `wait_reap` / `system_view` | 面向外部领域语义的窄接口层；外部模块通过这些模块表达“要做什么”，而不是自己理解 `published/live` |
| `ProcessLifecycleState` | 保存子进程退出事件、进程退出事件、已退出线程 CPU time 和已回收子进程 CPU time |
| `ThreadGroup` | 保存进程自有的 published 线程成员表（`tid -> weak task`）、进程退出码和 group-exit 标志 |
| `ProcessGroup` | 保存 PGID、所属 session 和弱引用进程成员表 |
| `Session` | 保存 SID、弱引用进程组表和 controlling terminal |
| `INIT_PROC` | 保存全局 init 进程，供退出 reparent 和 `init_proc` 查询 |

## 状态机

### Process 生命周期

```text
Created ──publish_task──> Running ──exit_thread(last)──> Exiting
   │                                              │
   │                                              v
   └──────────────────────────────────────────> Zombie ──free──> Reaped
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| `Created` | `Running` | publication 阶段把已准备好的 task 发布到 `Process` 自有线程成员表 |
| `Running` | `Running` | 非最后一个线程调用 `exit_thread` |
| `Running` | `Zombie` | 最后一个线程退出后调用 `Process::exit` |
| `Zombie` | `Reaped` | 父进程 wait 路径调用 `free` |

`Process::exit` 对 init 进程直接返回。
普通进程退出时设置 zombie 标志，并把子进程 reparent 到 init 进程。
`free` 只允许 zombie 进程调用，并从父进程 children 表移除当前进程。

### Process group 和 session 转换

```text
Inherited group
  ├─create_group───> New process group in same session
  ├─create_session─> New session + new process group
  └─move_to_group──> Existing group in same session
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| 继承父进程 group | 新 group | `create_group` 且当前进程不是 group leader |
| 继承父进程 session | 新 session 和新 group | `create_session` 且当前进程不是 session leader |
| 当前 group | 指定 group | `move_to_group` 且目标 group 属于同一 session |

## 算法流程

### 创建 init 进程

1. owner 为 init task 分配 leader `PidHandle`，保证首个 root-visible leader 为 `pid/tid 1`。
2. 创建新的 `Session` 和 `ProcessGroup`，并建立稳定 init `Process` 身份。
3. 创建 init `Thread` 与 `ProcessRuntime`，并把弱 runtime 引用登记到该 `Process`。
4. 调用 `TaskInner::new_user(..., thread)`；构造器在返回前建立带有 `UserTaskRuntime` 的用户 task，因此不存在可见的半初始化状态。
5. 调用方完成 tty / stdio 等外部 owner 设置后，再调用 `start_user_task(task)`。

### fork 子进程

1. `fork_process_runtime` 先克隆 child `NsProxy`。
2. 当前 `CLONE_NEWPID` 尚未启用，因此 leader `PidHandle` 仍在 root PID namespace
   分配，并保持 root-visible `tid == pid`；后续 task-active PID namespace 应挂在
   task/PID identity 层，而不是 `NsProxy`。
3. 再创建子进程稳定身份、加入 group 的 weak member 表、挂到父进程 children 表。
4. 然后在同一个 owner 域内创建 `ProcessRuntime`、地址空间、文件表和信号状态，并把弱 runtime 引用登记到新 `Process`。
5. syscall 层通过 staged publication 事务先完成 publication，使 PID/TID 与 group/session lookup 对外一致可见；随后执行 `CLONE_PARENT_SETTID` / `CLONE_PIDFD` 等父侧 writeback，成功后才把 task 变为 runnable，失败则回滚 publication 与 owner-side child membership。顺序对齐 Linux `kernel_clone()` 的“先完成 parent-side return setup，再 `wake_up_new_task()`”约束。

### 创建同进程线程

1. process-domain owner 为 sibling thread 分配新的 thread `PidHandle`。
2. 新 `Thread` 复用所属 `Process` 与 `ProcessRuntime`，并要求 task-owned identity 与 thread identity 保持一致。
3. `prepare_user_task()` 在发布前校验 task identity / thread identity 一致性。
4. publication 在单次 owner-side 事务里同时更新 task/process/group/session 可见性，并把 task 挂入 `Process` 自有线程成员表；事务对象在 activation 前若失败或被丢弃，会自动回滚 task/process 可见性以及未提交 child 的 owner-side 成员关系。可观测方应通过语义 helper 查询 published thread，而不是扫描全局 task 目录反推成员关系。

### 创建 session

1. `create_session` 检查当前进程所属 session 的 SID 是否等于 PID。
2. 创建 SID 等于 PID 的新 `Session`。
3. 创建 PGID 等于 PID 的新 `ProcessGroup`。
4. `set_group` 从旧 group 移除当前进程，再加入新 group。
5. 返回新 session 和新 group。

### 进程退出和回收

1. `exit_thread` 从 `Process` 自有线程成员表移除 TID，并在未 group-exit 时记录退出码。
2. 最后一个线程退出后，`posix-process` runtime glue 经 `process_exit` 语义模块触发稳定 `Process` 的 zombie 转换。
3. `Process` 内部设置 zombie 标志，将所有子进程 reparent 到 init。
4. zombie 之后该 `Process` 仍保持 published 身份，继续承担 wait/pidfd/reap 语义；与此同时，外部 `live` 查询必须开始把它视为不可操作对象。
5. 弱 runtime 引用不在 zombie 转换时主动清除，而是允许当前退出线程在尾段继续通过稳定 `Process` 访问其已持有的运行态资源；其生命周期最终由 `Thread -> Arc<ProcessRuntime>` 强引用自然结束。
6. 退出路径在唤醒父进程前，先把当前线程最终 CPU time 累计到 `ProcessLifecycleState`，再直接通过父进程 `child_exit_event` 唤醒等待者，并通过本进程 `exit_event` 通知 pidfd/poll 观察者。
7. wait 路径观察 zombie 子进程后调用 `wait_reap::reap_zombie_process`。
8. 回收阶段从父进程 children 表删除当前 PID，并从 published PID 目录撤销该身份。

## 并发模型

- `Process::thread_group`、`children`、`parent`、`group` 使用 `SpinNoIrq`，避免持锁区被本地中断打断。
- `Process::runtime_ref` 使用 `SpinNoIrq<Option<Weak<ProcessRuntime>>>`，只保存非拥有型 runtime upgrade 入口，不承担对外 `live` 判定。
- `kidentity` 当前按 namespace 线性分配 `PidHandle`；当前阶段不做回收，后续如引入 pid reuse，应仍保持 “publish before runnable” 不变量。
- `PidHandle` 已能携带 namespace 链，但 `kprocess` 对外 `pid()/tid()` 仍固定返回 root/global 编号；在 wait、kill、procfs、registry 全部 namespace-aware 之前，不把 namespace-visible 编号暴露到对外主语义。
- `ProcessLifecycleState` 的 wait 事件通过 `Arc<PollSet>` 共享，已退出线程和已回收 child CPU time 使用 relaxed 原子累计。
- `ProcessGroup::processes` 和 `Session::process_groups` 使用 `WeakMap`，避免 group/session 成员表延长成员生命周期。
- `Session::terminal` 使用 `SpinNoIrq<Option<Arc<dyn Any + Send + Sync>>>`，controlling terminal 只能设置一次，清除时要求对象指针匹配。
- `is_zombie` 使用 `AtomicBool`，`exit` 使用 Release 写入，`is_zombie` 使用 Acquire 读取。
- `set_group` 同时修改当前 process、旧 group 和新 group，调用者应避免在外部持有这些对象的成员锁后再调用 group mutation API。

## 设计决策

### 关系对象独立于运行时资源

`kprocess` 只维护进程身份关系和生命周期摘要。
地址空间、文件描述符、credential、信号处理和 futex 表留在 `kprocess`、`kresources`、`kcred`、`ksignal` 和 `kfutex`。
这种分层让 job-control 关系能够被多个上层模块共享，也避免 `kprocess` 变成进程运行时状态集合。
保留弱 runtime 引用是为了让稳定 `Process` 对象自己承载“升级到运行态 capability”的入口，
而不是让外部再依赖额外 trait 或第二层全局 registry 真相。

### 查询策略层与领域接口层分离

`published` 和 `live` 仍然是 `kprocess` 内部必须保留的两类 contract：

- `published` 用于可观测身份目录
- `live` 用于对外仍可操作的非 zombie 进程

但这两个术语不再作为外部主编程模型暴露。当前实现采用两层结构：

1. `lookup`
   - 只承载 crate 内部目录原语
   - 统一持有 `published/live` 判定语义
2. 领域模块
   - 如 `procfs`、`scheduler`、`job_control`、`pidfd`、`process_signals`
   - 对外暴露按场景命名的窄接口

这样可以同时避免两种坏味道：

- 外部模块直接理解 `published/live`
- `kprocess::lib.rs` 根命名空间堆满按调用方枚举的 helper

### group/session 成员表使用弱引用

process group 和 session 是查询索引，不拥有成员进程或成员进程组。
成员表使用 `WeakMap`，生命周期由进程父子关系、任务表和外部强引用决定。
这种设计降低循环引用风险，代价是查询时需要清理或过滤过期 weak entry。

### init 进程承担 reaper 角色

当前实现把退出进程的子进程统一 reparent 到 init。
这满足基本 wait 和 orphan child 处理需求。
subreaper 尚未实现，代码保留 TODO。

## Drop / 资源释放

- `Process` 没有自定义 `Drop`，生命周期由 `Arc` 引用计数控制。
- 父进程 children 表持有子进程强引用，`free` 从父表移除 zombie 子进程后释放这条所有权边。
- `ProcessGroup` 和 `Session` 成员表持有弱引用，不阻止成员释放。
- `Session::unset_terminal` 只在传入 terminal 与当前 terminal 指针相同的时候清空绑定。
