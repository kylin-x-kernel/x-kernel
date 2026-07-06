# kns — 设计文档

## 定位

`process/kns` 是 X-Kernel 的进程 namespace 引用集合 crate。它提供类似
Linux `struct nsproxy` 的 `NsProxy`，把一个进程当前使用的 mount、UTS、IPC、PID、
network、user、cgroup 和 time namespace 引用集中管理。

`kthread::ProcessState` 持有 `Arc<NsProxy>`，syscall 层通过 `kthread` 暴露的窄
API 读取当前进程的 namespace。`kns` 不解析 syscall ABI，也不直接调度 syscall。

## 背景

引入 `kns` 之前，进程的 filesystem context、UTS 名称、IPC 状态和各种未来
namespace 规划点分散在进程状态、POSIX syscall 模块或全局静态对象中。这让
`clone(CLONE_NEW*)` 很难按 namespace 类型创建或共享状态，也容易把尚未实现的
namespace flag 静默忽略。

当前实现先建立清晰的所有权边界：

- `NsProxy` 负责聚合各类 namespace 引用；
- `MntNamespace` 包装 `FsContext`；
- `UtsNamespace` 持有 per-namespace hostname 和 domainname；
- `IpcNamespace`、`PidNamespace`、`NetNamespace`、`UserNamespace`、`TimeNamespace`
  先提供身份占位；
- `CgroupNamespace` 已迁出到 `process/kcgroup`，`kns` 只持有并重导出该类型。

## 范围

本文档覆盖：

- `src/lib.rs`
- `src/nsproxy.rs`
- `src/types.rs`
- `src/mnt.rs`
- `src/uts.rs`
- `src/ipc.rs`
- `src/pid.rs`
- `src/net.rs`
- `src/user.rs`
- `src/time.rs`
- `process/kcgroup/src/lib.rs` 中被 `NsProxy` 持有的 cgroup namespace 骨架

`clone(2)` ABI 解析位于 `core/ksyscall`，进程状态接入位于 `process/kthread`，
不属于本 crate 的实现范围。

## 架构

```text
core/ksyscall
    |
    v
process/kthread::ProcessState
    |
    v
process/kns::NsProxy
    |-- MntNamespace  -> kfs::FsContext
    |-- UtsNamespace  -> RwLock<UtsInner>
    |-- IpcNamespace  -> phase-one identity placeholder
    |-- PidNamespace  -> pid_ns_for_children placeholder
    |-- NetNamespace  -> placeholder
    |-- UserNamespace -> root/parent placeholder
    |-- kcgroup::CgroupNamespace
    `-- TimeNamespace -> placeholder
```

`NamespaceFlags` 映射 Linux `CLONE_NEW*` flag。`NamespaceId` 为 `kns` 内部
namespace 分配生命周期内稳定的 ID，用于 procfs namespace 展示等后续能力。
`kcgroup` 使用独立的 `CgroupNamespaceId`，避免让 cgroup 子系统反向依赖 `kns`。

## 调用约束 / 执行上下文

`kns` API 面向普通进程/任务上下文。`NsProxy::clone_for_child` 会锁住父
`MntNamespace` 中的 `FsContext` 并 clone 其内容，因此调用方不能在持有会与 VFS
或进程状态形成反向依赖的锁时调用它。

当前 API 不要求中断上下文可用，也不设计为 early boot 原语。初始 `NsProxy` 在
进程状态创建时由调用方传入 `FsContext` 构造。UTS 名称读写使用 `ksync::RwLock`，
不应在禁止睡眠或已经持有不兼容自旋锁的路径中调用写入接口。

## 算法流程

`NsProxy::new_initial` 创建 init 进程使用的初始 namespace 集合：

1. 用传入的 `FsContext` 创建 `MntNamespace`。
2. 创建默认 `UtsNamespace`。
3. 创建 IPC、PID、net、user、cgroup、time 的初始对象或占位对象。
4. 将这些引用封装进一个 `Arc<NsProxy>`。

`NsProxy::clone_for_child(flags, share_fs)` 是 clone 路径的核心选择器：

1. 若包含尚未实现的 `NEWNET`、`NEWUSER`、`NEWCGROUP`、`NEWPID` 或 `NEWTIME`，
   返回 `CloneNsError::Unimplemented`，由 syscall 层映射为 `ENOSYS`。
2. 若 `NEWNS` 与 `share_fs` 同时出现，返回 `CloneNsError::InvalidFlagCombination`。
3. Mount namespace：
   - `NEWNS`：clone 父 `FsContext` 并创建新的 `MntNamespace`；
   - `share_fs`：共享同一个 `MntNamespace`；
   - 普通 fork：clone 父 `FsContext` 并创建新的 `MntNamespace`。
4. UTS namespace：`NEWUTS` 时复制当前名称并创建新 namespace，否则共享。
5. IPC namespace：`NEWIPC` 时创建新的空占位 namespace，否则共享。
6. 其他已占位 namespace 当前保持共享。

## 并发模型

`NsProxy` 自身不可变，生命周期由 `Arc` 管理。替换当前进程 namespace bundle 的写锁
位于 `kthread::ProcessState`，`kns` 不直接持有该锁。

`UtsNamespace` 用 `RwLock<UtsInner>` 保护 hostname 和 domainname。`read_names_into`
在一次读锁内复制两个字段，避免 `uname()` 热路径为两个名字分别分配临时 `Vec`。

`NamespaceId` 使用 `AtomicU64` 和 `Relaxed` 分配唯一 ID。该 ID 只要求唯一递增，
不作为跨字段同步信号，因此不需要 acquire/release 顺序。

## 设计决策

- `kns` 聚合 namespace 引用，但不承载 cgroup 控制器状态。cgroup namespace 身份
  已放入 `kcgroup`，给后续 hierarchy/controller 逻辑留下独立所有权边界。
- 未实现 namespace flag 显式返回错误，不静默共享全局状态。这样测试不会误判隔离
  已经生效。
- 第一阶段的 mount namespace 只隔离 `FsContext` 的 root/cwd 所有权，底层 mount tree
  仍可能共享。完整 mount tree copy 和 propagation 需要在 VFS 层继续设计。
- PID namespace 当前只保留 `pid_ns_for_children` 占位，不改变 `getpid`、`wait`、
  `kill` 等进程 API 的全局 PID 语义。

## Drop / 资源释放

`NsProxy` 和各 namespace 由 `Arc` 管理。释放 `NsProxy` 只会释放它持有的引用；
被其他进程、future namespace fd、mount、mapping 或 socket 继续持有的对象由各自
引用计数决定生命周期。

当前 placeholder namespace 没有额外资源回收逻辑。未来 IPC、mount、net 或 cgroup
状态迁入后，资源清理由对应 namespace 类型本身负责，而不是由 `NsProxy` 统一清理。
