# kns — 设计文档

## 定位

`process/kns` 是 X-Kernel 的进程 namespace 引用集合 crate。它提供类似
Linux `struct nsproxy` 的 `NsProxy`，把一个进程当前使用的 mount、UTS、IPC、
PID-for-children、network、cgroup 和 time namespace 引用集中管理。

`kprocess::ProcessRuntime` 持有 `Arc<NsProxy>`，syscall 层通过 `kprocess` 暴露的窄
API 读取当前进程的 namespace。`kns` 不解析 syscall ABI，也不直接调度 syscall。

## 背景

引入 `kns` 之前，进程的 filesystem state、UTS 名称、IPC 状态和各种未来
namespace 规划点分散在进程状态、POSIX syscall 模块或全局静态对象中。这让
`clone(CLONE_NEW*)` 很难按 namespace 类型创建或共享状态，也容易把尚未实现的
namespace flag 静默忽略。

当前实现先建立清晰的所有权边界：

- `NsProxy` 负责聚合 Linux `struct nsproxy` 语义内的 namespace 引用；
- `kvfs::MntNamespace` 是唯一的 mount namespace 对象，拥有 mount tree、mount
  生命周期、可见性规则和所属 `kcred::UserNamespace`；
- 进程 root/pwd/umask 所在的 `fs_context::FsStruct` 属于
  `kprocess::ProcessRuntime`，不放入 `NsProxy`；
- 当前 user namespace 属于 credentials；task-active PID namespace 属于 task/PID
  identity，二者都不放入 `NsProxy`；
- `UtsNamespace` 持有 per-namespace hostname 和 domainname；
- `IpcNamespace`、`PidNamespace`、`NetNamespace`、`TimeNamespace` 先提供身份占位；
- `CgroupNamespace` 已迁出到 `process/kcgroup`，`kns` 只持有并重导出该类型。

## 范围

本文档覆盖：

- `src/lib.rs`
- `src/nsproxy.rs`
- `src/types.rs`
- `src/uts.rs`
- `src/ipc.rs`
- `src/pid.rs`
- `src/net.rs`
- `src/time.rs`
- `process/kcred/src/namespace.rs` 中被 `kvfs::MntNamespace` 引用的 user namespace
  身份；进程当前 user namespace 由 credentials 持有，不属于 `NsProxy`；
- `fs/kvfs/src/mount.rs` 中被 `NsProxy` 持有的 `kvfs::MntNamespace`；
- `process/kcgroup/src/lib.rs` 中被 `NsProxy` 持有的 cgroup namespace 骨架

`clone(2)` ABI 解析位于 `core/ksyscall`，进程状态接入位于 `process/kprocess`，
不属于本 crate 的实现范围。

## 架构

```text
core/ksyscall
    |
    v
process/kprocess::ProcessRuntime
    |
    v
process/kns::NsProxy
    |-- kvfs::MntNamespace -> mount tree + owning kcred::UserNamespace
    |-- UtsNamespace  -> RwLock<UtsInner>
    |-- IpcNamespace  -> phase-one identity placeholder
    |-- PidNamespace  -> pid_ns_for_children
    |-- NetNamespace  -> placeholder
    |-- kcgroup::CgroupNamespace
    `-- TimeNamespace -> time_ns / time_ns_for_children
```

`NamespaceFlags` 映射 Linux `CLONE_NEW*` flag。`NamespaceId` 由 `kcred` 提供，
为 namespace 分配生命周期内稳定的 ID，用于 procfs namespace 展示等后续能力。
`kcgroup` 使用独立的 `CgroupNamespaceId`，避免让 cgroup 子系统反向依赖 `kns`。

## 调用约束 / 执行上下文

`kns` API 面向普通进程/任务上下文。`NsProxy::clone_for_child` 选择各 namespace
引用的共享或复制关系；当调用方请求 `CLONE_NEWNS` 时，必须传入私有 `FsStruct`，
以便 mount namespace copy 后同步 retarget root/pwd。

当前 API 不要求中断上下文可用，也不设计为 early boot 原语。初始 `NsProxy` 在
`kprocess::ProcessRuntime` 创建时和 `fs_context::FsStruct` 分别构造并挂到 runtime。
UTS 名称读写使用 `ksync::RwLock`，不应在禁止睡眠或已经持有不兼容自旋锁的路径中调用写入接口。

## 算法流程

`NsProxy::new_initial` 创建 init 进程使用的初始 namespace 集合：

1. 取得 boot 阶段由 VFS 初始化的初始 `kvfs::MntNamespace`。
2. 创建默认 `UtsNamespace`。
3. 创建 IPC、PID-for-children、net、time/time-for-children 的初始对象或占位对象，并
   复用 `kcgroup::CgroupNamespace::initial()`；该对象已可被启动期 cgroup2fs 挂载使用。
4. 将这些引用封装进一个 `Arc<NsProxy>`。

`NsProxy::clone_for_child(flags, fs_context)` 是 clone 路径的核心选择器：

1. 若包含尚未实现的 `NEWNET`、`NEWUSER`、`NEWCGROUP`、`NEWPID` 或 `NEWTIME`，
   返回 `CloneNsError::Unimplemented`，由 syscall 层映射为 `ENOSYS`。
2. 若 `NEWNS` 与 shared filesystem context 同时出现，返回
   `CloneNsError::InvalidFlagCombination`，对应 Linux `CLONE_NEWNS | CLONE_FS` 冲突。
3. Mount namespace：`NEWNS` 时从父 `kvfs::MntNamespace` 复制 mount tree，创建新
   `kvfs::MntNamespace`，并把调用方的 `FsStruct.root`/`FsStruct.pwd` retarget 到新
   mount tree；否则共享父 `kvfs::MntNamespace`。
4. UTS namespace：`NEWUTS` 时复制当前名称并创建新 namespace，否则共享。
5. IPC namespace：`NEWIPC` 时创建新的空占位 namespace，否则共享。
6. PID namespace：
   - `NsProxy` 只保存 `pid_ns_for_children`；
   - task-active PID namespace 由 task/PID identity 表达，不属于 `NsProxy`；
   - `CLONE_NEWPID` 当前返回 `ENOSYS`，因为 `getpid`、`wait`、`kill`、procfs、
     registry lookup 还没有全部切成 namespace-aware。
7. Time namespace：同时保存 `time_ns` 与 `time_ns_for_children`，但 `CLONE_NEWTIME`
   当前仍返回未实现。
8. 其他已占位 namespace 当前保持共享。

## 并发模型

`NsProxy` 自身不可变，生命周期由 `Arc` 管理。替换当前进程 namespace bundle 的写锁
位于 `kprocess::ProcessRuntime`，`kns` 不直接持有该锁。

`kvfs::MntNamespace` 强拥有其 mount set，父 mount 的 child map 只作为可见性索引，
不负责生命周期；这对应 Linux 中 mount namespace 拥有 mount list、路径解析从当前
mount 查找子 mount 的层次。

`UtsNamespace` 用 `RwLock<UtsInner>` 保护 hostname 和 domainname。`read_names_into`
在一次读锁内复制两个字段，避免 `uname()` 热路径为两个名字分别分配临时 `Vec`。

`NamespaceId` 使用 `AtomicU64` 和 `Relaxed` 分配唯一 ID。该 ID 只要求唯一递增，
不作为跨字段同步信号，因此不需要 acquire/release 顺序。

## 设计决策

- `kns` 聚合 namespace 引用，但不承载 cgroup 控制器状态。cgroup namespace 身份
  已放入 `kcgroup`，给后续 hierarchy/controller 逻辑留下独立所有权边界。
- 未实现 namespace flag 显式返回错误，不静默共享全局状态。这样测试不会误判隔离
  已经生效。
- User namespace 按 Linux 层次放在 credentials 语义下；`MntNamespace` 仅保存拥有该
  mount namespace 的 user namespace，`NsProxy` 不重复保存当前 user namespace。
- Mount namespace 按 Linux 层次表达：`NsProxy` 直接持有 `kvfs::MntNamespace`，
  VFS 层拥有 mount tree 和 mount 生命周期；`FsStruct` 独立保存 root/pwd/umask。
  `CLONE_NEWNS` 复制 mount tree 并同步 retarget `FsStruct`，普通 fork 共享
  `kvfs::MntNamespace`。
- 初始 mount namespace 由 boot/VFS 初始化并绑定 initial user namespace；
  `kvfs::MntNamespace` 的结构根是 VFS 内部 `nullfs`，可见 `/` 是覆盖其上的
  mutable rootfs。`NsProxy::new_initial` 只引用该对象。这对应 Linux 中
  `init_mnt_ns` 在 VFS mount 初始化后挂到 `init_task.nsproxy->mnt_ns`，而
  `fs_struct.root/pwd` 指向 topmost rootfs 的行为。
- 初始 cgroup namespace 由 `kcgroup` 持有，`NsProxy::new_initial` 只复用其 `Arc`。
  因此启动挂载与 PID 1 不会各自创建互不相干的 hierarchy。
- mount propagation、shared/slave/private 传播组和 `setns` 尚未实现，因此当前 copy
  只覆盖基本 mount tree 隔离。
- PID namespace 的对象图和编号链已经建立，但 syscall ABI 仍保持 root/global PID
  语义。`NsProxy` 只保存 `pid_ns_for_children`；完整 `CLONE_NEWPID` 支持要等
  task-active PID namespace、lookup、signal、wait、procfs 一起 namespace-aware
  后再打开。

## Cgroup namespace clone

`NsProxy` 保留 `CgroupNamespace` 对象模型，但 `CLONE_NEWCGROUP` 当前与其他尚未闭环的
namespace flag 一样返回 `Unimplemented`。创建新 view 需要调用者 user namespace 中的
统一 capability 授权；在 `kcred` 提供该授权上下文前，clone 层不能用 UID 临时判断，
也不能向用户态宣称隔离已经生效。普通 fork/clone 继续共享父 namespace 的 `Arc`。

## Drop / 资源释放

`NsProxy` 和各 namespace 由 `Arc` 管理。释放 `NsProxy` 只会释放它持有的引用；
被其他进程、future namespace fd、mount、mapping 或 socket 继续持有的对象由各自
引用计数决定生命周期。

Mount tree 由 `kvfs::MntNamespace` 强拥有；父 mount 到子 mount 的可见性索引使用弱引用，
避免 namespace、parent 和 child 之间形成引用环。

当前 placeholder namespace 没有额外资源回收逻辑。未来 IPC、net 或 cgroup 状态迁入后，
资源清理由对应 namespace 类型本身负责，而不是由 `NsProxy` 统一清理。
