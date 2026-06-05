# ksyscall - 设计文档

## 定位

`ksyscall` 是 x-kernel 的 syscall adapter crate。
它负责：

- 按 syscall number 分发入口；
- 解析 Linux ABI 参数；
- 执行用户指针 `copyin/copyout`；
- 做 syscall 边界上的标志位、标量和结构体校验；
- 将调用转接到真正的资源 owner。

`ksyscall` 不拥有 syscall 背后的长期状态、不变量或生命周期。
这些语义由各自的 owner crate 负责。

## 背景

历史上，部分 syscall 实现按 API 名字被聚在一起，
例如 `fs/` 目录同时承接了 VFS 路径操作、`timerfd`、`eventfd`、`pidfd` 等
并不属于同一资源边界的 syscall。

当前重构后的原则是：

- `ksyscall` 只保留 adapter；
- 资源语义回到对应 owner；
- 目录组织按 owner 语义对齐，而不是按历史 API 分类。

## 范围

当前涉及的源文件：

```text
core/ksyscall/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── dispatch.rs
│   ├── ipc/
│   │   ├── eventfd.rs
│   │   ├── mod.rs
│   │   └── pipe.rs
│   ├── io_mpx/
│   ├── sync/
│   │   ├── futex.rs
│   │   ├── membarrier.rs
│   │   └── mod.rs
│   ├── sys.rs
│   ├── task/
│   │   ├── clone.rs
│   │   ├── clone3.rs
│   │   ├── cpu_time.rs
│   │   ├── credentials.rs
│   │   ├── ctl.rs
│   │   ├── execve.rs
│   │   ├── exit.rs
│   │   ├── ids.rs
│   │   ├── job.rs
│   │   ├── limits.rs
│   │   ├── mod.rs
│   │   ├── pidfd.rs
│   │   ├── rusage.rs
│   │   ├── sched.rs
│   │   ├── signal.rs
│   │   ├── thread.rs
│   │   ├── umask.rs
│   │   └── wait.rs
│   ├── time/
│   │   ├── mod.rs
│   │   ├── itimer.rs
│   │   ├── posix_timer.rs
│   │   ├── queries.rs
│   │   ├── sleep.rs
│   │   └── timerfd.rs
│   └── vfs/
│       └── mod.rs
└── docs/
    ├── design.md
    └── security.md
```

## 架构

```text
user trap / arch syscall entry
    │
    v
ksyscall::dispatch_irq_syscall
    │ decode sysno + ABI arguments
    ├─ vfs adapter  ───────────> posix-fs / kfs / kvfs
    ├─ ipc adapter  ───────────> kfd_objects::{EventFd, PipeObject}
    ├─ time adapter ───────────> khal time sources / kthread CPU-time state / kfd_objects::TimerFd
    ├─ task adapter ───────────> kthread / posix-process / kprocess / kcred
    ├─ io_mpx adapter ─────────> kfd_objects::Epoll
    ├─ sync adapter ───────────> kfutex / kthread
    └─ misc adapter ───────────> posix-mm / posix-net / ...
```

## 设计原则

1. `ksyscall` 只拥有 ABI 适配，不拥有资源状态。
2. syscall 的目录归属应尽量贴近它最终路由到的资源 owner。
3. `copyin/copyout`、flag 校验、compat 分支属于 adapter 层。
4. 资源对象的状态机、不变量和生命周期必须留在 owner crate。
5. 不因 syscall 名字相似就把不同资源 owner 混进同一目录。

## 当前 owner 对齐

- `vfs/`
  - 路径和 VFS 语义相关 syscall
  - owner 在 `posix-fs` / `kfs` / `kvfs`
- `ipc/pipe.rs`
  - `pipe2`
  - owner 在 `kfd_objects::PipeObject`
- `ipc/eventfd.rs`
  - `eventfd2`
  - owner 在 `kfd_objects::EventFd`
- `time/timerfd.rs`
  - `timerfd_*`
  - owner 在 `kfd_objects::TimerFd`
- `time/queries.rs`
  - `clock_gettime` / `gettimeofday` / `clock_getres`
  - owner 在 `khal` 时钟源与 `kthread` CPU-time 查询
- `time/sleep.rs`
  - `nanosleep` / `clock_nanosleep`
  - owner 在 `ktask` sleep runtime 与 `khal` 时钟查询
- `time/itimer.rs`
  - `getitimer` / `setitimer`
  - owner 在 `ProcessTimerManager` 的 legacy interval timer 状态
- `time/posix_timer.rs`
  - `timer_create` / `timer_gettime` / `timer_settime` / `timer_delete` / `timer_getoverrun`
  - owner 在 `ProcessTimerManager` 的 POSIX timer 状态与 `kthread` timer delivery runtime
- `io_mpx/`
  - `select` / `pselect6` / `poll` / `ppoll` / `epoll_*`
  - owner 在 `kfd_objects::Epoll` 与通用 `FileLike` poll 接口
- `task/pidfd.rs`
  - `pidfd_*`
  - owner 在 `kthread::PidFd`
- `task/credentials.rs`
  - `get*id` / `set*id` / `getgroups` / `setgroups`
  - owner 在 `kthread` 当前进程 credential helper 与 `kcred` credential 模型
- `task/ids.rs`
  - `getpid` / `getppid`
  - owner 在 `kthread` 当前线程与父子进程关系
- `task/job.rs`
  - `getsid` / `setsid` / `getpgid` / `setpgid`
  - owner 在 `kthread` 进程组与 session 状态
- `task/thread.rs`
  - `gettid` / `set_tid_address` / `arch_prctl`
  - owner 在 `kthread` 当前线程状态与架构线程上下文
- `task/signal.rs`
  - `rt_sigprocmask` / `rt_sigaction` / `rt_sigpending` / `kill` / `tkill` / `tgkill`
    / `rt_sigqueueinfo` / `rt_tgsigqueueinfo` / `rt_sigreturn` / `rt_sigtimedwait`
    / `rt_sigsuspend` / `sigaltstack` / `signalfd4`
  - owner 在 `kthread` 当前线程 signal state、`ksignal` signal model
    与 `kfd_objects::Signalfd`
- `task/cpu_time.rs`
  - `times`
  - owner 在 `kthread` 进程 CPU-time 统计与 `khal` 时钟查询
- `task/rusage.rs`
  - `getrusage`
  - owner 在 `kthread` 进程/线程 CPU-time 采样状态
- `task/limits.rs`
  - `getrlimit` / `setrlimit` / `prlimit64`
  - owner 在 `ProcessState.resources` 的 rlimit 状态
- `task/umask.rs`
  - `umask`
  - owner 在 `ProcessState` 的文件创建掩码状态
- `task/sched.rs`
  - `sched_yield` / `sched_*affinity` / `sched_*scheduler` / `getcpu` / `getpriority` / `setpriority`
  - owner 在 `ktask` 调度接口、`kthread` 进程/线程状态和 `khal` CPU 查询
- `sync/futex.rs`
  - `futex` / `get_robust_list` / `set_robust_list`
  - owner 在 `kfutex` 等待队列与 `kthread` 线程 robust-list 状态

## 非目标

`ksyscall` 不负责：

- 保存 fd-backed object 的内部状态；
- 保存 `ProcessState` / 地址空间 / VFS 节点等共享对象；
- 实现路径解析、signal 状态机、timer 状态机或 pipe buffer 行为；
- 提供“方便导入”的 catch-all owner 抽象。
