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
- `src/node/`：dentry、inode、inode identity 状态机及文件系统 operation traits。
- `src/address_space.rs`：inode-owned `AddressSpace`、其私有 `PageCache` 实现，以及
  writeback 与 truncate/invalidation 边界。
- `src/fiemap.rs`：与用户 ABI 解耦的 inode FIEMAP 请求状态、标志和安全输出接口。
- `src/file.rs`：打开文件及其可变状态。
- `src/xattr.rs`：xattr 名称/标志、namespace 权限和 `Path` 语义入口。
- `src/pipe.rs`：匿名 pipe 与 pathname FIFO 共享的数据通路、会话状态及无状态
  file-operation 对象。
- `src/file_system_type.rs`：已注册文件系统类型及其创建入口。
- `src/mount.rs`、`src/super_block.rs`：挂载树和文件系统实例。

具体 `anon_inodefs` 和 `pipefs` 位于 `fs/filesystems/`；KVFS 不保存它们的 filesystem
type、hidden mount 或 singleton inode。

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
        +-- statfs <----- StatFs (filesystem statistics)
                         SuperBlockFlags + MountFlags
                                      |
                                      v
                                StatFsFlags (ABI output)

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

`Path::render_from(root)` is the shared mount-aware pathname renderer for
process-root-relative views. Crossing a mounted filesystem root contributes the
covered mountpoint name before continuing in the parent mount. The result is
typed as `RenderedPath::Reachable` or `RenderedPath::Unreachable`; `getcwd(2)`
uses the latter to emit Linux's `(unreachable)` prefix instead of converting the
condition to `ENOENT`. `absolute_path()` uses the same component traversal with
the mount-tree root as its boundary.

pipe/FIFO 遵循 Linux `inode`、`pipe_inode_info` 与 `file` 的三层所有权：

```text
VfsInode
├── shared stateless FifoFileOperations
└── fifo_pipe: Option<Arc<PipeObject>>

PipeObject
└── buffer, files, readers/writers, r_counter/w_counter, rd_wait/wr_wait

VfsFile
├── mode
├── pipe_generation
└── private_data -> PipeObject
```

`PipeObject` 对应 `pipe_inode_info`，而不是 file-operation table。pipe 模块必须和
`VfsInode` 位于同一个 fs-core crate：inode 需要持有具体的 pipe session，pipe file
operations 又需要 VFS file/inode 接口；拆成相互依赖的 crate 只会迫使实现增加 factory
或类型擦除。

原始整数只存在于 ABI 或兼容入口。进入 VFS 后，不同 flags 家族由不同 bitflags
类型表达，调用者通过 `contains`、`intersects` 或组合语义方法读取，避免重新解释
裸整数和误传其他 flags 家族。

FIEMAP 的原始 C 布局和用户指针留在 POSIX 层。KVFS 以可选
`InodeOperations::fiemap_operations()` 表达 inode capability，并把类型化 flags、容量、
计数器和借用的安全 writer 收敛到 `FiemapExtentInfo`。文件系统后端只调用
`fill_next_extent()`，不接触或保存用户地址。

`OpenParams` 对应 Linux namei 中规范化后的完整 open 参数。其字段保持私有，创建
意图、exclusive-create、lookup 行为和 mount 写入需求只能通过窄接口读取。
`OpenFlags` 是其中的 `O_*` 位集合，同时表示 `VfsFile::f_flags`；原子存储仍使用
`AtomicU32`，加载后立即恢复为类型。

`SuperBlock` 先分配对象，再由 root initializer 构造并交回 root dentry，最后才绑定 root。
nodev 实例完成初始化后进入全局 registry；block-backed 实例则像 Linux `sget_dev()` 一样，
在 fill 前以 nascent 状态和独占 block-device claim 进入 registry，使并发调用者等待同一个
identity。initializer 可通过 nascent `SuperBlock` 的 inode 方法进入 VFS inode
identity table 并构造 root inode；失败会把实例标记为 dead、释放 claim、从 registry 删除并
唤醒等待者，不发布半初始化 superblock。这对应 Linux 先分配
`struct super_block`、再执行 `fill_super()`、最后令 `s_root` 可见的层次。文件系统 operation
table 是由同一文件系统类型的所有挂载共享的静态对象；挂载私有状态只能放在 superblock
private object 中，不重复保存 root。这对应 Linux 中 `s_op`、`s_fs_info` 与 `s_root` 的
所有权边界，也避免私有状态和 root inode 之间形成引用环。`SuperBlockFlags` 对应
`super_block.s_flags`，当前承载 `SB_RDONLY`、`SB_NOATIME` 和 `SB_NODIRATIME`；
`SuperBlock` 同时拥有 Linux `s_magic`、`s_blocksize` 和 `s_maxbytes` 对应的通用值，文件系统
在构造时提供它们，inode-private state 不再复制 magic、block size 或通用文件大小上限。
`MountFlags` 对应 `vfsmount.mnt_flags`，承载 `MNT_RELATIME` 等 per-mount 策略。
`SuperBlockOperations::statfs()` 只返回文件系统统计数据，POSIX 导出时才由 VFS 将
superblock/mount 状态转换为 `StatFsFlags`。该转换遵循 Linux `fs/statfs.c`：当前建模的
superblock 位只有 `RDONLY` 导出为 `ST_RDONLY`；`ST_NOATIME`/`ST_NODIRATIME` 只由
per-mount 位导出，即使同名 superblock 位仍参与 inode atime 决策。重挂载在发布新
`SuperBlockFlags` 前调用静态 `FsContextOperations::reconfigure`，不把 transaction callback
放入 `SuperBlockOperations`；该 hook 从 `FsContext` 接收拟议 flags、changed mask、purpose、
credential、解析态 `fs_private` 和拟议 superblock-private `s_fs_info`。reconfigure context
在创建时绑定目标 `SuperBlock`，对应 Linux `fs_context::root->d_sb`；callback 只接收 context，
不再额外传递一份可能失配的 superblock。VFS 只接受 purpose 为 reconfigure 且目标 identity
匹配的 context；hook 和
`sb_flags`/`sb_flags_mask` 的发布、最终 shutdown 由 superblock 内的 umount lock
串行化，对应 `s_umount` 的职责。和 Linux 未提供 callback 时的行为一样，默认 hook 接受纯 VFS
flags 变更；block-backed superblock 在 callback 前由 VFS 统一拒绝与 canonical read-only
device 冲突的读写目标。

`ktime_types::TimestampLimits` 对应 Linux `super_block` 的 `s_time_gran`、`s_time_min` 和
`s_time_max`。`SuperBlockOperations::timestamp_limits()` 采用 Linux `alloc_super()` 的整秒、
完整 `i64` 范围作为保守默认；filesystem fill operation 安装 operation table 和 private state
后查询一次该 capability，再由唯一的 `SuperBlock` 持久保存。文件系统可以像 Linux
`fill_super()` 设置 `s_time_*` 一样，根据磁盘格式或协议状态显式覆盖；KVFS 不另建并行的
format object，也不在 bridge 中复制该状态。
`VfsInode::current_time()` 生成已经向下取整并截断范围的当前
时间，`truncate_timestamp()` 对显式值执行同一转换。显式 `setattr`、自动 I/O 时间更新和
namespace helper 在比较及 filesystem callback 前应用 superblock 级公共能力；filesystem
callback 再处理 FAT 分字段粒度等格式专有语义，并通过同一个 `MetadataUpdate` 返回实际生效值。
resident inode 只发布 callback 返回的字段，忽略的字段不再被 VFS 恢复为原请求；raw timestamp
setter 只负责存储，不再重复查询 superblock policy。这与
Linux `current_time()`/`timestamp_truncate()` 负责准备、`inode_set_*_to_ts()` 负责存储的层次
一致，也允许 FAT 像 Linux `fat_truncate_time()` 一样维护 atime 与 mtime/ctime 的不同不变量。

`FileSystemType` 对应 Linux `struct file_system_type`，描述文件系统实现的名称、
是否需要 backing device、`init_fs_context` 及类型 flags。`init_fs_context` 为每个
transaction 安装静态共享的 `FsContextOperations` 并初始化 context-private state，对应
Linux `file_system_type::init_fs_context` 设置 `fs_context::ops`/`fs_private` 的层次；operation
table 本身不保存 transaction state。全局注册表对应 Linux
`file_systems` list，是按类型名挂载和 `/proc/filesystems` 的共同事实源。
每个类型通过 context operation table 的 `get_tree` 回调构造 superblock，对应 Linux 的 `->get_tree`：nodev
类型调用 `get_tree_nodev`；device-backed 类型调用 KVFS `get_tree_bdev`，由 VFS super
层完成 source pathname、block-special inode、`nodev` 和 `rdev` 校验，再从 block core
取得 canonical `BlockDevice`。`get_tree_bdev` 随后按
`(FileSystemType, BlockDevice object)` 在同一个 superblock registry 中执行 Linux
`sget_dev()` 等价查找：已有 live 实例直接复用；初始化中
或正在 shutdown 的实例等待其 lifecycle transition；只有取得 reservation 的调用者才执行
filesystem fill-super。新的 block-backed superblock 在 block core 取得 exclusive claim，
所以同一 canonical device 不能由不同 filesystem type 或 instance 并行拥有；同一 `dev_t`
撤销后重新发布的 `BlockDevice` 对象则是新的介质代际，不会命中旧 root 和 cache。这与
Linux `setup_bdev_super()` 以新 superblock 作为 exclusive `bdev_open()` holder 一致：不同
filesystem type 创建不同 holder，因此不能同时取得同一设备。VFS
先分配并注册已经带有 `s_type/s_bdev/s_flags` 的 nascent `SuperBlock`，fill callback 只接收
该对象并安装 filesystem operations 与 root，不再接收或复制 type、device、flags；一次性
fill closure 可以捕获同一 `FsContext` 中已经解析的 mount state，而不把 option parser 放进
generic VFS。fill 成功后 VFS 才把 lifecycle 切换为 available。
已有 block-backed superblock 与新请求的 `SB_RDONLY` 不一致时返回 `EBUSY`，对应 Linux
`get_tree_bdev_flags()` 的已有 `s_root` 分支，而不是将 block mount 降格为 per-mount 策略。
与 Linux `bdev_file_open_by_path` 一样，可写 mount 会拒绝
canonical read-only device；只读 mount 仍可继续交给 filesystem fill-super。`fs_flags` 字段是类型化
的 `FileSystemTypeFlags`（bitflags），对应 Linux `struct file_system_type::fs_flags`，
其中 `REQUIRES_DEV` 声明是否需要 backing device，位编号与 Linux `include/linux/fs.h`
对齐，供 `/proc/filesystems` 的 nodev 列和 mount 错误路径判断；未来按 Linux 语义
新增标志（如 binary mountdata、subtype）时保持同一编号体系即可扩展。

block-special inode 统一安装 KVFS 的 `DefaultBlkdevFileOperations`，对应 Linux
`def_blk_fops`。open 按 inode `rdev` 直接查 block core；read/write/fsync 和通用
`BLKGETSIZE*` 操作作用于同一个 resident `BlockDevice`，未知 ioctl 再分派到
`Gendisk` 的 `BlockDeviceOperations`。普通 close 和 write 不隐式 flush，显式 fsync 才把
durability 请求传给 backend。KVFS 不维护第二张 `dev_t -> DeviceFileOps` 表；
devfs 只投影名称与 `rdev`，loop 设备也按普通 `Gendisk` 发布。

`FsContext` 对应 Linux `struct fs_context`，保存 `fs_type`、mount/reconfigure purpose、source、
一页有界的 kernel-owned opaque mount data、`sb_flags`/`sb_flags_mask`、mounter credential、
静态 operations、可选 parsed `fs_private` 和拟议 `s_fs_info`。opaque data 只借用于同步
transaction；KVFS 不施加文本编码或 option grammar，具体文件系统在 `init_fs_context` 中解析
自身 representation，并把挂载后仍需存活的选项复制为 typed private state。两种 private state
由 context 独占；callback 通过 typed borrow 验证拟议 `s_fs_info`，失败 reconfigure 不修改既有
superblock private。
reconfigure context 还借用其唯一目标 superblock，使 callback 只需 context 即可恢复 owner。
它不保存进程 `FsStruct` 的 root/pwd。Linux 在
`get_tree_bdev -> lookup_bdev -> kern_path` 中从 ambient `current->fs` 取得路径环境；
KVFS 为避免反向依赖 `kprocess`，由 mount 执行入口取得 `FsStruct` 的 root/pwd 快照，
在调用 `FsContext::get_tree` 时显式传入。该快照只沿调用栈存在，不成为 mount transaction
字段。`FileSystemType` registry 与 `SuperBlockRegistry` 分别对应 Linux 的 filesystem type list
和 superblock instance set，不互相复制状态。对于 block-backed 类型，后者以 canonical
`FileSystemType` 指针和 canonical `BlockDevice` 对象地址作为唯一 identity；已有 available
实例直接复用，nascent/dying 实例等待状态转换，dead 实例删除后重试。不同类型争用同一
device 时由 block core 的独占 claim 拒绝，不能建立第二份 mutable mount state。

`SuperBlock` 强制分离静态 filesystem operation table 与 mount-private object，对应 Linux
`s_op`/`s_fs_info`。构造 API 只接受 `&'static dyn SuperBlockOperations`，不能把每次挂载分配的
有状态 object 当作 operation table；需要挂载状态的文件系统通过
`try_new_with_flags_and_private()` 把唯一 private object 交给 superblock 独占。callback 通过
绑定到 `&SuperBlock` 生命周期的 `private::<T>()` 借用恢复它，不复制 `Arc`、不允许 typed owner
逃逸 superblock 生命周期。block size 与 max file size 作为通用 superblock 字段由 fill
operation 一次设置，不要求 private inode 或 operation table 复制。

`Dentry` 是可移动的 namespace 对象。rename 保留 source dentry 和 inode identity，只
改变 dentry 的位置和 cache membership。inode 持有文件状态和 address space，因此
rename 不会 flush 文件数据，也不会替换 inode `AddressSpace` identity。与 Linux `d_sb`/`i_sb`
一致，dentry 和 inode 首次附着 superblock 后都不可重绑；二者保存 `Weak<SuperBlock>` 以避免
root/dcache 强引用环，但旧 superblock 销毁也不会把同一 VFS identity 转交给新 superblock。

每个由文件系统通过 VFS-wide identity table 发布的 hashed
`(SuperBlock, backing inode number)` 复用同一个 `VfsInode`；hard link、rename 和重复 lookup
因此共享同一个 `AddressSpace`。Linux 风格的 pseudo/unhashed inode 不进入该表，也不能对同一
identity 混用直接构造与 `get_or_try_init_inode()`。非目录 inode 可以有多个
dentry alias；目录 inode 至多有一个 live alias，重复 lookup 复用该 dentry，对应 Linux
`d_splice_alias()` 所依赖的目录单 alias 不变量。文件系统可以让 operation object 实现
`InodeAttributeOperations`，使 KVFS generic attribute API 与文件系统组件共用一份状态；这对应
Linux filesystem-private inode 结构内嵌 `struct inode`。未使用共享后端的文件系统完成 mutation
后，operation callback 已持有 `VfsInode` 时仍可用 `update_metadata_after_backing_change()` 或
不改变 size 的 `update_attributes_after_backing_change()` 刷新 VFS 缓存；只持有目标
`Dentry` 时使用同名的 dentry metadata refresh。`Dentry` 不向外部 crate 暴露内部
`Arc<VfsInode>`。更新入口校验 positive state、inode number、node type、block size 和
`rdev`，防止把一个 core inode 的结果写入另一 identity。

identity table 对应 Linux `fs/inode.c` 的全局 `inode_hashtable`，按 superblock identity 与 inode
number 联合索引；它不成为 `SuperBlock` 或 filesystem/bridge 的字段。
`SuperBlock::lookup_inode()` 是该全局算法的面向对象入口，对应 Linux `ilookup()`；
`get_or_try_init_inode()` 对应
`iget_locked()` 加 filesystem fill，再由 VFS 发布。cache 同时承担 Linux inode cache 的
初始化与释放门禁：缺失 slot 先转为 `New`，只有
owner 运行 fallible filesystem initializer，其他 lookup 睡眠；成功后发布 `Live`，失败或
initializer panic unwind 都由局部 reservation guard 删除 `New` slot 并唤醒重试。该 guard 只表达
Linux `unlock_new_inode()`/`iget_failed()` 的成对约束，不进入 resident inode，也不增加 cache
状态。每个 slot 的等待队列对应 Linux 对该 inode `I_NEW/I_FREEING` state bit 的等待；等待者
保存 slot generation 并在 cache mutex 外睡眠，只有该 inode 的 publish/remove 才唤醒它，避免
全 cache 广播和跨 inode 惊群。Cache-only lookup 等待过渡对象后若 slot 已 unhashed，返回
`None`；必须从磁盘加载的 hashed 路径使用 `get_or_try_init_inode()`。新 hashed identity 在发布 `Live` 前
先绑定所属 superblock，因此 filesystem callback 观察不到缺失 `i_sb` 的 resident inode。最后一个
`VfsInode` 引用进入 drop 时，cache 在 filesystem eviction hook 前
把精确匹配的 `Live` entry 转为 `Freeing`；同号 lookup 等待 hook 完成、旧 entry 删除后重新
查找。`New`/`Freeing` 不会作为普通 inode 返回，也不会转换为 pathname 的 `EINVAL`/`ESTALE`。
Filesystem private state 由 `VfsInode` 组合持有，不得在后端另建 resident inode-number cache。
普通 pathname callback 的 `Path -> Mount` 强引用在调用期间保活 superblock；如果一个未绑定或
teardown 后的 raw inode 被绕过 Path 直接用于 filesystem callback，`VfsInode::super_block()` 返回
Linux `ESTALE`，不把对象生命周期错误误报为介质 `EIO`。
共享 attribute operations 的构造入口会在发布前校验 inode number 与 node type；构造后 KVFS
不再分配另一份 owner/link/time/size/block state。高频 generic field 读取直接调用共享组件的
单字段 getter；只有完整 `stat/getattr` snapshot 才调用 `fill_metadata()`。完整 snapshot 的 inode
number 和 superblock block size 由唯一 `InodeIdentity`/`SuperBlock` 传入，attribute component
不再复制 `i_ino` 或通用 block-size 字段。

`Nameidata` 只保存 Linux namei 所需的路径、root、组件与 lookup 状态，不保存
credential。所有会解析或修改 namespace 的入口都把 `&Cred` 作为方法参数逐层传递。
credential 的生命周期由 syscall 持有的 `Arc` 保证；对象字段不需要重复保存调用上下文。

## 调用约束 / 执行上下文

路径和 namespace 操作会获取 sleepable lock、分配对象并调用具体文件系统，可能阻塞，
不适用于中断上下文，也不能在持有 spinlock 时调用。这些 API 依赖调度器、分配器和
正常内核运行环境。POSIX 路径通常需要当前进程的 mount、root 和 cwd；纯 VFS 对象
方法只依赖显式传入的对象。

最后一个 `VfsInode` 引用的释放可能同步执行 filesystem eviction；最后一个
`VfsMount` 的释放可能同步执行 superblock shutdown。两者同样只能发生在可睡眠的 task
context，且调用者不能持有 non-sleepable lock 或 teardown 所需的锁。这对应 Linux
`iput()` 本身可以睡眠的调用约束，不额外引入 inode 专用 deferred worker。

一次完整 pathname 操作必须复用同一个 credential snapshot。调用者不能在每个路径
组件重新查询 current task，否则并发 credential commit 可能让同一次解析混用身份。

文件系统 callback 可在 I/O 上阻塞，但不能在持有同一组 VFS inode namespace lock 时
重新进入这些 VFS namespace 操作，否则会形成自锁。

内建文件系统类型也必须在用户进程启动前注册完成。运行期查找只读取注册表，不负责
加载实现或补做启动初始化。

## 算法流程

open 在入口清理 legacy flags，校验已知位，生成 access mode、open intent 和 lookup
flags。namei 使用这些语义执行查找、创建和最终 open，不再直接组合 `O_CREAT` 与
`O_EXCL`。`O_PATH` 路径同样执行最终对象类型约束；例如 `O_PATH | O_DIRECTORY`
只接受可执行子项查找的目录，普通文件和 autodir 按 Linux 语义返回 `ENOTDIR`。最终组件
后的 `/` 在通用 `path_lookupat()` 中转换为 `FOLLOW_FINAL | DIRECTORY`，因此即使同时指定
`O_NOFOLLOW`，尾随斜杠仍会跟随最终符号链接，并用 `Dentry::can_lookup()` 校验解析结果。

pathname FIFO 不通过 syscall fallback 或第二次 lookup 打开。所有 FIFO inode 共享一张
无运行时字段的 file-operation table；每个 inode 在 special state 中持有当前活动
`PipeObject`，对应 Linux `inode->i_pipe`。namei 在同一个 resolved `Path` 上完成
`may_open` 后进入该 table 的 `open()`。operation table 可以修改现有
`VfsFileBuilder` 的 stream mode、private data 和最终 file operations，但不得替换
builder 已绑定的 path。由此 pathname DAC、实际使用的 inode 和 opened-file identity
属于同一次 open transaction。

设备 open callback 可通过 `VfsFileBuilder::requests_no_controlling_tty()` 检查瞬态
`O_NOCTTY` 请求。该检查发生在 callback 内；open 完成后，`O_NOCTTY` 与其他创建期
flags 会从最终 `f_flags` 中移除。

FIFO open 在 inode pipe-slot 锁内创建或复用 session 并增加 `files`，随后
`PipeObject::open_fifo()` 根据 builder 的 `f_mode` 完成 reader/writer rendezvous。
最后一个 file release 在同一 slot 锁域内减少 `files` 并清空 inode slot，防止旧
release 与新 open 交错后把仍在使用的 session 清除。

### 文件 extent 查询

调用方先从 `VfsInode::fiemap_capability()` 取得可选 inode capability，再用
`FiemapExtentInfo` 发起查询。普通文件在 inode data shared lock 下执行，目录在 namespace
shared lock 下执行；因此同一机制既覆盖 regular inode，也覆盖 directory inode，而不把
FIEMAP 错挂到 open-file operation。文件系统把 inode 创建时确定的格式上限传给
`FiemapExtentInfo::prepare()`；该 helper 先校验长度、最大文件大小和文件系统支持的 flags，
最后才按需执行 data-only writeback。

后端只输出与查询范围相交的 mapped、unwritten 或 delayed extent，跳过 hole。
`extent_count == 0` 时 `FiemapExtentInfo` 只计数；数组满后正常停止，只有已确认遍历结束的
最后一个 extent 才添加 `LAST`。

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

open-create 和独占 namespace create 复用同一个父目录锁内最终 lookup 骨架，避免
`O_EXCL`、`mknodat`、`linkat`、`symlinkat` 与 lookup/create 竞争。
`O_CREAT | O_EXCL` 跳过 speculative lookup，直接执行锁内最终 lookup；独占创建在
positive dentry 上先返回 `AlreadyExists`，只有 negative dentry 才执行创建 callback。
`Filename` 只做 parent resolution 和 pathname-specific errno，Dentry 只把锁内最终
negative dentry 交给 callback，不额外暴露 parent inode。`Path::vfs_create()`、
`Path::vfs_mkdir()`、`Path::vfs_mknod()`、`Path::vfs_symlink()` 与
`Path::vfs_link()` 从 parent Path 自身取得 inode，负责 mount write、父目录 DAC、
设备节点授权、mode preparation 以及 inode callback。
lookup 得到的同一个 negative dentry 会传给 callback，不再按名称构造第二个对象；
regular `mknodat` 传递
`exclusive=true`。create、mkdir、mknod、symlink 和 link callback 都必须实例化该 negative
dentry；lookup 只有在复用既有目录 alias 时才返回另一个 dentry。create-only 错误只在
锁内最终 lookup 仍为 negative 时检查。

unlink 和 rmdir 同样由 `Filename::unlink_at()` / `Filename::rmdir_at()` 先解析 parent，
再由 Dentry 在父目录 exclusive namespace lock 下唯一一次解析 victim。syscall 不先取得
完整目标 Path 后再按名称查找，因此不会在 replacement 竞争中删除另一个同名 inode。

### 路径遍历与 DAC

namei 在进入每个目录并查找下一组件前检查目录 search permission (`MAY_EXEC`)；最终
open 再按访问模式检查目标 inode。默认 `InodeOperations::permission` 进入
`generic_permission()`，依次选择 owner、group（包括补充组）或 other 权限位。
`fsuid == 0` 当前近似 Linux DAC override：读写可以越过普通 mode 位，普通文件执行
仍要求至少一个 execute bit，目录 search 可越过 execute bit。

namespace 修改由 `Path` 或 `Filename` 在调用文件系统回调前统一检查：
create、mkdir、mknod、symlink、link、unlink、rmdir 和 rename 要求父目录
`MAY_WRITE | MAY_EXEC`。所有 create-like 操作由锁内 callback 调用共同的 `Path::vfs_*`
能力；unlink、rmdir 和 rename 把 Path policy 作为 validator 传入 Dentry 操作。
这些 policy 在父目录锁、所需 participant lock 和最终 lookup 结果仍然有效时执行。
sticky 目录中的删除和替换还要求 root、目录所有者或该最终 victim 的所有者身份之一，
mountpoint 检查也针对同一个 dentry。

inode metadata 修改也由 `Path` 统一授权，再进入同一个后端 `setattr` callback：

- `chown` 只允许特权调用者改变 UID；owner 可保留原 UID，并把 GID 改为当前组或补充组；
- `chmod` 要求 inode owner 或特权，非所属组调用者请求的 setgid 位会被清除；
- `set_times` 区分 touch 与显式 times 数组。空 times 或两个当前时间允许 owner、特权或
  对 inode 有写权限的调用者；显式值以及 `UTIME_NOW/UTIME_OMIT` 混合只允许 owner/特权。

`SetattrTime` 只承载单次调用中“当前值/显式值”的授权信息，落盘的 `MetadataUpdate`
只包含解析后的 atime/mtime/ctime 值，不在 inode 或 namei 状态中保存调用上下文。
自动 I/O 时间更新使用 Linux `FS_UPD_ATIME` / `FS_UPD_CMTIME` 对应的
`InodeUpdateTime`，并经 filesystem `update_time` callback 落盘。授权完成后的显式时间值由
`VfsInode::truncate_timestamp()` 准备；自动更新时间通过 `current_time()` 取得可表示值，并在
atime/mtime/ctime 比较之前使用。callback 返回实际生效的 `MetadataUpdate`；resident attribute
setter 只发布返回字段及其最终值，因而后端可以降低字段精度、联动多个时间字段或明确忽略
无法持久化的字段，setter 本身不再重复执行 capability policy。

Xattr 也由 `Path` 统一承载 mount、namespace 和 DAC 策略，再进入
`InodeOperations::{get,list,set,remove}_xattr`。get/set/remove 输入名称以受检的内核
`Vec<u8>` 保存；list 通过 `XattrNameRef` 与 `XattrNameSink` 逐项传递可分片借用的完整名称，
从而允许后端零拷贝添加 namespace prefix，并在 `Path` 层把 `trusted.*` 过滤后才交给调用者。
两种表示都完整保留非 UTF-8 suffix；`XattrSetFlags` 在进入后端前已从 ABI raw bits 转换，
`CREATE|REPLACE` 组合保持为两个同时置位的约束交给后端。`user.*` 仅允许
regular file、directory 和 socket，并对 sticky directory 写入执行 owner/privileged 检查；
`trusted.*` 只对 privileged credential 可见；`security.*` 读操作保留 Linux VFS 的 LSM
委托模型，但在 LSM/capability hook 尚未接入时，set/remove 使用 privileged credential
近似阻止非特权调用者伪造安全属性；`system.*` 留给 filesystem 或 ACL 层授权。所有 namespace
分支之前先执行通用写入检查：带 `NodeFlags::IMMUTABLE` 或 `NodeFlags::APPEND_ONLY` 的 inode
拒绝 set/remove 并返回 `EPERM`，读取不受影响。Set/remove 在 inode data lock 内调用后端，
具体文件系统必须在自己的事务或锁内原子完成四种 `CREATE`/`REPLACE` 组合的存在性判断。

### 新 inode 所有者

创建回调接收同一个 `&Cred`。后端通过 `inode_init_owner()` 得到初始 mode、UID 和 GID：

- open-create、`mkdirat`、regular `mknodat` 和 special-node `mknodat` 都进入对应的
  `Path::vfs_*` 能力；parent inode 的 mode-preparation 方法依次处理 setgid、调用者
  umask、VFS allowed-permission mask 和 callback node type，当前没有 POSIX ACL 延迟
  umask 的分支；
- `vfs_mkdir` 通过 allowed-permission mask 排除调用者提供的 set-user-ID/set-group-ID
  位，而不是在 Path 层再次手工修改结果；
- UID 使用 `fsuid`；
- 普通父目录下 GID 使用 `fsgid`；
- setgid 父目录下继承父 GID；
- setgid 父目录下创建子目录时由 `inode_init_owner()` 重新传播 setgid 位。

支持 Unix owner 元数据的后端在分配 inode 时持久化这些值。不能表达 Unix owner 的
文件系统仍受其磁盘格式限制。

`SimpleDir` 的 `DirMapping` 可持久插入由 `mknod` 分配的 special inode，也可插入
不可变目标的 symbolic link，供 devfs 等简单文件系统创建 FIFO、pathname Unix socket
以及 `/dev/fd` 一类启动期链接。快速链接目标只存入 `VfsInode` cached-link 状态，
对应 Linux `inode::i_link`；`SimpleFsNode` 只保存 inode number、mode、owner 和
`i_size` 等元数据，不再用 `SimpleFile` closure 保存第二份目标。未显式支持动态插入的
simple directory 保持返回 `EPERM`。

非递归 bind mount 创建新的 `Mount`，共享源 path 的 superblock 与 root dentry，但拥有
独立 mount ID、父挂载位置和覆盖关系，并继承源 mount 的 per-mount flags。对应 Linux
`clone_mnt(old, root, clone_flags)`，namespace 复制与 bind mount 复用
`Mount::clone_mnt`：它只构造 detached `Mount`，不同时写入 parent topology；
`Path::graft_tree()` 再执行 root/target 类型检查并把对象发布到 mountpoint。新文件系统
挂载也先构造 detached `Mount`，再走同一个 graft 阶段，避免出现“已有父路径但未进入
parent child map”的半挂载状态。和当前 Linux `graft_tree()` 一样，root/target 的目录性
不一致时两个方向都返回 `ENOTDIR`，不在 bind 路径另建 errno 分支。非递归 bind 不复制
源 path 下的 child mount，初次 `MS_BIND` 也不把同次调用的普通 mount flags 应用到副本。

`Mount` 作为被 `Path`、打开文件和 namespace topology 共同引用的稳定身份，不在 remount
时替换。父 mount 和 mountpoint 由一个 `Mutex<Option<Path>>` 表示，attach 时一次设置，
detach 时清空；不再分别保存可能失配的 parent/mountpoint 字段。namespace 操作通过已有
mount ID registry 校验 source、target 和 detach 对象属于当前 `MntNamespace`；同一把
registry mutex 覆盖校验、topology 修改和 registry 提交，作为 namespace 级事务边界。
递归 detach 先从目标 mount 收集每个 child mountpoint 的完整 overmount stack，再校验所有
对象仍在 registry 中；校验成功后才执行不再返回错误的 topology commit，最后删除 registry
引用。目标 mount 自身覆盖的旧层不属于待删除子树，commit 时恢复到 parent mountpoint。
`VfsMount::flags()` 与 `set_flags()` 在 `mnt_flags` 字段上封装原子读写，对其余 mount
代码保持强类型 `MountFlags` 接口。

普通 remount 只允许作用于已注册 mount 的根路径，由 `MntNamespace::remount()` 接收一个
reconfigure-purpose `FsContext` 和 per-mount flags，更新 `SuperBlock` 上所有共享 mount 都能观察到的
只读策略并原子替换目标 mount flags；`MS_REMOUNT|MS_BIND` 通过
`MntNamespace::reconfigure_mount()` 只替换目标的 per-mount flags，不要求目标本身由
bind 创建。superblock flags 由 `AtomicSuperBlockFlags` 保存；普通 remount 在发布新 flags
前调用 `FsContextOperations::reconfigure`，并由 per-superblock umount lock 串行化 hook
和发布。文件系统后端固定报告只读时，切换到读写会返回 `ReadOnlyFilesystem`。
`MntNamespace::make_mount_private()` 复用同一 registered-mount-root 校验。KVFS 当前没有
shared propagation group，所有 mount tree 在构造和 namespace clone 后都不会向其它
namespace 传播，因此 make-private 是幂等操作；递归请求不需要遍历或修改 descendants。
mount 不保存“是否由 bind 创建”的来源状态；所有 mount 卸载都只移除 topology 节点，
`VfsMount` 创建和复制时取得一个 superblock active 引用，最后释放时归还，对应 Linux
`cleanup_mnt()` 中的 `dput(mnt_root)` 和 `deactivate_super(mnt_sb)`。非最后一个 mount
释放不重复 teardown；active 计数归零时，在 umount lock 下先写回 inode/page cache 和
filesystem 状态，再驱逐 dcache 所有权，最后再次同步 eviction 产生的 metadata、journal
checkpoint 和设备状态，对应 `generic_shutdown_super()` 与 block-device final flush 的职责。
`MntNamespace::mount_new()` 把 `get_tree` 与 active-reference acquisition 组成一个重试循环：
若最后一个旧 mount 在两者之间令复用实例进入 dying/dead，VFS 不把该对象 graft 进 namespace，
而是重新执行 `get_tree`。同一 shared superblock 的新 mount 若请求不同的只读状态则返回
`EBUSY`，不会让 per-mount 创建悄然改变既有实例的 filesystem-wide policy。
`Path::unmount()` 不在 topology 层调用 `sync_fs()`，所以打开文件或其它 `Path` 持有 detached
mount 时会像 Linux 一样推迟 final shutdown。shutdown 的 sync 错误被记录但不回滚已提交的
topology；清理和最终 flush 仍继续执行。
卸载在 parent mountpoint 的 inode namespace lock 下移除 parent child 索引、恢复被覆盖的
mount，并清空 detached mount 的 parent location，使仍持有该 mount 的打开路径不能沿旧
parent 返回已经离开的 namespace。

### pathname 与打开文件

`VfsFile` 捕获 open 时使用的 `Arc<Cred>`，对应 Linux `file::f_cred`。path-based
`Path::truncate()` 使用调用时 `&Cred` 检查写权限；descriptor-based
`VfsFile::truncate()` 验证文件以写模式打开后，复用 open 已建立的 authority，不重复
pathname DAC。`O_TRUNC` 已在 open 权限检查后执行同一 opened truncate 路径。

pipe/FIFO 的访问方向只读取 `VfsFile::mode`，private data 只保存 `PipeObject`。
`VfsFile::pipe_generation` 对应 Linux `file->f_pipe`：无 writer 的非阻塞只读 FIFO
open 记录当前 `w_counter`，poll 仅在此 file 存活期间见过新的 writer generation 且
当前 writer 数为零时报告 HUP。poll waiter 依据 `f_mode` 注册 `rd_wait`/`wr_wait`，
用户请求的 event mask 只过滤最终 readiness，不能取消 HUP/ERR 所需的唤醒来源。
`r_counter` 与 `w_counter` 都从一开始就是非零，保留默认 `f_pipe` snapshot 为零的
匿名 pipe HUP 语义。

匿名 pipe 与 pathname FIFO 复用 `PipeObject` 的 read/write 核心。只有 pathname FIFO
的 file-operation wrapper 在一次成功的对外 `read`/`read_iter` 后执行统一 atime
策略，在一次成功的 `write`/`write_iter` 后执行 mtime/ctime 更新；迭代 I/O 内部的
4 KiB chunk 不各自更新时间。`PIPE_BUF` 原子性按调用开始时的完整写请求判定，内部
暂存 chunk 不会把大写入错误地提升为原子写；发生部分写入时，未提交的 source iterator
进度会回退。atime 热路径只读取 `MountFlags` 和 `SuperBlockFlags`，不调用 `statfs`；
RELATIME 使用 Linux 的 `mtime/ctime >= atime` 与“至少 24 小时”边界。

rename 在 syscall 边界用 `RenameFlags::from_bits` 拒绝未知位，并拒绝互斥模式。
VFS rename 入口再次检查组合不变量，文件系统 helper 再检查自身支持的子集。

默认 `FileOperations::read_iter` 使用页大小的内核缓冲区适配标量 read 回调。
一次迭代已经读取部分数据后，后续回调错误转换为已完成字节数，使 stream I/O
保持 POSIX partial-read 语义；首个回调失败时保留原错误。

`libfs::new_pseudo_super_block()` 对应 Linux `init_pseudo()` 与
`pseudo_fs_fill_super()`：统一安装共享 simple `s_op`、`s_magic`、page-size geometry、
标准 private root inode 和具体文件系统的静态 `s_d_op`。root dentry 在
superblock publish 时继承该静态 table；已有 parent/superblock 的新 dentry 在构造或绑定时
继承同一个引用。dentry operation identity 因而不需要 per-dentry `Arc`、mutex 或由具体
文件系统在 `alloc_file_pseudo()` 后补装。

`VfsInode` 的 data lock 串行化 buffered write 与 truncate。shared-file write fault 不取得
该独占锁；`AddressSpace::page_mkwrite()` 取得 address-space invalidate shared lock，再在
folio lock 内重新检查 EOF、调用文件系统 mapping-prepare callback、标脏 folio并完成 PTE
更新。文件系统 `AddressSpaceOperations::set_len()` 在 address-space invalidate exclusive
lock 下执行，并在 backing prepare 后、释放 block 前调用一次
`AddressSpace::truncate_setsize()`。该入口按 Linux `truncate_setsize()` 顺序先发布唯一的
`inode::i_size`，再执行第一次 mapped-view unmap、cached-folio truncate 和第二次 unmap；
第二轮用于清理 cache truncate 窗口中产生的 private COW PTE。Backing filesystem 的 prepare
阶段不得提前修改该 `i_size`，否则这里无法取得真实旧 EOF，可能跳过 folio 丢弃和同页增长区间
清零。

`simple_write_end` 保留在 `kvfs::libfs`，只服务 ramfs/memfs 风格的 aops。
块设备文件系统完成自己的 write-end 后，经 `AddressSpace::write_end_set_size()`
在同一 generic-write data-lock 临界区发布接受后的 `i_size`，而不把 ext4 语义伪装成
libfs helper。

`AddressSpace` 自己持有 object-id、mapped views、静态共享的 `AddressSpaceOperations` 和私有
`PageCache` storage。`PageCache` 不保存 length、object-id、views 或 invalidation
lifecycle；writeback 的 EOF 也由 `AddressSpace` 从 inode 读取后传入。
构造 API 只接受 `&'static dyn AddressSpaceOperations`，因此 aops 不是 inode private object。
Backing-store callbacks 只从当前 `AddressSpace` 的 `mapping->host` 借用唯一 filesystem-private
inode，再经 host 的 `i_sb` 恢复 superblock；filesystem-private inode 因而不需要保存平行
inode identity 或 superblock 回指。
`VfsFile::mapping()` 借用 Linux `f_mapping` 对应物；MM 不接触底层 storage，只有确实需要
超出 file borrow 的生命周期时才显式 clone 该 `Arc`。

Generic buffered-write limit check 只读取 `SuperBlock` 创建时缓存的最大文件大小。
需要按 inode 格式进一步收紧范围的文件系统由 `FileOperations::write_iter()` 调用
`generic_file_write_iter_with_checks()`；该文件系统检查在同一个 inode data critical section
内、进入 page-cache write 前执行，不把 open-file 写策略下沉到 address-space operation。

KVFS 在非 special file 的 writable open 进入文件系统 `open` callback 前增加
inode `write_count`，成功后用 `FMode::WRITER` 记录当前 file 持有该计数，打开
失败或 `FileOperations::release()` 返回后减少。因此 release callback 中读到 1
表示当前 file 是最后一个 writer，对应 Linux `do_dentry_open()` 中的
`get_write_access()`/`FMODE_WRITER` 和 `ext4_release_file()` 的调用时序。当前 KVFS
尚未实现 executable deny-write 导致的 negative count 状态。

## 并发模型

dentry 的 inode 和 children 由各自 mutex 保护；静态 operation table 在首次绑定
superblock 时发布后不再改变。一个 live dentry
对 parent 持有强引用，parent 的 children map 只保存弱索引；superblock dentry cache
强持有仍处于 hashed 状态的 child，直到 unlink、rename、forget 等路径显式驱逐。
这对应 Linux dcache 中 child 引用 parent、零外部引用的 hashed dentry 仍可驻留缓存的
生命周期，同时避免父子强引用环。`DentryKey` 通过持有 parent 的弱引用稳定对象身份，
不需要为每个 dentry 分配额外 ID。`VfsFile` 的 position 和 private data 使用 mutex，
`f_flags` 与 `f_mode` 使用原子整数存储，inode `write_count` 由打开与释放
路径原子更新。bitflags 类型是按值复制的语义快照，不额外引入锁或分配。`VfsFile::f_cred` 是创建后不变的 `Arc<Cred>`，无需额外锁。
FIEMAP writer 只在一次同步调用中借用，不由 VFS 或文件系统保存。inode capability 持有
对应 inode 的 shared lock 覆盖整个回调，避免 `SYNC` 写回后、extent 遍历前插入 mapping
修改；buffered write、shared write fault 和 truncate 都从 exclusive 侧更新 mapping 状态，
同时不扩大成文件系统挂载级写阻塞。

superblock 默认 dentry operations 使用静态引用，对应 Linux `s_d_op`；dentry 只保存继承的
table identity，不把 operation object 变成 dentry-private runtime state。

文件系统类型注册表使用独立 mutex 保护。内建类型在 boot CPU 串行阶段写入；按名称查找
在锁内复制一个静态描述符，枚举则取得列表快照后再格式化输出，不在持锁期间创建
superblock 或执行文件系统代码。

Namespace callback 在 inode namespace lock 下运行。文件系统应在回调返回前把 core
mutation result 同步到受影响的 live inode，并通过 `LockedDentry::instantiate()` 完成创建
对象的 inode attachment；dentry cache 的删除/rebind 仍由 KVFS 在成功返回后完成。

全局 namespace lock 顺序为：

```text
mount-namespace registry mutex（仅 mount tree 操作）
    -> superblock rename mutex（仅 rename）
    -> parent-directory namespace locks
    -> child-directory namespace locks
    -> non-directory namespace locks (pointer order)
    -> per-dentry parallel-lookup mutex
    -> mount topology lock
    -> dentry cache and location locks
```

detach transaction 在丢弃其收集的 `Arc<Mount>` 前先释放 namespace registry mutex。
superblock lifecycle 对应 Linux `SB_BORN`、`s_active`、`SB_DYING` 和 `SB_DEAD`：nascent
block-backed instance 可被 `sget_dev` 查到但不可激活；并发调用者等待 fill 成功或失败。非最后引用直接
递减；最后引用候选保留计数为一并释放 lifecycle lock，取得 umount lock 后重新校验，再
切换到 dying。shutdown callback 运行期间不持有 lifecycle lock，避免 inode eviction
反向进入 mount release；dying/dead superblock 不能取得新的 active 引用。最终 shutdown 在
同一个 registry 临界区内发布 dead、释放 block claim 并删除 identity，再唤醒等待者。显式 filesystem
sync 和 Weak registry 的全局 sync snapshot 也获取 umount lock，不能与 final dcache
eviction 交错，并在取得锁后跳过已经 dying/dead 的 superblock。shutdown 不反向获取
mount-namespace registry mutex。

挂载操作与 namespace validator 都按 inode namespace lock 在外、mount topology lock 在内
的顺序执行，挂载点检查不会引入反向嵌套。parallel-lookup mutex 只序列化同一个 hashed
candidate 的 filesystem lookup callback，不会把不同名称的 slow lookup 串行化。
`SuperBlock::rename_mutex` 对应 Linux `s_vfs_rename_mutex`，不是 dcache rename
seqlock；它只在 cross-directory rename 中获取。`VfsInode::namespace_lock` 表达
Linux `inode->i_rwsem` 中和 namespace 相关的子集。`DentryLocation` 用一个 `RwLock`
同时保护 `parent` 和 `name`，读者不会观察到混合位置。

pathname FIFO 的锁顺序是 inode `fifo_pipe` slot lock 在外、`PipeObject` state lock
在内。release 先在 pipe state 下减少 reader/writer，释放该锁后再进入 slot lock 完成
`files`/slot 生命周期更新，禁止形成反向嵌套。buffer、reader/writer 数、
`r_counter/w_counter` 以及 poll 状态只由 `PipeObject` state lock 保护；waiter wake
在释放 state lock 后执行。

child cache 仍使用 mutex。RCU、seqcount lookup 和 lock-free dcache traversal 在当前
锁模型和回归覆盖稳定前保持在范围外。

## 设计决策

- ABI carrier 与内核语义类型分离，转换尽量靠近边界。
- FIEMAP 使用 `FiemapExtentInfo` 和安全 writer 连接 ABI 与 inode operation，避免把用户
  指针下传，也避免按用户声明容量在 VFS 中分配临时 extent 数组。
- 不提供通用 raw-flags getter；只有写入 ABI 或底层存储时调用 `bits()`。
- 不同 flags 家族不共享整数别名，使错误组合在编译期失败。
- current task 定位只存在于 `kprocess`；`kvfs` 对所有安全相关操作显式接收 `&Cred`。
- `Nameidata` 不保存 credential，避免把一次调用上下文变成冗余对象状态。
- 通用 DAC、父目录修改检查、sticky policy 和初始 owner 由 VFS 统一实现，后端只负责
  自身元数据与 operation callback。
- 打开文件保存 `f_cred`；descriptor 权限与 pathname 权限使用不同入口表达。
- pathname special file 的 `FileOperations::open()` 必须保留 namei 已解析的 `Path`；
  FIFO 使用共享无状态 operation table，runtime session 由 inode 的 typed pipe slot
  持有；禁止使用 per-inode stateful fops、全局 factory、裸指针 key 或 anonymous inode
  替换原 opened-file identity。
- 具体 pseudo filesystem 的显式预初始化由各自 `fs/filesystems` crate 拥有；KVFS 只提供
  hidden mount 所需的通用对象机制，不提供具体 filesystem facade。
- `FileSystemType` 保留 Linux 的“类型描述符 + 全局注册表”层次；POSIX mount 不依赖
  具体文件系统 crate，`/proc/filesystems` 也不维护平行的名称表。
- 不为 immutable simple symlink 增加文件系统私有 target 字段；cached-link 是唯一目标
  owner，operation table 只标识并读取该 inode 状态。
- `AddressSpaceOperations::set_len()` 必须进入 `truncate_setsize()`；文件系统不能自行组合
  i_size、cache resize 和 mmap invalidation，也不能建立第二套 EOF/cache owner。
- `VfsInode` 直接保存唯一的 `Arc<AddressSpace>`，不增加单字段 address-space wrapper；
  `VfsFile` 保存 Linux 风格的 `f_mapping` 引用，MM runtime 只保存 `VfsFile`。
- 锁由语义 VFS 对象持有，而不是由 filesystem bridge 持有。
- `RenameData` 对应 Linux `struct renamedata`，其一次性 `execute` 会消费操作对象并驱动
  VFS orchestration；辅助方法只借用该对象，不为它提供 `Clone`/`Copy`。
- callback closure 是 Path policy 与锁内 Dentry final lookup 之间的窄接口，不新增持久
  状态，也不在锁外保留 lookup 结果；parent inode 由 Path 自身解析，调用者不能传入
  不一致的 Path/inode/dentry 组合。
- `mknodat` 直接由 `Filename` 驱动上述 transaction，不新增 syscall 专用类型或
  transaction 状态结构；`Umode::mknod_node_type()` 只解码 type bits，
  namei 层的 `may_mknod()` 统一分配 Linux 错误并返回通过校验的 `NodeType`。
  syscall 边界将该类型写回现有 `Umode` 后再开始 `dirfd`/pathname 解析。
- namespace lock 使用 blocking lock，因为文件系统 callback 可以执行 I/O。

### SimpleFile 写入模式

普通 `SimpleFileOps` 保留 seekable 文件的覆盖写语义，offset 0 的短写不会隐式截断尾部。
`CommandFile` 表达 procfs/cgroupfs 一类 command-style pseudo file。一次 write/writev
请求先完整复制到最多 4096 字节的内核缓冲区，再作为一个命令交给 callback；文件 offset
和此前写入均不参与命令拼接。callback 同时收到保存 opener credential 的 `VfsFile`，因此
descriptor-based 授权不需要反向查询 current task。这样 command ABI 不会改变其它
`SimpleFile` 使用者的 seekable 文件语义。

`SimpleDirOps::mkdir()` 接收 VFS 已解析并加锁的 parent inode、准备后的 mode 和同一次
pathname 操作的 credential；`rmdir()` 接收 parent inode 与加锁的 victim dentry。动态
文件系统必须基于这些对象提交后端 mutation，不能按裸名称重新 lookup 或查询 ambient
credential。`child_names()` 可传播生命周期或后端错误，使旧目录 fd 的 readdir 不会被
静默转换为空目录。需要持久 identity 的 pseudo filesystem 可用 `SimpleFile::new_inode()`、
`SimpleDir::new_inode_with_owner()` 和 `SimpleDirLookup::{file,dir}_from_inode()` 重用 inode。

## Drop / 资源释放

VFS 对象通过 `Arc`/`Weak` 管理生命周期。unlink、rename replacement 和 forget 会从
parent children 弱索引与 superblock dentry cache 中移除 dentry；仍有 Path/File 引用
时对象继续存活，否则释放时沿 strong parent 链逐级归还引用。每个 `VfsMount` 持有一个
superblock active 引用；最后一个 active mount 释放时执行 writeback、dcache eviction 和
最终 filesystem/device sync。superblock 对象的 `Arc` 最终释放只负责丢弃已经 shutdown 的
root 和私有状态。dentry cache 与 open file 都持有 `VfsInode` 引用；最后一个引用消失后，
hashed VFS inode identity table 先发布 `Freeing`，再让 `SuperBlockOperations::evict_inode()` 获得 final teardown
机会。默认实现只丢弃 inode `AddressSpace` 中的 cached folios，磁盘文件系统可在 nlink=0
时追加 journaled inode 回收。hook 返回后 cache 删除精确匹配的旧 entry 并唤醒同号 lookup；
失败只能记录，但仍不能把正在释放的 identity 重新交给普通操作。唤醒使用被删除 slot 自己的
等待队列，不广播到其它 inode。identity table 的一把 mutex 对应 Linux `inode_hash_lock`；
锁内只做 `(SuperBlock, inode number)` 哈希查找和状态转换，initializer、等待和 eviction hook
都在锁外。key 保存 `Weak<SuperBlock>` 并按其分配地址比较：它不延长 superblock 值的生命，
但会保留 `Arc` 分配直到 slot 删除，因此地址不能在旧 identity 仍 hashed 时复用，也不需要新增
superblock serial。`VfsInode` 同样只弱持有 superblock，避免与 root/dcache 形成强引用环；
filesystem callback 按需执行可失败的 upgrade，不在 bridge 中缓存第二份 superblock owner。
unhashed pseudo inode 不发布 `Freeing` slot，最后一个引用直接执行同一 eviction hook。
per-open
`FileOperations::release()` 先于 final inode eviction，不能代替后者。flags
类型不拥有资源；inode 与文件系统私有资源仍由现有对象生命周期及文件系统回调负责。
FIFO 最后一个 open file 关闭时清空 inode pipe slot，`Arc<PipeObject>` 随最后一个引用
释放；hard link 因共享同一 `VfsInode` 而共享同一活动 session。

## 已知限制

- 尚无完整 capability、LSM、ACL、user namespace ID 映射或 idmapped mount 权限语义；
  `trusted.*` 访问和 `security.*` mutation 暂以 `euid == 0` 近似相应 capability。
- FAT 等不能原生表达 Unix UID/GID 的后端不能完整持久化创建者身份。
- 当前 POSIX rename 路径不支持 `RENAME_WHITEOUT`。
- superblock dentry cache 尚无 Linux 风格 LRU/shrinker。
- fast lookup 仍是 mutex-based，没有 RCU 或 rename sequence validation；slow lookup 已按
  hashed candidate owner/waiter 模型合并同名并发 lookup。
- layered filesystem 的跨文件系统 lock rank 尚未建模。
- mount topology 同步与 superblock rename mutex 仍是不同机制。
- 文件系统类型当前都是静态内建实现，尚无 Linux module autoload、引用计数和
  `unregister_filesystem()` 生命周期。
- `FsContext` 只传递一页 opaque mount data；具体 binary/text format 和支持的 option 集合属于
  各 filesystem type。
