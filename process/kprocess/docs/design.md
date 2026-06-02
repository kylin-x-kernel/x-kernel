# kprocess — 设计文档

## 定位

`kprocess` 提供 x-kernel 的进程身份和 POSIX job-control 基础对象。
它维护 process、process group、session、父子关系、线程组成员、退出状态和 controlling terminal 绑定。
`kthread` 在这些对象之上挂接运行时状态、任务表和资源状态，`posix/process`、`ksyscall`、`ktty`、procfs 等模块通过公开 API 查询或调整进程关系。

## 背景

x-kernel 需要在内核中表达 POSIX 进程层级、进程组、session 和终端控制关系。
这些关系跨线程调度、信号、wait、TTY job control 和 procfs 展示共用。
`kprocess` 只保存身份关系和生命周期摘要，不保存地址空间、文件表、信号处理器或 credential 等运行时资源。

## 范围

涉及的源文件：

```text
process/kprocess/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── process.rs
    ├── process_group.rs
    ├── session.rs
    └── tests.rs
```

## 架构

```text
Process
  ├─ parent: Weak<Process>
  ├─ children: StrongMap<Pid, Arc<Process>>
  ├─ tg: ThreadGroup
  └─ group: Arc<ProcessGroup>
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
| `Process` | 保存 PID、父子关系、线程组状态、zombie 标志和当前进程组 |
| `ThreadGroup` | 保存线程 ID 集合、进程退出码和 group-exit 标志 |
| `ProcessGroup` | 保存 PGID、所属 session 和弱引用进程成员表 |
| `Session` | 保存 SID、弱引用进程组表和 controlling terminal |
| `INIT_PROC` | 保存全局 init 进程，供退出 reparent 和 `init_proc` 查询 |

## 状态机

### Process 生命周期

```text
Created ──add_thread──> Running ──exit_thread(last)──> Exiting
   │                                              │
   │                                              v
   └──────────────────────────────────────────> Zombie ──free──> Reaped
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| `Created` | `Running` | 调用者通过 `add_thread` 添加线程 ID |
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

1. `Process::new_init(pid)` 调用内部 `Process::new(pid, None)`。
2. 创建新的 `Session` 和 `ProcessGroup`。
3. 创建 `Process`，父进程设为空弱引用。
4. 把进程加入所属 process group。
5. 如果 `INIT_PROC` 尚未初始化，将该进程设为全局 init 进程。

### fork 子进程

1. 父进程调用 `fork(pid)`。
2. 子进程继承父进程的 `ProcessGroup`。
3. 子进程加入 group 的 weak member 表。
4. 父进程 children 表加入子进程强引用。
5. 调用者继续在 `kthread` 层创建 `ProcessState`、地址空间、文件表和信号状态。

### 创建 session

1. `create_session` 检查当前进程所属 session 的 SID 是否等于 PID。
2. 创建 SID 等于 PID 的新 `Session`。
3. 创建 PGID 等于 PID 的新 `ProcessGroup`。
4. `set_group` 从旧 group 移除当前进程，再加入新 group。
5. 返回新 session 和新 group。

### 进程退出和回收

1. `exit_thread` 从线程集合移除 TID，并在未 group-exit 时记录退出码。
2. 最后一个线程退出后，`core/kservices` 调用 `Process::exit`。
3. `Process::exit` 设置 zombie 标志，将所有子进程 reparent 到 init。
4. wait 路径观察 zombie 子进程后调用 `free`。
5. `free` 从父进程 children 表删除当前 PID。

## 并发模型

- `Process::tg`、`children`、`parent`、`group` 使用 `SpinNoIrq`，避免持锁区被本地中断打断。
- `ProcessGroup::processes` 和 `Session::process_groups` 使用 `WeakMap`，避免 group/session 成员表延长成员生命周期。
- `Session::terminal` 使用 `SpinNoIrq<Option<Arc<dyn Any + Send + Sync>>>`，controlling terminal 只能设置一次，清除时要求对象指针匹配。
- `is_zombie` 使用 `AtomicBool`，`exit` 使用 Release 写入，`is_zombie` 使用 Acquire 读取。
- `set_group` 同时修改当前 process、旧 group 和新 group，调用者应避免在外部持有这些对象的成员锁后再调用 group mutation API。

## 设计决策

### 关系对象独立于运行时资源

`kprocess` 只维护进程身份关系和生命周期摘要。
地址空间、文件描述符、credential、信号处理和 futex 表留在 `kthread`、`kresources`、`kcred`、`ksignal` 和 `kfutex`。
这种分层让 job-control 关系能够被多个上层模块共享，也避免 `kprocess` 变成进程运行时状态集合。

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
