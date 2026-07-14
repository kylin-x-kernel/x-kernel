# kvfs - 设计文档

## 定位

`kvfs` 提供 Linux 风格的 VFS 对象模型，包括 superblock、mount、dentry、inode、
open file、路径解析和通用文件系统 helper。具体文件系统通过 inode/file operation
traits 接入，POSIX 层负责把 syscall ABI 参数转换成 VFS 语义对象。

## 范围

- `src/open_flags.rs`：原始 `O_*` 参数的规范化与 open intent。
- `src/namei.rs`：路径遍历和 open 流程。
- `src/node/`：dentry、inode 及文件系统 operation traits。
- `src/file.rs`：打开文件及其可变状态。
- `src/mount.rs`、`src/super_block.rs`：挂载树和文件系统实例。

## 架构

```text
POSIX syscall ABI (u32 flags)
        |
        +-- open --> OpenHow --> OpenParams --> namei --> VfsFile
                                  |
                                  +-- flags: OpenFlags
        |
        +-- rename --> RenameFlags --> Path/Dentry --> InodeDirOperations
        |
        +-- statfs <---------------- StatFsFlags <---- filesystem
```

原始整数只存在于 ABI 或兼容入口。进入 VFS 后，不同 flags 家族由不同 bitflags
类型表达，调用者通过 `contains`、`intersects` 或组合语义方法读取，避免重新解释
裸整数和误传其他 flags 家族。

`OpenParams` 对应 Linux namei 中规范化后的完整 open 参数。其字段保持私有，创建
意图、exclusive-create、lookup 行为和 mount 写入需求只能通过窄接口读取。
`OpenFlags` 是其中的 `O_*` 位集合，同时表示 `VfsFile::f_flags`；原子存储仍使用
`AtomicU32`，加载后立即恢复为类型。

`SuperBlock` 在创建时接管调用者提供的 root dentry。文件系统 operation 对象只保存
文件系统私有状态，不重复保存 root；这对应 Linux 中 `super_block.s_root` 的所有权
边界，也避免私有状态和 root inode 之间形成引用环。

## 调用约束 / 执行上下文

路径操作会获取 mutex、分配对象并调用具体文件系统，可能阻塞，不适用于中断上下文。
这些 API 依赖分配器和正常内核运行环境。POSIX 路径通常需要当前进程的 mount、root
和 cwd；纯 VFS 对象方法只依赖显式传入的对象。

## 算法流程

open 在入口清理 legacy flags，校验已知位，生成 access mode、open intent 和 lookup
flags。namei 使用这些语义执行查找、创建和最终 open，不再直接组合 `O_CREAT` 与
`O_EXCL`。

rename 在 syscall 边界用 `RenameFlags::from_bits` 拒绝未知位，并拒绝互斥模式。
VFS rename 入口再次检查组合不变量，文件系统 helper 再检查自身支持的子集。

## 并发模型

dentry 的 inode、children 和可变 operation 状态由各自 mutex 保护。一个 live dentry
对 parent 持有强引用，parent 的 children map 只保存弱索引；superblock dentry cache
强持有仍处于 hashed 状态的 child，直到 unlink、rename、forget 等路径显式驱逐。
这对应 Linux dcache 中 child 引用 parent、零外部引用的 hashed dentry 仍可驻留缓存的
生命周期，同时避免父子强引用环。`DentryKey` 通过持有 parent 的弱引用稳定对象身份，
不需要为每个 dentry 分配额外 ID。`VfsFile` 的 position 和 private data 使用 mutex，
`f_flags` 与 `f_mode` 使用原子整数存储。bitflags 类型是按值复制的语义快照，不额外
引入锁或分配。

## 设计决策

- ABI carrier 与内核语义类型分离，转换尽量靠近边界。
- 不提供通用 raw-flags getter；只有写入 ABI 或底层存储时调用 `bits()`。
- 不同 flags 家族不共享整数别名，使错误组合在编译期失败。

## Drop / 资源释放

VFS 对象通过 `Arc`/`Weak` 管理生命周期。unlink、rename replacement 和 forget 会从
parent children 弱索引与 superblock dentry cache 中移除 dentry；仍有 Path/File 引用
时对象继续存活，否则释放时沿 strong parent 链逐级归还引用。superblock 最终释放会
整体丢弃剩余 dcache 和 root。flags 类型不拥有资源；inode 与文件系统私有资源仍由
现有对象生命周期及文件系统回调负责。
