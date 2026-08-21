# anon_inodefs — 设计文档

## 定位

`anon_inodefs` 是内核匿名文件的具体伪文件系统。它拥有 `anon_inodefs` 的
`FileSystemType`、隐藏 mount、superblock、默认 dentry operations 和 singleton inode；
eventfd、epoll、timerfd、signalfd、pidfd、socket 与 BPF object 等调用者只请求创建
open file。KVFS 只提供通用 superblock、dentry、inode 和 `alloc_file_pseudo()` 机制，
不拥有该具体文件系统。

## 背景

Linux `fs/anon_inodes.c` 通过一个内核 mount 和一个共享 inode 为无需独立 inode 状态的
kernel object 创建 file。X-Kernel 保持相同语义层次，并用 `AnonInodeFs` 对象组合 Linux
中的全局 mount 与 singleton inode；静态 filesystem、superblock 和 dentry operation
table 不进入该对象，也不按 mount 或 file 重复分配。

## 范围

- `src/lib.rs`：隐藏文件系统初始化、共享 inode、动态 dentry 名称和 file 创建。
- `docs/design.md`：对象层次、启动顺序和生命周期。
- `docs/security.md`：输入边界、并发初始化与失效模式。

## 架构

```text
static FileSystemType / s_op / s_d_op
                 |
                 v
Once<AnonInodeFs> { hidden Mount, singleton VfsInode }
                 |
                 +-- get_file(name, fops, private, flags, cred)
                           |
                           v
                 per-call Dentry + VfsFile
                           |
                           +-- shared singleton inode
                           +-- caller-owned file private data
```

superblock 的默认 `s_d_op` 对应物使 root 和 `alloc_file_pseudo()` 创建的 dentry 自动继承
同一静态 table。`AnonInodeFs` 因而不保存 dentry-operation `Arc`，每个 file 也不保存
平行 operation owner。

## 调用约束 / 执行上下文

`init_anon_inodefs()` 必须在 boot 的可睡眠执行路径调用，早于普通任务或并行测试创建
匿名文件。初始化会分配 VFS 对象并取得 sleepable lock，不能在中断上下文或持有
spinlock 时调用。

`get_file()` 可以从普通 task context 调用。它需要分配 dentry/file，因此不能在中断
上下文调用。接口不反向读取 current task；调用者
显式传入 open credential snapshot，所以内核线程和测试可使用受控 credential。

## 状态机

```text
Uninitialized -- init_anon_inodefs() --> Published
Published     -- get_file() ----------> Published
Uninitialized -- global()/get_file() -> panic
```

没有运行期卸载状态。`Once` 发布后 mount 和 singleton inode 在系统生命周期内保持有效。

## 算法流程

初始化阶段：

1. 通过 KVFS `new_pseudo_super_block()` 以静态 `FileSystemType`、共享 simple `s_op`、
   `s_magic` 和默认 `s_d_op` 创建 superblock。
2. 通用 helper 创建 pseudo-fs root inode/dentry，并由 superblock 绑定默认 dentry operations。
3. 创建带 `NODEV | NOEXEC` 的隐藏 root mount。
4. 通过 KVFS `get_next_ino()` 创建一个无文件类型、权限为 `0600` 的共享匿名 inode。
5. 将 mount 与 inode 一次性发布到 `Once<AnonInodeFs>`。

创建 file 时只分配 per-file dentry/file，保留 `O_ACCMODE | O_NONBLOCK`，把调用者的
private object 安装到 `file->private_data` 对应物。`getattr` 从 path 的唯一 VFS inode
取得 metadata，再清除 file-type bits，保持 Linux 用户 ABI 对匿名 inode 的历史语义。

## 并发模型

初始化由 boot 串行执行；`Once` 负责发布可见性。发布后 `AnonInodeFs` 字段不可变，
并发 `get_file()` 只共享 mount 和 singleton inode，各自创建独立 dentry/file/private
state，不需要文件系统级 mutex。VFS 对象内部同步由 KVFS 负责。

## 设计决策

- 具体 `anon_inodefs` 位于 `fs/filesystems`，不作为 KVFS 模块或 facade re-export。
- `AnonInodeFs` 只组合 Linux 中确实存在的 hidden mount 与 singleton inode；operation
  tables 使用静态对象，不放入 runtime state。
- 所有普通匿名 file 共享 inode；需要独立 inode/security context 的 Linux
  `anon_inode_create_getfile()` 语义尚未实现，不能用复制 singleton state 伪装。
- 初始化显式发生在 boot，而不是在任意 runtime caller 的首次访问中执行复杂 VFS 构造。

## Drop / 资源释放

全局 filesystem 对象不在运行期释放。每个 `VfsFile` 释放时由自身 operation table 完成
kernel object cleanup，随后释放 per-file private `Arc`、dentry 和 file 引用；共享 inode
与 hidden mount 仍由 `AnonInodeFs` 持有。
