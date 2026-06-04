# kfd_objects - 设计文档

## 定位

`kfd_objects` 是 x-kernel 中 fd-backed kernel object 的 owner crate。
它承接那些通过进程 fd table 暴露给用户态、实现 `FileLike` / `Pollable`，
但本质上不属于路径/VFS 对象的数据和状态机。

当前该 crate 先承接 `TimerFd` 和 `EventFd`。
目标读者是维护 `ksyscall` syscall adapter、`kfd`/`kresources` fd table，
以及 `timerfd`、`eventfd`、`pipe`、`pidfd` 等匿名对象实现的开发者。

## 背景

历史上，`timerfd` 之类对象因为“通过 fd 暴露”而被顺手放进了
`posix-fs` 一类按 syscall/API 分类的 crate。
这会让资源 owner 和 syscall adapter 混在一起：

- syscall 层承担 ABI 参数和用户指针转换；
- 资源对象自身承担状态、不变量和生命周期；
- fd table 只负责持有和索引对象句柄。

`kfd_objects` 的目标是把第二层单独收出来，按对象状态边界组织实现。

## 范围

当前涉及的源文件：

```text
process/kfd_objects/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── eventfd.rs
│   └── timerfd.rs
└── docs/
    ├── design.md
    └── security.md
```

## 架构

```text
core/ksyscall adapter
    │ eventfd2 / timerfd_* ABI binding
    v
process/kfd_objects::{EventFd, TimerFd}
    │ owns timer/event state and FileLike/Pollable behavior
    v
kfd / kresources fd table
    │ stores Arc<dyn FileLike>
    v
read/poll/close via generic fd syscalls
```

## 设计原则

- syscall ABI 适配留在 `ksyscall`。
- `kfd_objects` 拥有对象状态、不变量和生命周期。
- `kfd`/`kresources` 只负责 fd 槽位和对象句柄，不拥有对象业务语义。
- 路径/VFS 相关逻辑不进入该 crate。

## `TimerFd` 角色

`TimerFd` 拥有：

- `clock_id`
- 当前 `deadline`
- 周期 `interval`
- 未消费的 `expirations`
- 注册到底层 timer runtime 的 handle
- `poll(IN)` 与 `read()` 的一致性语义

它不处理：

- Linux `itimerspec` 的 `copyin/copyout`
- syscall flag ABI 解析
- fd table 分配策略

这些分别留在 `ksyscall` adapter 和 `kresources`。

## `EventFd` 角色

`EventFd` 拥有：

- 当前 64-bit 计数值；
- semaphore 与普通累加两种读语义；
- 非阻塞模式；
- `poll(IN/OUT)` 的就绪状态与唤醒点。

它不处理：

- `eventfd2` flags ABI 解析；
- fd table 分配策略；
- syscall 层错误码和参数边界。
