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
│   ├── arch/
│   │   ├── mod.rs
│   │   └── riscv_hwprobe.rs
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
    ├─ vfs adapter  ───────────> posix-fs / kvfs
    ├─ ipc adapter  ───────────> kfd_objects::{EventFd, PipeObject}
    ├─ time adapter ───────────> khal time sources / kprocess CPU-time state / kfd_objects::TimerFd
    ├─ task adapter ───────────> kprocess / posix-process / kprocess / kcred
    ├─ io_mpx adapter ─────────> kfd_objects::Epoll
    ├─ sync adapter ───────────> kfutex / kprocess
    └─ misc adapter ───────────> posix-mm / posix-net / ...
```

## 设计原则

1. `ksyscall` 只拥有 ABI 适配，不拥有资源状态。
2. syscall 的目录归属应尽量贴近它最终路由到的资源 owner。
3. `copyin/copyout`、flag 校验、compat 分支属于 adapter 层。
4. 资源对象的状态机、不变量和生命周期必须留在 owner crate。
5. 不因 syscall 名字相似就把不同资源 owner 混进同一目录。
6. 每个 syscall number 按自己的 ABI 参数个数解码；旧 ABI 未定义的参数寄存器不得被当作扩展 flags 使用。

## 当前 owner 对齐

- `vfs/`
  - 路径和 VFS 语义相关 syscall
  - owner 在 `posix-fs` / `kvfs`
- `ipc/pipe.rs`
  - `pipe2`
  - owner 在 `kfd_objects::PipeObject`
- `ipc/eventfd.rs`
  - `eventfd2`
  - owner 在 `kfd_objects::EventFd`
- `sys.rs`
  - `sethostname` 路由到当前 UTS namespace
  - `reboot` 校验 Linux magic/command 后路由到 `khal::power` 终点：
    `HALT` 走 `halt()`（停止所有 CPU、保持供电），`POWER_OFF` 走
    `power_off()`（平台断电）；两者进入终点前均通过 SMP stop 钩子先停
    止其他 CPU，fs sync/设备清理等上层收尾由后续 orderly-shutdown
    supervisor 负责；`SW_SUSPEND` 走 `suspend_to_ram()`（非终点：平台
    睡眠代理进入 S3，无代理/被拒时向调用方返回平台错误）
- `arch/`
  - 架构专属 system-info/control syscall adapter（`arch/mod.rs` 按架构
    `cfg` 组织子模块，`lib.rs` 声明顶层 `mod arch;`，`sys.rs` 仅
    `pub use crate::arch::*` 转发），避免为单个架构在顶层散落特例文件
  - riscv64 `riscv_hwprobe` 解析 Linux `struct riscv_hwprobe`、用户
    cpuset 和 `RISCV_HWPROBE_WHICH_CPUS`，再向 `kcpu` 查询每个 present
    logical CPU 的 RISC-V capability snapshot；`ksyscall` 只做 ABI
    copyin/copyout 和 cpuset 边界处理，hwprobe key 的取值、聚合与匹配
    语义由 `kcpu` 的 hwprobe helper 提供
  - riscv64 `riscv_flush_icache` 读取第 3 个参数 `flags`（Linux ABI 为 64
    位：内核 syscall 定义 `uintptr_t`、libc 原型 `unsigned long`，故按
    `usize` 处理并保留位检查覆盖全部 64 位——注意与 `riscv_hwprobe` 的 32 位
    `unsigned int flags` 区分）：`SYS_RISCV_FLUSH_ICACHE_LOCAL` 只刷新本 hart
    （`karch::flush_icache_all_local()`，等价单条 `fence.i`），其余保留位
    返回 `EINVAL`；未置位 LOCAL 时调用 `karch::flush_icache_all()` 经 IPI
    广播到所有 hart，保证自修改代码在任务迁移后仍可见
- `time/timerfd.rs`
  - `timerfd_*`
  - owner 在 `kfd_objects::TimerFd`
- `time/queries.rs`
  - `time` / `clock_gettime` / `gettimeofday` / `clock_getres` / `clock_settime` /
    `settimeofday`
  - owner 在 `khal` 时钟源、`ktime` realtime 时钟关联（含 `set_realtime`）与 `kprocess` CPU-time 查询
- `time/sleep.rs`
  - `nanosleep` / `clock_nanosleep`
  - owner 在 `ktask` sleep runtime 与 `khal` 时钟查询
- `time/itimer.rs`
  - `getitimer` / `setitimer`
  - owner 在 `ProcessTimerManager` 的 legacy interval timer 状态
- `time/posix_timer.rs`
  - `timer_create` / `timer_gettime` / `timer_settime` / `timer_delete` / `timer_getoverrun`
  - owner 在 `ProcessTimerManager` 的 POSIX timer 状态与 `kprocess` timer delivery runtime
- `io_mpx/`
  - `select` / `pselect6` / `poll` / `ppoll` / `epoll_*`
  - owner 在 `kfd_objects::Epoll` 与通用 `FileLike` poll 接口
- `task/pidfd.rs`
  - `pidfd_*`
  - owner 在 `kprocess::PidFd`
- `task/credentials.rs`
  - `get*id` / `set*id` / `getgroups` / `setgroups`
  - owner 在 `kprocess` 当前进程 credential helper 与 `kcred` credential 模型
- `task/ctl.rs`
  - `prctl` 的 `PR_GET_KEEPCAPS` / `PR_SET_KEEPCAPS`
  - owner 在 `kprocess` 当前任务凭据发布路径与 `kcred` securebits 状态
- `task/ids.rs`
  - `getpid` / `getppid`
  - owner 在 `kprocess` 当前线程与父子进程关系
- `task/job.rs`
  - `getsid` / `setsid` / `getpgid` / `getpgrp` / `setpgid`
  - owner 在 `kprocess` 进程组与 session 状态
- `task/thread.rs`
  - `gettid` / `set_tid_address` / `arch_prctl`
  - owner 在 `kprocess` 当前线程状态与架构线程上下文
- `task/signal.rs`
  - `rt_sigprocmask` / `rt_sigaction` / `rt_sigpending` / `kill` / `tkill` / `tgkill`
    / `rt_sigqueueinfo` / `rt_tgsigqueueinfo` / `rt_sigreturn` / `rt_sigtimedwait`
    / `rt_sigsuspend` / `sigaltstack` / `signalfd4`
  - owner 在 `kprocess` 当前线程 signal state、`ksignal` signal model
    与 `kfd_objects::Signalfd`
- `task/cpu_time.rs`
  - `times`
  - owner 在 `kprocess` 进程 CPU-time 统计与 `khal` 时钟查询
- `task/rusage.rs`
  - `getrusage`
  - owner 在 `kprocess` 进程/线程 CPU-time 采样状态
- `task/limits.rs`
  - `getrlimit` / `setrlimit` / `prlimit64`
  - owner 在 `kprocess::ProcessRuntime` 持有的 process resource/rlimit 状态
- `task/umask.rs`
  - `umask`
  - owner 在 `kprocess::ProcessRuntime` 的文件创建掩码状态
- `task/sched.rs`
  - `sched_yield` / `sched_*affinity` / `sched_*scheduler` / `getcpu` / `getpriority` / `setpriority`
  - owner 在 `ktask` 调度接口、`kprocess` 进程/线程状态和 `khal` CPU 查询
  - `PRIO_PROCESS` 按 TID 选择单个 task；`PRIO_PGRP` 与 `PRIO_USER` 按每个已发布 task
    遍历，不能用进程代表线程代替 per-thread nice 或 real UID
  - `setpriority` 以调用者 effective UID 对比目标 real/effective UID；当前用 root 近似
    `CAP_SYS_NICE`，非特权调用者不能降低 nice 值来提高优先级
  - `sched_setaffinity` 同样要求 caller euid 匹配目标 ruid/euid，或 caller 为 root
    （近似 `CAP_SYS_NICE`）；错误序为 ESRCH → EPERM → EINVAL(empty mask)；
    随后 `ktask::set_task_affinity` 迁移离队，迁不走返回 EBUSY
- `sync/futex.rs`
  - `futex` / `get_robust_list` / `set_robust_list`
  - compound op（`REQUEUE` / `CMP_REQUEUE` / `WAKE_OP`）在单次
    `address_space` 锁内解析两个 key；robust-list 遍历在 `posix/process`

## 非目标

`ksyscall` 不负责：

- 保存 fd-backed object 的内部状态；
- 保存 `ProcessRuntime` / 地址空间 / VFS 节点等共享对象；
- 实现路径解析、signal 状态机、timer 状态机或 pipe buffer 行为；
- 提供“方便导入”的 catch-all owner 抽象。
