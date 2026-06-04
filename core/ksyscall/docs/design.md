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
│   ├── sys.rs
│   ├── task/
│   │   ├── clone.rs
│   │   ├── clone3.rs
│   │   ├── ctl.rs
│   │   ├── execve.rs
│   │   ├── exit.rs
│   │   ├── job.rs
│   │   ├── mod.rs
│   │   ├── pidfd.rs
│   │   ├── thread.rs
│   │   └── wait.rs
│   ├── time/
│   │   ├── mod.rs
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
    ├─ ipc adapter  ───────────> posix-fs::pipe / kfd_objects::EventFd
    ├─ time adapter ───────────> kfd_objects::TimerFd / posix-time
    ├─ task adapter ───────────> kthread / posix-process / kprocess
    ├─ io_mpx adapter ─────────> posix-io-mpx
    ├─ sync adapter ───────────> posix-sync / kthread
    └─ misc adapter ───────────> posix-mm / posix-net / posix-signal / ...
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
  - owner 在 `posix-fs::PipeObject`
- `ipc/eventfd.rs`
  - `eventfd2`
  - owner 在 `kfd_objects::EventFd`
- `time/timerfd.rs`
  - `timerfd_*`
  - owner 在 `kfd_objects::TimerFd`
- `task/pidfd.rs`
  - `pidfd_*`
  - owner 在 `kthread::PidFd`

## 非目标

`ksyscall` 不负责：

- 保存 fd-backed object 的内部状态；
- 保存 `ProcessState` / 地址空间 / VFS 节点等共享对象；
- 实现路径解析、signal 状态机、timer 状态机或 pipe buffer 行为；
- 提供“方便导入”的 catch-all owner 抽象。
