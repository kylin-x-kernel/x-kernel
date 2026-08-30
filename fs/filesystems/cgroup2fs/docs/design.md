# cgroup2fs — 设计文档

## 定位

`cgroup2fs` 是 Linux cgroup v2 文件系统 ABI 适配器。层级、controller 状态和 task
membership 由 `kcgroup` 持有；进程查找和稳定 task identity 校验由 `kprocess` 持有。
本 crate 把 VFS 操作翻译成 owner API，并拥有每个 mount 的 VFS node registry；registry
只保存 inode identity/metadata，不复制 hierarchy 或 membership 状态。

## 范围

```text
src/
├── lib.rs         # crate 入口与公开导出
├── mount.rs       # filesystem type、FsContext、superblock
├── state.rs       # per-mount 稳定目录/控制文件 inode registry
├── dir.rs         # 动态目录枚举、lookup、mkdir、rmdir
├── control.rs     # 控制文件 mode
├── command.rs     # 内核缓冲区中的控制命令解析
├── controller.rs  # controller registry 与通用控制文件
├── pids.rs        # pids controller 文件适配
└── process.rs     # cgroup.procs 展示、授权与进程迁移
```

## 架构

```text
VFS
 │
 ├─ user mount ─────────────> current CgroupNamespace root
 ├─ fs_boot mount ──────────> initial CgroupNamespace root
 └─ CgroupFsState ──────────> stable VFS inode identity
     └─ dir.rs
         ├─ process.rs ─────> kprocess cgroup facade
         └─ controller.rs ──> pids.rs ─────> kcgroup
```

`state.rs` 为每个 `Arc<Cgroup>` 一次性构造目录 inode 和固定控制文件 inode。`dir.rs`
的重复 lookup 只建立或复用指向这些 inode 的 dentry。controller 文件可见性在每次
lookup/readdir 时从 canonical cgroup 状态判断。新增 controller 时在 state 的文件声明
和 `controller.rs` adapter 中增加对应项。

## 调用约束 / 执行上下文

普通用户挂载和文件操作要求当前 task 是用户进程线程，并且 scheduler、process
registry、VFS 和 cgroup namespace 已初始化。`new_initial_cgroup2fs()` 仅用于 `fs_boot`
的启动期挂载，不要求 current process。路径使用 sleepable lock，不能在 IRQ、softirq
或持有 spinlock 的上下文调用。输入已经由 VFS 复制到内核；本 crate 不访问用户指针。

## 算法流程

### 挂载

普通 mount 从当前进程的 `CgroupNamespace` 取得 view root。`KFEAT_FS_CGROUP2=y` 时，
`fs_boot` 在用户进程创建前把 initial namespace hierarchy 挂到 `/sys/fs/cgroup`。每个
superblock 创建一个 `CgroupFsState` 并首先物化稳定 root inode。

### 控制文件

`command.rs` 拒绝非 UTF-8、嵌入 NUL、空 `subtree_control` 命令和缺少 `+`/`-` 前缀的
操作。controller registry 在 mutation 前解析所有名称。所有可写控制文件使用 KVFS
`CommandFile`：一次 write 最多 4096 字节并作为独立命令处理，file offset 和此前写入
不参与拼接。`pids.max` 只接受 `max` 或 Linux PID domain 内的数值。

### 目录与 inode identity

`mkdir` 使用 VFS 传入的 parent inode、准备后 mode 与 credential，通过
`inode_init_owner()` 初始化 owner。`rmdir` 使用已加锁 victim dentry，并验证它引用的
inode 与 registry 中目标 cgroup inode 相同后才删除。节点删除后 registry 保留 tombstone
inode 到 unmount；旧 fd 继续指向旧 identity，同名重建会得到新 cgroup 和新 inode。

### 进程展示与迁移

`process.rs` 通过 `kprocess::cgroup_member_process_ids()` 获取去重 PID。facade 验证
registry task 与 membership 的 `Arc<PidHandle>` identity，避免 TID 重用错误映射。

写 `cgroup.procs` 时，`kprocess` 在 process cgroup gate 内把稳定 source、destination
交给 adapter。adapter 验证两端均位于 mount view root 下，求最近共同祖先，并使用
打开 fd 时保存的 credential 检查共同祖先 `cgroup.procs` inode 写权限；通过后才提交
整组迁移。

## 并发模型

`CgroupFsState` 用 mutex 保护 node registry，并以 `Weak<SimpleFs>` 避免引用环。每个
控制文件操作先取得 `kcgroup` operation guard：guard 存活期间 removal 返回 `EBUSY`，
节点删除后新操作返回 `ENODEV`。层级事务由 `kcgroup` 序列化，进程 publication 与
整组迁移由 `kprocess` 序列化。目录 dentry cache 保持关闭，避免 controller 激活后旧的
negative lookup 隐藏新可见文件；每次 lookup 仍复用 registry 中的 stable inode identity。

## 设计决策

- per-mount registry 属于 VFS adapter，而不是 `Cgroup`，避免 core hierarchy 依赖文件系统。
- hierarchy/controller mutation 使用 VFS directory/control-file DAC，不叠加隐藏的
  current-process 管理员判断。
- `cgroup.procs` 额外执行 mount view 与共同祖先写权限检查，使用 opener credential。
- 启动挂载显式选择 initial view，不在缺少 current process 时隐式 fallback。

## Drop / 资源释放

registry 持有 stable inode 和 cgroup 节点直到 superblock 释放。tombstone 防止旧 fd
identity 被同名重建复用；membership、controller charge 和 hierarchy 生命周期仍由
`kcgroup` 管理。
