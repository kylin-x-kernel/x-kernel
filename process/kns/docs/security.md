# kns — 安全与可靠性分析

## 信任模型

`kns` 信任内核调用方已经完成 syscall ABI 解析和基本 flag 校验。用户态不能直接构造
`NsProxy` 或 namespace 对象；用户输入只能通过 `clone(2)`、后续 `unshare(2)` /
`setns(2)`、UTS 名称 syscall 和 procfs namespace 视图间接影响本 crate。

`kns` 的职责是在已解析的内核语义值上维护 namespace 引用关系。权限检查、用户指针
copy-in/copy-out 和 errno 映射属于 syscall 层或更上层策略代码。

## 外部边界 / 攻击面

主要边界来自：

- `clone(CLONE_NEW*)` flag 组合；
- UTS hostname/domainname 内容和长度；
- 未来 procfs namespace fd、`setns` 和 `unshare` 路径；
- mount namespace 对 `FsContext` 的复制与共享语义。

本 crate 不直接访问用户内存、MMIO/PIO、DMA、FFI 或 inline assembly。用户指针必须在
进入 `kns` 前由 `kuaccess`/syscall 层复制成内核拥有的字节或标志值。

## unsafe 代码清单

当前 `kns` 只有 `src/uts.rs::bytes_from_uts` 使用 `unsafe`，把 `[c_char]` 的前缀视作
`[u8]`：

- 输入 slice 来自 `UtsInner` 内部固定数组；
- `c_char` 在目标平台上是 `i8` 或 `u8`，大小和对齐与 `u8` 相同；
- 长度由首个 NUL 或数组长度限制，保持在原 slice allocation 内；
- 写入路径只保存 ASCII 字节，避免 signedness 改变造成语义歧义。

`kcgroup` 当前没有 `unsafe`。

## 内存安全不变量

- `NsProxy` 字段均为 `Arc<T>`，共享 namespace 不转移所有权。
- `ProcessState` 替换 `NsProxy` 时必须一次性发布完整的新 `Arc<NsProxy>`，不能暴露半初始化 bundle。
- `UtsInner` 的两个固定数组始终 NUL 初始化或由 ASCII 字节填充；setter 拒绝长度达到
  65 字节的输入，保留 NUL 终止空间。
- `MntNamespace` 只暴露 `Arc<Mutex<FsContext>>` 引用，调用方必须通过锁访问可变 filesystem context。

## 线程安全

`NsProxy` 本身是不可变引用集合，适合多进程/多线程通过 `Arc` 共享。内部可变状态由各
namespace 自己同步：

- UTS 名称通过 `RwLock` 同步；
- mount filesystem context 通过 `Mutex<FsContext>` 同步；
- placeholder namespace 当前没有共享可变 payload；
- ID 分配使用原子递增，仅用于唯一身份，不承载同步语义。

调用方不应在持有 `ProcessState.nsproxy` 写锁时执行可能阻塞或递归进入 VFS/IPC 的操作。
构造新 bundle 应先在锁外完成，再短暂交换指针。

## 威胁分析

- **静默忽略 namespace flag**：会让用户态误以为获得隔离。当前对未实现 namespace 返回
  `CloneNsError::Unimplemented`，由 syscall 层映射为显式错误。
- **非法 flag 组合导致共享语义混乱**：`NEWNS` 与 `CLONE_FS` 冲突在 `kns` 中拒绝，
  其他组合由 syscall 校验层负责。
- **UTS 名称越界或非终止字符串**：setter 限制最大长度并重置缓冲区，保证 NUL 终止空间。
- **mount namespace 过度承诺**：当前只隔离 root/cwd 的 `FsContext`，不承诺 mount tree
  mutation 隔离。文档和测试必须保留这个阶段限制。

## 故障模式与影响分析（FMEA）

| 故障模式 | 影响 | 缓解 |
| --- | --- | --- |
| 未实现 namespace flag 被接受 | 隔离失效且难以发现 | `clone_for_child` 返回 `Unimplemented` |
| `NEWNS` 与 `CLONE_FS` 同时接受 | child root/cwd 共享语义不明确 | 返回 `InvalidFlagCombination` |
| UTS 名称长度越界 | 固定数组越界或缺少 NUL | setter 拒绝超长输入 |
| mount tree 仍共享 | `mount`/`umount` 可跨 namespace 可见 | 作为已知限制记录，后续由 VFS mount tree copy 修复 |
| ID 计数回绕 | procfs namespace 身份可能重复 | 现实中极难触发；未来可在分配器中加入回绕检测 |

## 故障管理

`kns` 使用 `Result` 报告可恢复错误：

- `CloneNsError::InvalidFlagCombination`
- `CloneNsError::Unimplemented`
- `UtsError::NameTooLong`

本 crate 不直接选择 errno。syscall 层应把非法组合映射为 `EINVAL`，把已知但未实现的
namespace 映射为 `ENOSYS`，把 UTS 名称过长映射为 ABI 要求的错误。

## 隐私分析

`kns` 不处理用户数据内容，除 UTS hostname/domainname 外不保存来自用户态的字符串。
UTS 名称本身是系统公开状态，通常可通过 `uname` 或 procfs 观察。未来 cgroup、PID、
user namespace 接入后，需要重新审计路径视图、PID 可见性和 credential 映射是否泄露
宿主全局状态。

## 已知限制

- `NEWPID`、`NEWNET`、`NEWUSER`、`NEWCGROUP`、`NEWTIME` 当前只建模类型或骨架，clone
  路径返回未实现。
- `MntNamespace` 尚未复制 mount tree，不能提供完整 Linux mount namespace 隔离。
- `IpcNamespace` 目前只是身份占位，SysV IPC manager 迁移需要后续补丁完成。
- `setns`、namespace fd 和完整 `/proc/[pid]/ns/*` 语义尚未由本 crate 闭环。
- user namespace 权限模型尚未接入，后续 capability 检查不能散落在 syscall 调用点。

## 审计清单

- [ ] 新增 `CLONE_NEW*` 处理时，确认未实现语义不会静默共享全局状态。
- [ ] 修改 `NsProxy::clone_for_child` 时，重新检查 `CLONE_FS`、`NEWNS`、普通 fork 的
      `FsContext` 共享/复制关系。
- [ ] 修改 UTS 字符串表示时，重新审计 NUL 终止、长度限制和 `c_char`/`u8` 转换。
- [ ] 添加 namespace 内部可变状态时，明确锁类型、调用上下文和 drop 行为。
- [ ] 将 cgroup controller 或 hierarchy 状态接入时，优先放入 `kcgroup`，避免把
      `kns` 扩成资源管理 catch-all。
- [ ] 接入 procfs/setns 时，检查权限、fd 类型匹配、多线程限制和引用生命周期。
