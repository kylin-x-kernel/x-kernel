# pipefs — 设计文档

## 定位

`pipefs` 拥有匿名 pipe 的具体伪文件系统：`pipefs` 类型、隐藏 mount、superblock、默认
dentry operations，以及每次创建时的 pipe inode。KVFS 的 `pipe` 模块保留匿名 pipe 与
pathname FIFO 共用的数据通路、`PipeObject` 和共享 file operations；syscall 层只调用
`pipefs::create_pipe_files()`。

## 背景

Linux `fs/pipe.c` 为匿名 pipe 使用独立 `pipefs`，而不是 `anon_inodefs`。独立 inode 保存
`pipe_inode_info`，动态名称为 `pipe:[ino]`。X-Kernel 采用相同所有权：每条匿名 pipe 有
唯一 VFS inode，inode private data 与两个 file 的 private data 指向同一个 `PipeObject`。

## 范围

- `src/lib.rs`：pipefs 初始化、pipe inode/file 创建和动态 dentry 名称。
- `kvfs::pipe`：共享 pipe/FIFO 数据结构与 file operation 算法，不属于本 crate 的
  filesystem instance state。
- `docs/design.md`、`docs/security.md`：生命周期和可靠性契约。

## 架构

```text
static pipefs type / s_op / s_d_op
                 |
                 v
       Once<PipeFs> { hidden Mount }
                 |
        create_pipe_files(cred, flags)
                 |
                 v
     unique VfsInode --private--> PipeObject
                 |                    ^
                 +-- write VfsFile ---+
                 +-- read  VfsFile ---+
```

## 调用约束 / 执行上下文

`init_pipefs()` 在 boot 的可睡眠执行路径运行，早于 pipe syscall 与并行测试。初始化会
分配 VFS 对象，不能在中断上下文或持有 spinlock 时调用。

`create_pipe_files()` 分配 pipe state、inode、dentry 和两个 file，并可能取得 sleepable
lock，只能从普通 task context 调用。接口显式接收 credential，不依赖反向 current-task
lookup；它可重入，不依赖 CPU-local state 或设备映射。

## 状态机

全局 filesystem 只有 `Uninitialized -> Published`。每次创建的 `PipeObject` reader、writer、
file count 和 wait queues 状态机由 `kvfs::pipe` 定义；`pipefs` 只负责一次性建立它与 inode/
file 的所有权关系。

## 算法流程

1. boot 通过 KVFS `new_pseudo_super_block()` 用静态 type、共享 simple `s_op`、pipefs
   `s_magic` 和静态 `s_d_op` 创建并发布 hidden mount。
2. syscall 调用 `create_pipe_files()` 并传入类型化 status flags；pipefs 只保留
   `NONBLOCK`，自行派生只读端与只写端 access mode。
3. 创建 `PipeObject`，初始 `files=2`、`readers=1`、`writers=1`。
4. 用 `get_next_ino()` 创建唯一 FIFO-mode inode；owner 来自传入 credential 的 fsuid/fsgid，
   inode private data 保存同一个 `PipeObject`。
5. 在 pipefs hidden mount 上创建 write file，再 clone read file；两个 file private data
   保存同一个 pipe object，并使用共享匿名-pipe file operation table。
6. dentry 自动继承 pipefs 默认 table，路径显示为 `pipe:[inode-number]`。

## 并发模型

`Once<PipeFs>` 只在初始化时写入。发布后的 hidden mount 不变；不同 pipe 创建互不共享
inode 或 `PipeObject`。同一 pipe 的 buffer、reader/writer 和 wait queue 并发由
`kvfs::pipe` 的 state lock 管理，pipefs 不再增加挂载级锁。

## 设计决策

- 匿名 pipe 不再借用 `anon_inodefs` singleton；独立 inode identity 对齐 Linux pipefs。
- `PipeFs` 只保存 Linux `pipe_mnt` 对应物，不复制 pipe session 或 operation table。
- inode private 与 file private 保存同一个 `Arc<PipeObject>`；这是 Linux `inode->i_pipe`
  与 `file->private_data` 指向同一对象的所有权表达，不是两份 pipe state。
- pipe/FIFO 数据通路仍由 KVFS 共用，避免为匿名 pipe 和 pathname FIFO 复制算法。

## Drop / 资源释放

hidden mount 在系统生命周期内保留。read/write file 释放时共享 pipe operations 更新
reader/writer/file counts；最后一个 file 释放最后的 file-private 引用，inode 随其 dentry/
file 引用释放，最终释放 inode-private `PipeObject`。没有额外 pipefs inode registry。
