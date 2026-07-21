# kcred - 设计文档

## 定位

`kcred` 是 x-kernel 的 Linux/POSIX 凭据数据与转换策略 crate。它定义 `Cred`，维护
real、effective、saved 和 filesystem UID/GID 以及补充组，并实现 set-ID 与 exec
相关的状态转换。

`kcred` 不知道“当前线程”，也不保存进程或线程对象。当前任务的凭据指针由上层
`kprocess::Thread` 持有；下层 `kvfs` 只依赖 `kcred`，通过显式 `&Cred` 参数执行 DAC。
这保持了依赖方向：

```text
kprocess  --->  kcred  <---  kvfs
   |                         ^
   +-- syscall snapshots ----+
```

## 范围

```text
process/kcred/
├── src/
│   ├── lib.rs                  # 公开类型、initial_cred
│   ├── credentials/
│   │   ├── mod.rs
│   │   ├── model.rs            # Cred 与状态转换
│   │   ├── user.rs             # Uid
│   │   └── group.rs            # Gid
│   ├── namespace.rs            # user namespace 身份类型
│   └── tests.rs
└── docs/
    ├── design.md
    └── security.md
```

## 对象模型

```text
Thread
  real_cred: RwLock<Arc<Cred>>   objective credential
  cred:      RwLock<Arc<Cred>>   subjective credential
                         |
                         | Arc snapshot
                         v
Cred
  ruid/euid/suid/fsuid
  rgid/egid/sgid/fsgid
  supplementary_groups: Arc<[Gid]>
```

已提交的凭据始终以 `Arc<Cred>` 存在，并按不可变对象使用。修改遵循 Linux
`prepare_creds()` / `commit_creds()` 的两阶段模型：

1. `CurrentThread::prepare_creds()` 从当前 subjective credential 克隆出普通 `Cred`。
2. 调用者在未发布副本上执行完整转换与校验。
3. 成功后 `CurrentThread::commit_creds()` 创建新的 `Arc<Cred>`，同时替换当前
   `real_cred` 和 `cred`。
4. 已持有旧 `Arc<Cred>` 的并发操作继续看到稳定旧快照，不会观察到半更新字段。

当前尚未实现临时 override credential，因此 `real_cred` 与 `cred` 在提交前必须指向
同一对象；分开保存这两个 Linux 语义角色，是支持 objective/subjective 查询所必需，
不是 VFS 的附加状态。

## 文件访问凭据

普通文件访问直接使用 committed `Cred` 的 `fsuid/fsgid` 和补充组。
`access(2)` 的身份选择仍复用同一种对象表达：

- `Cred::for_access()`：把副本的 filesystem IDs 设为 real IDs；
- `AT_EACCESS`：直接使用当前 committed credential，不改写 filesystem IDs；
- 普通 open、namei 和元数据变更：使用当前 committed credential 的 filesystem IDs。

Linux 的 VFS 始终以 `fsuid/fsgid` 做 DAC；`AT_EACCESS` 只是取消 `access(2)` 默认的
real-ID override。因此显式 `setfsuid/setfsgid` 后，`AT_EACCESS` 仍使用修改后的
filesystem IDs，不创建一份 effective-ID credential。

没有单独的 `AccessCredentials`，也不在 `Nameidata` 中增加 credential 字段。syscall
入口取得一次 `Arc<Cred>`，再把 `&Cred` 沿该次完整路径解析和权限检查逐层传递。
这样既让一次操作使用一致快照，也避免 `kvfs` 反向依赖 `kprocess::current_cred()`。

打开文件时，`VfsFile` 保存该 `Arc<Cred>`，对应 Linux `file::f_cred`。descriptor-based
操作首先依赖 open file 的访问模式；pathname-based 操作继续显式接收调用时凭据。

## 状态转换

### UID/GID

- 当前简化特权判定为 `euid == 0`，代替尚未实现的 `CAP_SETUID/CAP_SETGID`。
- 特权 `setuid/setgid` 可同时更新 real、effective、saved 和 filesystem ID。
- 非特权 `setuid/setgid` 只能把 effective/filesystem ID 设为当前 real 或 saved ID；
  当前 effective ID 本身不是额外的合法目标。
- `setreuid/setregid` 和 `setresuid/setresgid` 按各自的 Linux 规则更新 saved ID。
  `setreuid/setregid` 每次成功都让 filesystem ID 跟随最终 effective ID，包括两个参数
  都为 `-1` 的调用。
- 被拒绝的 checked 转换返回 `KError::OperationNotPermitted`，且不发布副本。
- `setfsuid/setfsgid` 始终返回旧值；目标不允许时保持状态不变。
- `apply_exec()` 令 saved IDs 跟随 effective IDs。

### 补充组

`set_supplementary_groups` 在替换前执行排序，并保存为 `Arc<[Gid]>`。
`Cred::in_group()` 先比较 `fsgid`，再对补充组执行二分查找。副本共享不可变数组，
只有真正替换补充组时才分配新数组。

### 初始凭据

`initial_cred()` 通过 `Once<Arc<Cred>>` 发布全局 root credential，供初始任务和内核创建
的 VFS 对象共享。普通用户任务的当前身份仍只从其 `Thread` 读取。

## 并发模型

`kcred` 内部没有锁。`Cred` 的已提交实例不可变，发布和替换由 `kprocess` 的
`RwLock<Arc<Cred>>` 串行化。读取者只克隆 `Arc`，无需在路径遍历期间持有线程锁。

补充组数组也是不可变 `Arc`；因此 prepare 阶段修改普通 `Cred` 不会影响任何已提交
快照。一次 namei、exec 或 access 操作应在入口只获取一次 credential snapshot。

## 设计决策

- 凭据数据和转换策略属于 `kcred`，当前任务定位属于 `kprocess`。
- VFS 显式接收 `&Cred`，不引入全局 current hook 或向上依赖。
- access 身份选择复用 `Cred`，不增加只为搬运相同字段的结构体。
- `Nameidata` 只保存路径解析状态；调用上下文由方法参数表达。
- 错误直接使用内核统一 `KError`，不增加只做一对一映射的错误枚举。

## Drop / 资源释放

`Cred` 没有自定义 `Drop`。线程替换凭据后，旧对象在最后一个任务、打开文件或正在
执行的操作释放其 `Arc` 时自动销毁；补充组数组同样按最后一个引用释放。

## 已知限制

1. 尚无 capability 和 securebits；特权转换使用 `euid == 0` 近似。
2. user namespace 类型已经存在，但 UID/GID 转换与 VFS idmapping 尚未接入。
3. 尚无临时 subjective credential override。
4. 尚未实现 setuid/setgid executable 和 file capability。
5. `NGROUPS_MAX` 输入上限由 syscall 层负责。

## 审计清单

- set-ID 改动是否保持 real/effective/saved/filesystem ID 关系。
- 所有失败转换是否发生在 `commit_creds()` 之前。
- 新的多步安全操作是否只取得一次 `Arc<Cred>` 快照。
- 补充组替换是否继续保持排序不变量。
- VFS 入口是否显式接收 `&Cred`，且没有依赖 `kprocess`。
- capability、namespace 或 override credential 接入时是否同时更新两类凭据角色。
