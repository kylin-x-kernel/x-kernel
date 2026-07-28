# kvfs - 设计文档

## 定位

`kvfs` 提供 Linux 风格的 VFS 对象模型，包括 superblock、mount、dentry、inode、
open file、路径解析和通用文件系统 helper。具体文件系统通过 inode/file operation
traits 接入，POSIX 层负责把 syscall ABI 参数转换成 VFS 语义对象。`kvfs` 同时拥有
namespace validation、lock ordering、dcache identity、类型化 operation flags，以及
`VfsInode` 上的 page-cache attachment。

权限模型与 Linux 保持同一层次：`kcred` 定义 `Cred`，syscall 层从当前 task 取得一次
credential snapshot，`kvfs` 接收显式 `&Cred` 并完成路径遍历和通用 DAC。`kvfs` 不
依赖 `kprocess`，因此可被 boot、内核伪文件系统和测试以明确身份复用。

## 范围

- `src/open_flags.rs`：原始 `O_*` 参数的规范化与 open intent。
- `src/namei.rs`：路径遍历和 open 流程。
- `src/permission.rs`：owner/group/other DAC 与 open access 映射。
- `src/node/`：dentry、inode、live inode identity cache 及文件系统 operation traits。
- `src/address_space.rs`：inode-owned `AddressSpace`、其私有 `PageCache` 实现，以及
  writeback 与 truncate/invalidation 边界。
- `src/file.rs`：打开文件及其可变状态。
- `src/mount.rs`、`src/super_block.rs`：挂载树和文件系统实例。
- `src/anon_inode.rs`：匿名 inode pseudo filesystem，用于 eventfd、epoll、
  timerfd、pidfd 等内核创建的匿名文件。

## 架构

```text
current task                          kcred::Cred
    |                                     |
    | one Arc snapshot                    | explicit &Cred
    v                                     v
POSIX syscall ABI --> Filename --> Nameidata methods --> Path/VfsInode::permission
        |                                      |
        +-- open --> OpenHow --> OpenParams ---+--> VfsFile { f_cred: Arc<Cred> }
        |                         |
        |                         +-- flags: OpenFlags
        |
        +-- rename --> RenameFlags --> Path / Dentry --> InodeDirOperations
        |
        +-- statfs <---------------- StatFsFlags <---- filesystem

Mount / Path
    |
    v
Dentry ---- namespace location (parent, name)
    |                         |
    v                         v
VfsInode                 child cache
    |
    v
filesystem operation traits
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

`Dentry` 是可移动的 namespace 对象。rename 保留 source dentry 和 inode identity，只
改变 dentry 的位置和 cache membership。inode 持有文件状态和 address space，因此
rename 不会 flush 文件数据，也不会替换 inode `AddressSpace` identity。

每个 live backing inode number 通过 filesystem `InodeCache` 复用同一个 `VfsInode`；hard
link、rename 和重复 lookup 因此共享同一个 `AddressSpace`。非目录 inode 可以有多个
dentry alias；目录 inode 至多有一个 live alias，重复 lookup 复用该 dentry，对应 Linux
`d_splice_alias()` 所依赖的目录单 alias 不变量。具体文件系统完成 mutation
后，operation callback 已持有 `VfsInode` 时可用 `update_metadata_after_backing_change()` 或
不改变 size 的 `update_attributes_after_backing_change()` 刷新 VFS 缓存；只持有目标
`Dentry` 时使用同名的 dentry metadata refresh。`Dentry` 不向外部 crate 暴露内部
`Arc<VfsInode>`。更新入口校验 positive state、inode number、node type、block size 和
`rdev`，防止把一个 core inode 的结果写入另一 identity。

`Nameidata` 只保存 Linux namei 所需的路径、root、组件与 lookup 状态，不保存
credential。所有会解析或修改 namespace 的入口都把 `&Cred` 作为方法参数逐层传递。
credential 的生命周期由 syscall 持有的 `Arc` 保证；对象字段不需要重复保存调用上下文。

## 调用约束 / 执行上下文

路径和 namespace 操作会获取 sleepable lock、分配对象并调用具体文件系统，可能阻塞，
不适用于中断上下文，也不能在持有 spinlock 时调用。这些 API 依赖调度器、分配器和
正常内核运行环境。POSIX 路径通常需要当前进程的 mount、root 和 cwd；纯 VFS 对象
方法只依赖显式传入的对象。

一次完整 pathname 操作必须复用同一个 credential snapshot。调用者不能在每个路径
组件重新查询 current task，否则并发 credential commit 可能让同一次解析混用身份。

文件系统 callback 可在 I/O 上阻塞，但不能在持有同一组 VFS inode namespace lock 时
重新进入这些 VFS namespace 操作，否则会形成自锁。

`init_anon_inodefs()` 必须在 boot/runtime 初始化阶段调用，早于普通任务和并行单元测试
创建匿名 inode 文件。`AnonInodeFs::global()` 只读取已经初始化的 singleton，不会在
运行时首次访问路径中构造 VFS 对象；未初始化时会 panic 暴露启动顺序错误。

## 算法流程

open 在入口清理 legacy flags，校验已知位，生成 access mode、open intent 和 lookup
flags。namei 使用这些语义执行查找、创建和最终 open，不再直接组合 `O_CREAT` 与
`O_EXCL`。

VFS 采用 Linux directory-locking ownership model：

- slow lookup 对父目录 inode 加 shared namespace lock；
- create 对父目录 inode 加 exclusive namespace lock；
- unlink 和 rmdir 先锁父目录，再锁 victim inode；
- link 先锁新父目录，再锁非目录 source inode；
- same-directory rename 按 parent dentry identity 判定并只锁一次父目录；
- cross-directory rename 先获取 superblock topology mutex，再按拓扑顺序锁父目录。

Slow lookup 对应 Linux `d_alloc_parallel()`：cache miss 的任务建立唯一 hashed negative
dentry，设置 parallel-lookup 状态并持有该 dentry 的 lookup mutex 后调用 filesystem
`lookup`；同名并发 lookup 复用该对象并等待 owner 完成。filesystem 以 `Ok(None)` 表达
negative miss，找到 inode 时通常原位实例化 candidate；只有 `d_splice_alias()` 语义需要
复用目录 alias 时才返回另一个 dentry。普通 lookup 和 namespace mutation 使用同一个
locked lookup 对象方法，差别只在父目录分别持有 shared 或 exclusive namespace lock，
不存在 mutation 专属的 negative-cache 路径。

rename 在两个父目录稳定后解析 source 和 target，然后先锁参与的子目录，再锁非目录
inode。目录单 alias 不变量保证不同 parent dentry 不会引用同一个目录 inode。participant
最多是 source 和 target 两个 inode，直接按
目录/非目录组合获取，不构造临时 `Vec`；两个非目录 inode 按指针值排序。父目录拓扑遍历
同时给出锁顺序和 Linux `lock_two_directories()` 语义中的 `trap`，最终 source/target 直接与
`trap` 比较，不再次遍历父链。类型、flag、祖先关系以及针对最终 source/target 的 Path
policy 都在同一个 namespace transaction 内完成；filesystem callback 前已保存旧/新 cache
key，成功后 VFS 只交换 dentry location 并原位替换已有 cache slot，不再分配 name 或插入
新的 hash slot。目录是否为空由 filesystem rename/rmdir callback 判定，
通用层不把 dentry child cache 当作后端目录内容。

open-create 在同一个父目录 exclusive lock 下完成最终 lookup 和可能的 create，避免
`O_EXCL` 与 lookup/create 竞争。`O_CREAT | O_EXCL` 跳过 speculative lookup，直接执行锁内
最终 lookup；lookup 得到的同一个 negative dentry 会传给 filesystem create callback，不再
按名称构造第二个对象。create、mkdir、mknod、symlink 和 link callback 都必须实例化该
negative dentry；lookup 只有在复用既有目录 alias 时才返回另一个 dentry。read-only mount
和创建权限属于 create-only 错误：只有锁内最终
lookup 仍为 negative 时才检查；若名称已经变为 positive，普通 `O_CREAT` 打开现有对象，
`O_CREAT | O_EXCL` 返回 `AlreadyExists`。

### 路径遍历与 DAC

namei 在进入每个目录并查找下一组件前检查目录 search permission (`MAY_EXEC`)；最终
open 再按访问模式检查目标 inode。默认 `InodeOperations::permission` 进入
`generic_permission()`，依次选择 owner、group（包括补充组）或 other 权限位。
`fsuid == 0` 当前近似 Linux DAC override：读写可以越过普通 mode 位，普通文件执行
仍要求至少一个 execute bit，目录 search 可越过 execute bit。

namespace 修改由 `Path` 在调用文件系统回调前统一检查：create、mkdir、mknod、symlink、
link、unlink、rmdir 和 rename 要求父目录 `MAY_WRITE | MAY_EXEC`。unlink、rmdir 和 rename
把 Path policy 作为 validator 传入 Dentry 操作；validator 在父目录锁、所需 participant
锁和最终 lookup 结果仍然有效时执行。sticky 目录中的删除和替换还要求 root、目录所有者
或该最终 victim 的所有者身份之一，mountpoint 检查也针对同一个 dentry。

inode metadata 修改也由 `Path` 统一授权，再进入同一个后端 `setattr` callback：

- `chown` 只允许特权调用者改变 UID；owner 可保留原 UID，并把 GID 改为当前组或补充组；
- `chmod` 要求 inode owner 或特权，非所属组调用者请求的 setgid 位会被清除；
- `set_times` 区分 touch 与显式 times 数组。空 times 或两个当前时间允许 owner、特权或
  对 inode 有写权限的调用者；显式值以及 `UTIME_NOW/UTIME_OMIT` 混合只允许 owner/特权。

`SetattrTime` 只承载单次调用中“当前值/显式值”的授权信息，落盘的 `MetadataUpdate`
仍只包含解析后的时间值，不在 inode 或 namei 状态中保存调用上下文。

### 新 inode 所有者

创建回调接收同一个 `&Cred`。后端通过 `inode_init_owner()` 得到初始 mode、UID 和 GID：

- `mkdir` 在进入文件系统回调前清除调用者提供的 set-user-ID/set-group-ID 位；
- UID 使用 `fsuid`；
- 普通父目录下 GID 使用 `fsgid`；
- setgid 父目录下继承父 GID；
- setgid 父目录下创建子目录时由 `inode_init_owner()` 重新传播 setgid 位。

支持 Unix owner 元数据的后端在分配 inode 时持久化这些值。不能表达 Unix owner 的
文件系统仍受其磁盘格式限制。

`SimpleDir` 的 `DirMapping` 可持久插入由 `mknod` 分配的 special inode，也可插入
不可变目标的 symbolic link，供 devfs 等简单文件系统创建 FIFO、pathname Unix socket
以及 `/dev/fd` 一类启动期链接；未显式支持动态插入的 simple directory 保持返回 `EPERM`。

非递归 bind mount 创建新的 `Mount`，共享源 path 的 superblock 与 root dentry，但拥有
独立 mount ID、父挂载位置和覆盖关系。卸载 bind mount 只移除 topology 节点，不对共享的
源 dentry 执行 `forget()`；普通 filesystem mount 仍在卸载时释放其独占 mount-root dentry。

### pathname 与打开文件

`VfsFile` 捕获 open 时使用的 `Arc<Cred>`，对应 Linux `file::f_cred`。path-based
`Path::truncate()` 使用调用时 `&Cred` 检查写权限；descriptor-based
`VfsFile::truncate()` 验证文件以写模式打开后，复用 open 已建立的 authority，不重复
pathname DAC。`O_TRUNC` 已在 open 权限检查后执行同一 opened truncate 路径。

rename 在 syscall 边界用 `RenameFlags::from_bits` 拒绝未知位，并拒绝互斥模式。
VFS rename 入口再次检查组合不变量，文件系统 helper 再检查自身支持的子集。

默认 `FileOperations::read_iter` 使用页大小的内核缓冲区适配标量 read 回调。
一次迭代已经读取部分数据后，后续回调错误转换为已完成字节数，使 stream I/O
保持 POSIX partial-read 语义；首个回调失败时保留原错误。

匿名 inode 文件创建复用 boot 阶段发布的 `AnonInodeFs` singleton。初始化阶段创建
匿名 inode superblock、root dentry、singleton inode 和 root mount；后续
`get_file()` 只分配 per-file dentry/file，并共享 singleton inode。

`VfsInode` 的 data lock 串行化 buffered write 与 truncate。文件系统
`AddressSpaceOperations::set_len()` 在 backing prepare 后、释放 block 前调用一次
`AddressSpace::truncate_setsize()`。该入口按 Linux `truncate_setsize()` 顺序先发布唯一的
`inode::i_size`，再执行第一次 mapped-view unmap、cached-folio truncate 和第二次
unmap；第二轮用于清理 cache truncate 窗口中产生的 private COW PTE。

`simple_write_end` 保留在 `kvfs::libfs`，只服务 ramfs/memfs 风格的 aops。
块设备文件系统完成自己的 write-end 后，经 `AddressSpace::write_end_set_size()`
在同一 generic-write data-lock 临界区发布接受后的 `i_size`，而不把 ext4 语义伪装成
libfs helper。

`AddressSpace` 自己持有 object-id、mapped views、`AddressSpaceOperations` 和私有
`PageCache` storage。`PageCache` 不保存 length、object-id、views 或 invalidation
lifecycle；writeback 的 EOF 也由 `AddressSpace` 从 inode 读取后传入。
`VfsFile::mapping()` 借用 Linux `f_mapping` 对应物；MM 不接触底层 storage，只有确实需要
超出 file borrow 的生命周期时才显式 clone 该 `Arc`。

KVFS 在非 special file 的 writable open 进入文件系统 `open` callback 前增加
inode `write_count`，成功后用 `FMode::WRITER` 记录当前 file 持有该计数，打开
失败或 `FileOperations::release()` 返回后减少。因此 release callback 中读到 1
表示当前 file 是最后一个 writer，对应 Linux `do_dentry_open()` 中的
`get_write_access()`/`FMODE_WRITER` 和 `ext4_release_file()` 的调用时序。当前 KVFS
尚未实现 executable deny-write 导致的 negative count 状态。

## 并发模型

dentry 的 inode、children 和可变 operation 状态由各自 mutex 保护。一个 live dentry
对 parent 持有强引用，parent 的 children map 只保存弱索引；superblock dentry cache
强持有仍处于 hashed 状态的 child，直到 unlink、rename、forget 等路径显式驱逐。
这对应 Linux dcache 中 child 引用 parent、零外部引用的 hashed dentry 仍可驻留缓存的
生命周期，同时避免父子强引用环。`DentryKey` 通过持有 parent 的弱引用稳定对象身份，
不需要为每个 dentry 分配额外 ID。`VfsFile` 的 position 和 private data 使用 mutex，
`f_flags` 与 `f_mode` 使用原子整数存储，inode `write_count` 由打开与释放
路径原子更新。bitflags 类型是按值复制的语义快照，不额外引入锁或分配。`VfsFile::f_cred` 是创建后不变的 `Arc<Cred>`，无需额外锁。

匿名 inode pseudo fs 使用 `Once<AnonInodeFs>` 作为发布槽，但初始化只允许通过
`init_anon_inodefs()` 在受控 boot 阶段发生。这样避免多个 runtime 调用者并发首次访问时
在 `Lazy` 初始化闭包中构造复杂 VFS 对象。

Namespace callback 在 inode namespace lock 下运行。文件系统应在回调返回前把 core
mutation result 同步到受影响的 live inode，并通过 `LockedDentry::instantiate()` 完成创建
对象的 inode attachment；dentry cache 的删除/rebind 仍由 KVFS 在成功返回后完成。

全局 namespace lock 顺序为：

```text
superblock rename mutex
    -> parent-directory namespace locks
    -> child-directory namespace locks
    -> non-directory namespace locks (pointer order)
    -> per-dentry parallel-lookup mutex
    -> mount topology lock
    -> dentry cache and location locks
```

挂载操作与 namespace validator 都按 inode namespace lock 在外、mount topology lock 在内
的顺序执行，挂载点检查不会引入反向嵌套。parallel-lookup mutex 只序列化同一个 hashed
candidate 的 filesystem lookup callback，不会把不同名称的 slow lookup 串行化。
`SuperBlock::rename_mutex` 对应 Linux `s_vfs_rename_mutex`，不是 dcache rename
seqlock；它只在 cross-directory rename 中获取。`VfsInode::namespace_lock` 表达
Linux `inode->i_rwsem` 中和 namespace 相关的子集。`DentryLocation` 用一个 `RwLock`
同时保护 `parent` 和 `name`，读者不会观察到混合位置。

child cache 仍使用 mutex。RCU、seqcount lookup 和 lock-free dcache traversal 在当前
锁模型和回归覆盖稳定前保持在范围外。

## 设计决策

- ABI carrier 与内核语义类型分离，转换尽量靠近边界。
- 不提供通用 raw-flags getter；只有写入 ABI 或底层存储时调用 `bits()`。
- 不同 flags 家族不共享整数别名，使错误组合在编译期失败。
- current task 定位只存在于 `kprocess`；`kvfs` 对所有安全相关操作显式接收 `&Cred`。
- `Nameidata` 不保存 credential，避免把一次调用上下文变成冗余对象状态。
- 通用 DAC、父目录修改检查、sticky policy 和初始 owner 由 VFS 统一实现，后端只负责
  自身元数据与 operation callback。
- 打开文件保存 `f_cred`；descriptor 权限与 pathname 权限使用不同入口表达。
- 匿名 inode pseudo fs 采用显式预初始化，而不是 `Lazy` 首次访问初始化。该设计对齐
  复杂 VFS 全局对象的生命周期：启动时构造，运行时只复用。
- `AddressSpaceOperations::set_len()` 必须进入 `truncate_setsize()`；文件系统不能自行组合
  i_size、cache resize 和 mmap invalidation，也不能建立第二套 EOF/cache owner。
- `VfsInode` 直接保存唯一的 `Arc<AddressSpace>`，不增加单字段 address-space wrapper；
  `VfsFile` 保存 Linux 风格的 `f_mapping` 引用，MM runtime 只保存 `VfsFile`。
- 锁由语义 VFS 对象持有，而不是由 filesystem bridge 持有。
- `RenameData` 对应 Linux `struct renamedata`，其一次性 `execute` 会消费操作对象并驱动
  VFS orchestration；辅助方法只借用该对象，不为它提供 `Clone`/`Copy`。
- validator closure 是 Path policy 与锁内 Dentry transaction 之间的窄接口，不新增持久
  状态，也不在锁外保留 lookup 结果。
- namespace lock 使用 blocking lock，因为文件系统 callback 可以执行 I/O。

## Drop / 资源释放

VFS 对象通过 `Arc`/`Weak` 管理生命周期。unlink、rename replacement 和 forget 会从
parent children 弱索引与 superblock dentry cache 中移除 dentry；仍有 Path/File 引用
时对象继续存活，否则释放时沿 strong parent 链逐级归还引用。superblock 最终释放会
整体丢弃剩余 dcache 和 root。dentry cache 与 open file 都持有 `VfsInode` 引用；最后一个
引用消失后，`SuperBlockOperations::evict_inode()` 先获得 final teardown
机会。默认实现只丢弃 inode `AddressSpace` 中的 cached folios，磁盘文件系统可在
nlink=0 时追加 journaled inode
回收。per-open `FileOperations::release()` 先于 final inode eviction，不能代替后者。flags
类型不拥有资源；inode 与文件系统私有资源仍由现有对象生命周期及文件系统回调负责。

## 已知限制

- 尚无 capability、LSM、ACL、user namespace ID 映射或 idmapped mount 权限语义。
- FAT 等不能原生表达 Unix UID/GID 的后端不能完整持久化创建者身份。
- 当前 POSIX rename 路径不支持 `RENAME_WHITEOUT`。
- superblock dentry cache 尚无 Linux 风格 LRU/shrinker。
- fast lookup 仍是 mutex-based，没有 RCU 或 rename sequence validation；slow lookup 已按
  hashed candidate owner/waiter 模型合并同名并发 lookup。
- layered filesystem 的跨文件系统 lock rank 尚未建模。
- mount topology 同步与 superblock rename mutex 仍是不同机制。
