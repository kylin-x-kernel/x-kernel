# v9fs — 设计文档

## 定位

`fs/filesystems/9p` 是具体的 9P 网络文件系统，对应 Linux `fs/9p`。它拥有 mount、superblock、
inode、namespace、open-file 和 directory-iteration 的 KVFS 适配；`net/9p` 拥有 9P client、fid、
协议编解码和 connection 接口。

## 背景

远端对象在 rename/unlink 后仍由 fid 标识。按绝对路径重新 walk 会改变 open-file identity，
因此 v9fs 必须像 Linux 一样让 inode/dentry 和 opened file 持有不同角色的 fid。

## 范围

代码按 Linux 的职责拆分：

| X-Kernel | Linux | 职责 |
| --- | --- | --- |
| `fs.rs` | `vfs_super.c`、`v9fs.c` | mount session 和 superblock operations |
| `inode.rs` | `vfs_inode.c`、`vfs_inode_dotl.c` | inode、metadata 和 namespace operations |
| `file.rs` | `vfs_file.c` | open、read/write 和 fsync |
| `dir.rs` | `vfs_dir.c` | opened directory iteration |
| `util.rs` | dotl/VFS conversion helpers | device number 和 `d_type` 转换 |

## 架构

```text
SuperBlock private_data
        │
        v
V9fsSession ─────► Arc<P9Client> ─────► P9Transport
                         ▲
                         │
V9fsInode ───────► unopened Arc<P9Fid>
                         │ zero-name walk + open
                         v
VfsFile private_data ─► opened Arc<P9Fid>
```

`V9fsSession` 对应 Linux `v9fs_session_info`，只保留 mount 级 client。`V9fsInode` 不保存绝对
路径、重复的 node type 或 filesystem 外层锁；节点类型和身份来自 qid/attributes。

Linux 将 lookup fid 放在 dentry private data。KVFS 当前没有 dentry-private slot，因此 v9fs
把同一 unopened fid 放在 `V9fsInode`。这是维持对象身份所需的最小结构差异，没有为此增加
path cache 或第二层 fid wrapper。

## 调用约束 / 执行上下文

mount、lookup、namespace、I/O 和最后一个 fid 引用的释放都会同步等待 transport，只能在
scheduler 与 allocator 可用后的可睡眠 task context 执行。不要求 current userspace process，
但不得在 IRQ context 或持有 spinlock 时执行。并发入口可重入，协议请求由共享 client 串行化。

## 算法流程

### mount 和 inode

boot/driver glue 先选择具体 device 并建立 transport connection，随后调用 `v9fs::mount()`。
v9fs 创建 `P9Client`，协商 9P2000.L、attach root fid、取得并验证 root attributes，再发布
superblock。块大小按 Linux `fls(maxdata - 1)` 等价规则计算，最大文件大小使用 KVFS
`MAX_LFS_FILESIZE`。

inode number 使用 Linux 64-bit `QID2INO(qid.path) = qid.path + 2`。当前实现对应 Linux 默认
cacheless 模式：每次成功 lookup 由 returned fid 建立 inode，不以 inode number 合并不同 lookup；
hard link 则复用 source dentry 已有的 VFS inode。

attributes 通过 `TGETATTR` 获取，并验证 fid qid、attribute qid 和 VFS node type 一致。
显式 mode、owner、size、atime、mtime 修改转换为一次 `TSETATTR`；atime/mtime
使用明确指定的时间。ctime 只能请求服务端更新，`RSETATTR` 不返回其实际值，因此不会把
客户端 ctime 当作确认结果发布；对外 getattr 仍从服务端读取时间。

自动访问/修改时间更新对应 Linux `generic_update_time`：仅向 KVFS 返回本地 inode 应更新的
atime 或 mtime/ctime，不发送远端 `TSETATTR`，也不把自动更新转换成服务端取当前时间。

### open、I/O 和生命周期

每次 VFS open 都从 inode 的 unopened fid 执行 zero-name walk，得到独立 fid，按 Linux
`v9fs_open_to_dotl_flags()` 规则选择 access mode/status flags 后执行 `TLOPEN`。opened fid 作为
typed private data 放入 `VfsFile`；read、write、readdir 和 fsync 始终从该对象取 fid。
write 根据服务端确认的写入字节数更新 inode size，使用 KVFS 的
`grow_size_after_backing_write()` 保持 backing write 与本地大小发布的职责一致。

因此 rename/unlink 后，已打开文件不依赖旧路径，仍引用同一服务端对象。文件释放时 typed
private data 释放最后一个 `Arc<P9Fid>`，由 `P9Fid::drop` clunk；v9fs 不实现重复的 release 状态机。

directory iteration 把服务端 qid 转换为 inode number，并把服务端 `d_off` 作为下一次
`DirContext` 位置，不使用本地数组下标模拟远端 offset。

### namespace 操作

- lookup 在 parent fid 上 walk 单个 child name；
- create/mkdir/mknod/symlink 在 parent fid 上执行 dotl 操作，再 walk child 获得 unopened fid；
- link 发送 `dfid, oldfid, name`，并复用 source VFS inode；
- unlink/rmdir 优先执行 `TUNLINKAT`，仅对 `EOPNOTSUPP` 回退到 cloned child fid 的 `TREMOVE`；
- rename 优先执行 `TRENAMEAT`，仅对 `EOPNOTSUPP` 回退到 source fid 的 `TRENAME`；
- 全部操作使用对象 fid，不构造或更新绝对路径字符串。

`statfs` 使用 root inode fid 的 `TSTATFS`；仅在服务端明确表示 unsupported 时回退到
`simple_statfs`。`sync_fs` 没有 superblock-owned dirty state，opened object 的 durability 由
per-fid `TFSYNC` 表达。

## 并发模型

v9fs 自身不增加全局或 session 级锁。一个 mount 的 inode/file fids 共享 `P9Client`，client
内部 mutex 串行 transport request 和协议分配状态；VFS 对象和 fid 生命周期由 `Arc` 表达。

## 设计决策

- 只实现 9P2000.L cacheless filesystem 语义；
- 尚无 KVFS dentry-private fid slot、atomic-open contract、page-cache、xattr 或远端锁支持；
- transport discovery 和 VirtIO/kclass glue 保持在现有 boot/driver 层，本次不重构它们。

## Drop / 资源释放

superblock private data 持有 mount client；inode 持有 lookup fid；opened file 的 typed private
data 持有独立 opened fid。各宿主对象释放其最后一个 `Arc<P9Fid>` 时由协议层 clunk，v9fs
不复制 clunk 状态或实现额外 release wrapper。
