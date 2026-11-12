# kvfs - 设计文档

## Superblock private owners

`SuperBlock::{new_with_flags_and_private,try_new_with_flags_and_private,
initialize_with_private}` take a prepared `KBox<T>` and erase its type without
allocating or moving its pointee. Policy selection and allocation failure now
belong to the filesystem caller; root-initializer errors retain their existing
error type. `SimpleFs` constructors return `VfsResult`, allocate the owner before
calling the root factory, and use KERNEL by default. The explicit allocation-flags
variant supports singleton builders that must not enter synchronous reclaim.

KERNEL callers require sleepable task context, IRQs/preemption enabled, no
non-sleepable RCU section, and no locks needed by reclaim, I/O or filesystem
callbacks. NOWAIT changes only owner storage allocation, not the root callback
or nested container allocations. In particular it does not make mounting IRQ-safe.

| Private-owner preparation path | Policy | Context decision |
| --- | --- | --- |
| SimpleFs default, procfs, cgroup2fs, bpffs | KERNEL | Before the root factory; mount namespace and hierarchy locks are not held |
| devfs and sysfs singleton | NOWAIT | Inside Once initialization; avoid reclaim re-entering the initializing singleton |
| MemoryFs, ramfs, tmpfs mounts, private memfd | KERNEL | Reclaim-safe task context, before root/namespace/fd publication |
| Shared kernel shmem | NOWAIT | KERNEL_SHM_FS mutex remains held to preserve singleton identity |
| ext4 and FAT fill_super | NOWAIT | Canonical device identity is reserved but nascent; avoid new reclaim dependencies on mount completion |
| 9P mount session | KERNEL | Negotiation and metadata transport calls have returned; no transport guard is retained |

These choices cover the new private owner only. NOWAIT does not make input
construction or destruction non-sleeping, and does not imply reserve access or
guaranteed success. NOFS/NOIO still allow direct reclaim; neither is a general
solution for initialization re-entry. Preparing outside an existing singleton
gate would require a separate identity/publication redesign, not a mechanical
owner conversion.

## Parallel lookup completion

Parallel lookup candidates are not completed cache hits. `lookup_cache_entry`
checks `PAR_LOOKUP` while holding the parent's children mutex, so candidate
replacement or removal cannot race with that classification. In-progress
candidates remain hashed for `cache_lookup_candidate` to join; waiters release
the children mutex before waiting on the candidate's lookup mutex and then
retry. A detached negative candidate must never escape as a completed miss
after its owner clears `PAR_LOOKUP`. Completed negative entries in cacheable
directories remain valid cache hits.

## Static simple-directory entries

`DirMapping` retains one inode for each statically declared directory, simple
file, sequence file or supplied `Arc<VfsInode>`. Lookup materializes a dentry
around that inode; disabling or invalidating the dentry cache does not change
the object's identity. Inode-owned subscriptions and state therefore survive
repeated lookups. The mapping releases its inode references on teardown, and
inode aliases and superblock references remain weak to avoid ownership cycles.
Dynamic `add_child` callbacks retain responsibility for their object lifetimes.
File operations still generate or snapshot contents independently on each open.
Symlink entries are cached by the same mapping. kvfs provides
`SimpleFsNode::new_symlink_inode`; devfs consumes it for fixed links such as
`/dev/ptmx -> pts/ptmx`, so the call direction is devfs -> kvfs. The target is
stored in the inode and returned by `read_link` without being re-resolved.

## Same-superblock mount reuse

Mounting a filesystem over the root of its own superblock with no requested
superblock flags (or an empty set) returns the existing `Mount` when its
per-mount flags already contain the request. The mount is not stacked a second
time, so child mounts such as `/dev/shm` and `/dev/pts` remain visible. The
containment test is intentionally one-way: a request that would loosen a flag
is not treated as a remount; callers must use `MS_REMOUNT` to change flags.

## Fallible directory iterators

SimpleDirOps::child_names returns a fallibly allocated KBox iterator. DirMapping
releases its entries lock after collecting the name snapshot; ChainedDirOps owns
both child iterators and releases the first if the second fails. The outer owner
also propagates NoMemory. All use the default KERNEL policy. The getdents path
obtains an owned file handle; VfsFile::iterate_dir samples/updates position with
separate lock acquisitions and holds no position lock over iterate_shared.
SimpleDir::read_dir_at constructs the child iterator before publishing even dot
entries. These paths require sleepable task context, enabled IRQs/preemption,
and no non-sleepable RCU section or lock required by reclaim/IO/FS callbacks.
There is no current held-lock reason to restrict these allocations to NOWAIT or
NOFS; future callers must preserve this contract.

## Fallible mount-transaction private state

`FsContext` owns both private-state slots through `KBox<dyn Any + Send + Sync>`.
The setters return `VfsResult<()>`; allocation succeeds before replacement,
so failure drops the input but preserves the installed state. tmpfs/ext4
initializers propagate failures through the existing mount/remount Result.
These allocations use KERNEL policy in sleepable task context, before mount
publication or superblock reconfiguration locking. The process filesystem
snapshot lock is released before context initialization. Other callers must
also avoid locks required by reclaim IO/FS callbacks. State destruction has
its own context requirements and does not inherit allocation flags.
Boot mounts run from kruntime's preemptible late-init kernel thread after
scheduler initialization, not from the IRQ/preemption-disabled early boot path.
Persistent superblock-private state remains a separate, unmigrated owner.

## Fallible file-lock context

Inodes own advisory-lock contexts through `Once<KBox<FileLockContext>>`.
Initialization uses `KBox::new_nowait` because Once waiters spin, even
though the lock operation itself runs in task context. Failed initialization
returns `NoMemory` through both `FileOperations::lock` and `flock`, publishes
no context, and permits a later retry. The constructor returns `KResult`, so
VFS forwards its error directly. A ready context is reused without
allocation. Queries and unlocking an inode without a context remain allocation
free. Lock-record allocations are unchanged by this migration.

The inode guest regression injects a local initialization failure and verifies
absence, successful retry, and reuse without invoking another allocator.

## Test Layout

Guest tests live in crate-root `ut-tests/`; `unittest::path_mod_test!`
mounts them at their original private parents under `cfg(unittest)`.
There are 25 mounts and 28 modules containing 250 guest cases. The
`address_space::tests` mount retains its `read`, `truncate` and `writeback`
children in `ut-tests/address_space/`, including shared fixtures and the
`observe_readahead_run` re-export used by the production-side test hook.
The other files mirror their source names, with nested names flattened:
`node_dentry.rs`, `node_inode.rs`, `node_device.rs`, and
`node_device_registration.rs`. The dentry module remains `tests_dentry`;
other mounted guest modules remain `tests`.

All guest cases retain their original assertions and parent access, without
additional architecture/feature gates. Seventeen cases retain `serial`
(seven superblock, nine device, one lock); none carries `user`.
Production-owned `cfg(unittest)` hooks remain in place.
The separate `path::test` host module is mounted from `tests/host/path.rs`
under its original `cfg(test)`, with its one `#[test]` case unchanged.
It is not a guest case or a standalone Cargo integration-test target.
The guest mount in `src/path.rs` also has an outer `cfg(unittest)` so a
standalone `rustc --test` wrapper can import this alloc/core-only source
without resolving the kernel `unittest` crate.
Guest validation uses the configured kernel unittest workflow.

## Named character-device registration

`node/device/registration.rs` adds discoverable identities to the existing
cdev registry. `cdev_add_named` installs dispatch and name/class metadata in
one transaction; rejection publishes nothing. Duplicate live class/name identities are also rejected. The registry owns names and
numbers. Snapshot handles retain the exact entry and its retirement bit, so
reusing a number cannot revive an old identity. Subscribers and snapshots
are captured under the registry mutex; add/delete invoke callbacks only after
unlocking. devfs constructs an inode from the returned registration, procfs
reads the current list, and sysfs subscribes and gates attributes on availability.
Existing unnamed cdev_add callers and hardware DeviceNode consumers are unchanged.

The global device-access policy slot is protected by
`RwLock<Option<Arc<dyn DeviceAccessPolicy>>>`. The write path is
`register_device_access_policy` (a second registration while one exists is
rejected, and passing `None` clears the slot and drops the registered policy,
returning device access to the unrestricted state); the read path is
`check_device_access` at every open/mknod enforcement point — the `Arc` is
cloned and the read lock released before the policy callback runs, so
callbacks always execute outside the lock.


## 定位

`Dentry::notification_context` captures linkage and an interested parent inode
under one location read lock. It copies the name only when the parent subscribes
to the requested event bits. Bind roots retain their actual dentry parent; no
synthetic path across a mount root is constructed. Callbacks run after the lock
is released. Unlink and replacement mark the removed dentry's location as
unlinked without discarding its former parent/name; cache eviction does not.

Each inode caches an atomic notification event mask. The notification registry
serializes all aggregate updates, including subscription replacement and removal.
The cache filters work; the notifier still validates each watch at delivery.
Namespace event records retain inode references captured under the operation's
namespace locks, keeping routing independent of mount IDs and name reuse.

`kvfs` 提供 Linux 风格的 VFS 对象模型，包括 superblock、mount、dentry、inode、
open file、路径解析和通用文件系统 helper。具体文件系统通过 inode/file operation
traits 接入，POSIX 层负责把 syscall ABI 参数转换成 VFS 语义对象。`kvfs` 同时拥有
namespace validation、lock ordering、dcache identity、类型化 operation flags，以及
`VfsInode` 上的 page-cache attachment。

权限模型与 Linux 保持同一层次：`kcred` 定义 `Cred`，syscall 层从当前 task 取得一次
credential snapshot，`kvfs` 接收显式 `&Cred` 并完成路径遍历和通用 DAC。`kvfs` 不
依赖 `kprocess`，因此可被 boot、内核伪文件系统和测试以明确身份复用。

## 字符设备分派

字符设备注册表和 PTMX 等 open 时选择后端的 multiplexer 共用
`open_character_device`。私有 `ChrdevFileOperations` 统一转发能力、I/O、
poll 和 open/release；文件系统节点不再复制这层适配。
安装替换表不会自动再次调用 open，统一入口负责调用选定后端一次。
VFS 在 open 成功后按最终操作表计算能力，末引用释放时调用最终后端 release。
注册表查找锁不跨后端回调；设备号的注册与注销仍由调用方拥有。

### Device-access policy (reverse injection)

kvfs defines the `DeviceAccessPolicy` trait and the registration entry
`register_device_access_policy`, and invokes the policy at three enforcement
points: character-device `open` (`open_character_device`) and block-device
open (both map the open mode onto READ/WRITE bits and OR in the inode's
TYPE_CHAR/TYPE_BLOCK), and device-node `mknod` (`MKNOD | TYPE_*`). The
implementor is `bpffs`, which owns the controller state (the cgroup-v2 device
controller) and injects it during `init_bpf_fs`. The dependency direction is
inverted: kvfs does not depend on bpf/cgroup, and policy callbacks run after
the registry read lock is released. With no policy registered, device access
is unrestricted; a denial answers `VfsError::OperationNotPermitted` (`EPERM`).

## 块设备文件尺寸与范围操作

块设备有两类刻意分离的尺寸视图，与 Linux 一致：设备节点的
`getattr`/`fstat` 元数据保持零尺寸（Linux 设备节点同样如此），而
`VfsFile::size()`——`lseek(SEEK_END)` 的基准——实时读取
`BlockDevice::size()`，对应 Linux 的 `bd_inode`。容量随 loop
attach/detach 等操作即时变化，不缓存快照；记录锁的 `SEEK_END` 基准是节点
尺寸，append 写保持在调用者位置（`blkdev_write_iter` 不做重定位）。

`fallocate` 的模式合法性（未知 flag、`PUNCH_HOLE` 与 `ZERO_RANGE` 互斥、
`PUNCH_HOLE` 必须带 `KEEP_SIZE`、COLLAPSE/INSERT/UNSHARE 的排他性）由
公共入口按 `vfs_fallocate` 统一校验；块设备随后由
`DefaultBlkdevFileOperations` 按设备语义执行，不走普通文件的逐字节模拟：
容量（越界且无 KEEP_SIZE 则 `EINVAL`）与逻辑块对齐（`EINVAL`）先于任何
介质写入校验，通过后仅 `ZERO_RANGE`（± `KEEP_SIZE`）对齐地整段写零。
`PUNCH_HOLE` 携带释放语义：Linux 以 `BLKDEV_ZERO_NOFALLBACK` 调用，设备
具备 write-zeroes/discard 卸载时成功、否则 `EOPNOTSUPP`。块设备契约
`BlockDeviceOperations` 目前尚未定义这两类能力位，本内核有意收窄为对所有
后端返回 `EOPNOTSUPP`（按能力缺失上报，而非软件写零冒充释放）。没有这一步，依赖文件尺寸的上层语义（如
`ext2fs_zero_blocks2` 的清零请求）会把整个设备视为空而静默跳过实际工作。

字符设备统一按同一 major 内的非空 minor 范围注册，单设备使用长度 1。
注册表用按 `(major, first_minor)` 排序的 BTreeMap 查找覆盖范围；重复、包含
或部分重叠均返回错误，不隐式替换已有驱动。注销必须匹配完整起点和长度。

inode 只弱引用一次注册对象；该对象拥有操作表与单向退役标记。注销在释放
注册表锁之前标记退役，新注册产生新对象，即使复用同一操作表也不会复活旧
缓存。下一次打开忽略退役缓存并重新查表，不需要保留或遍历 inode 反向列表。
已选定的操作表和已打开文件仍持有后端引用；设备下线后的准入由后端负责。
注册、注销、打开使用可睡眠上下文；后端回调及最终析构均在注册表锁外执行。
VFS 不判断路径是否来自 devpts，也不负责 PTY 实例生命周期。

普通 open 的最终 magic link 与路径查询使用相同的对象跳转，不将 readlink
展示字符串重新解析为路径。因此 procfd 可保留已取消链接 inode 的身份，且
不会因同名路径重新创建而打开另一个对象。跳转仍受跟随策略、链接次数与
目标打开权限检查约束；普通文本符号链接继续按其目标字符串解析。

## 文件同步契约

显式接入 `poll_file_io` 的文件操作先尝试 I/O；非阻塞无就绪直接返回。
阻塞等待被任务信号打断时只再尝试一次，仍无法完成才返回文件等待中断标记。
对象锁不得跨等待；future 拥有并清理订阅。对象和 iterator 仍负责保存已完成量，
该标记不能单独证明整个请求可以重放。普通操作错误和注册失败不升级为此标记；
是否转换成 syscall 重启码由已审计的 syscall 出口决定。

管道读写接入该等待流程，FIFO 打开时的伙伴等待仍使用原接口。管道单次写入的
完成量及向量写入跨块累计量优先于中断；小于等于 PIPE_BUF 的请求仍保留整体
原子写约束。重查同时观察数据、空间及端点关闭，不能将 EOF/EPIPE 改成重启。

`transfer_files` 接收文件及可选的内核态偏移，管理整个请求的完成量和位置提交；
它不接收用户指针，也不生成 syscall 重启码。传输栈分两层：边界层把
`TransferPosition` 协议解析成显式起点（地址序位置锁、同 open description
共锁、非定位文件的惰性槽位都在 `resolve_pair_positions` 结算一次），引擎层
只看到显式偏移，不再接触 `f_pos`。所有形状共享同一个结算泵：每步经
staged→确认→只消费确认前缀的管道语义或定位读写搬运至多一块，错误后有进度
返回已完成字节数，仅零进度失败向外传播。文件间传输使用 4 KiB 中间缓冲；
管道腿按步重取位置锁，锁不跨管道就绪等待；两个管道按对象地址序直接对转，
不经缓冲。

`copy_file_range` 引擎在同一临界区内完成范围校验与拷贝，顺序对应 Linux
`generic_copy_file_checks`：目录报 `EISDIR`，非普通文件报 `EINVAL`，目标以
`O_APPEND` 打开报 `EBADF`；配对准入紧随其后、早于回绕检查与 EOF 截短；范围按
无符号加法回绕报 `EOVERFLOW`，请求先按源 EOF 截短，再受目标最大文件尺寸约束
（起点达到上限即报 `EFBIG`，即使请求已被截短为零），同 inode 重叠区间报
`EINVAL`。拷贝允许部分成功：错误后有进度返回已完成字节数并提交对应位置，
已发布的位置始终描述目标实际接受的字节。
配对准入按 Linux `f_op->copy_file_range` 的分支结构落在目标端操作表的
原生拷贝原语 `copy_chunk_from` 上：目标表携带原语时由其自行判定输入兼容性
（不兼容的存储由实现返回 `EXDEV`，VFS 不做身份比较），块级原语由泵驱动，
结算仍只在泵里定义一次；目标表无原语时退回同超级块通用泵，跨超级块返回
`EXDEV`。准入用一次 `len == 0` 的原语查询完成，位置与 Linux 一致地排在 EOF
截短之前，因此不可服务的跨超级块空请求同样报 `EXDEV`，而不是被截短为零后
静默成功（`copy_file_range01` 的跨设备能力探测正是这一形态）。memfs 落真实
原语（memfs 存储到 memfs 存储的页缓存搬运），memfs 实例间的跨超级块拷贝因此
是真实能力。

管道复制由 `PipeObject` 管理消费边界：输入管道仅移除目标已接受的前缀，输出管道
先在 mutex 内确认空间再调用源读取。两个管道按对象地址顺序持有 mutex，直接转移
已接受的字节；释放锁后通知读写者。非管道回调可以睡眠，但不得重入管道。
回调接口仅供 VFS 内部使用，可在管道锁内获取非管道文件的位置锁；
调用者不得持位置锁进入管道操作，
等待管道自身就绪时不持管道锁或文件位置锁。

`VfsFile::fsync(data_only)` delegates to `fsync_range(0..u64::MAX, data_only)`.
Both use the one `FileOperations::fsync(file, range, data_only)` capability.
Ranges are half-open bytes; backwards ranges return `EINVAL`, empty ranges still
reach the filesystem, and the full sentinel never requires adding one to MAX.
Path-only files fail with `EBADF` before dispatch; readonly opens may sync.
Invalid range/context does not consume the file's error cursor. After dispatch,
the backend error takes precedence while recorded mapping history is observed.

Missing fsync capability returns `EINVAL`. Pipe, FIFO, socket and generic character
devices explicitly reject it; replacement character operation tables decide their
own support. Memfs, simple directories and nullfs explicitly succeed for volatile
storage. Block files perform the real device flush, including on empty ranges.
9p forwards the intent to server fid fsync and may synchronize more than the
requested range. Ext4 composes data completion with its metadata/journal/device
duties. All synchronous entry points require sleepable task context, no held
spinlock or IRQ/preemption guard, and no current userspace process.

## Captured Poll Sources

Open-file operations may supply one `FilePollSource` containing a weak source
and its supported events. `VfsFile` forwards capture to its file operations.
Ordinary backends return no captured source
and retain their existing registration path. Capture fixes wait-source identity,
not readiness: consumers still query the file in the current calling context.
Event filtering and weak registration introduce no additional owner or source
list. An expired captured source must not fall back to the default path.

## 范围

- `src/open_flags.rs`：原始 `O_*` 参数的规范化与 open intent。
- `src/namei.rs`：路径遍历和 open 流程。
- `src/permission.rs`：owner/group/other DAC 与 open access 映射。
- `src/node/`: dentry, inode, inode-identity state machine and filesystem
  operation traits; also the character/block device file operations and the
  device-access policy registration (see "Device-access policy (reverse
  injection)").
- `src/address_space/`：inode-owned `AddressSpace`、其直接持有的 `MappingPages` 容器，以及
  writeback 与 truncate/invalidation 边界。`mod.rs` 定义对象和构造；`operations.rs`
  定义 callback 与默认实现；`filemap.rs`、`readahead.rs`、`writeback.rs`、`truncate.rs`
  分别承载算法编排；`views.rs` 管理注册与失效通知；crate 根目录的
  `ut-tests/address_space/` 按算法组织回归。
- `src/fiemap.rs`：与用户 ABI 解耦的 inode FIEMAP 请求状态、标志和安全输出接口。
- `src/file.rs`：打开文件及其可变状态。
- `src/file_ra.rs`：每次打开文件的预读历史、窗口选择与错误退让。
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
`VfsInode::current_time()` rounds and clamps the current time;
`truncate_timestamp()` applies the same conversion to explicit values. Explicit
setattr, automatic I/O updates, and namespace helpers apply superblock policy
before comparison and filesystem callbacks. Backends then apply format-specific
rules such as FAT field precision. Explicit setattr returns effective values in
`MetadataUpdate`, and KVFS publishes only those fields. Automatic `update_time`
callbacks publish resident timestamps themselves; the default likewise publishes
only fields confirmed by setattr. Ignored fields are never restored from the
original request. Raw setters only store values without repeating policy lookup.
This follows Linux's separation between `current_time()`/`timestamp_truncate()`
and `inode_set_*_to_ts()`, while allowing FAT-style distinct atime and mtime/ctime
invariants.

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
generic VFS. Driver open runs after reservation, outside the registry mutex. The once-installed
claim corresponds to Linux `s_bdev_file`, separate from reserved `s_bdev` identity.
Failure/final shutdown closes it before unregistering and waking mount waiters.
VFS publishes the available lifecycle only after fill succeeds.
已有 block-backed superblock 与新请求的 `SB_RDONLY` 不一致时返回 `EBUSY`，对应 Linux
`get_tree_bdev_flags()` 的已有 `s_root` 分支，而不是将 block mount 降格为 per-mount 策略。
与 Linux `bdev_file_open_by_path` 一样，可写 mount 会拒绝
canonical read-only device；只读 mount 仍可继续交给 filesystem fill-super。`fs_flags` 字段是类型化
的 `FileSystemTypeFlags`（bitflags），对应 Linux `struct file_system_type::fs_flags`，
其中 `REQUIRES_DEV` 声明是否需要 backing device，位编号与 Linux `include/linux/fs.h`
对齐，供 `/proc/filesystems` 的 nodev 列和 mount 错误路径判断；未来按 Linux 语义
新增标志（如 binary mountdata、subtype）时保持同一编号体系即可扩展。

block-special inode 统一安装 KVFS 的 `DefaultBlkdevFileOperations`，对应 Linux
`def_blk_fops`. Only open looks up `rdev`; existing file-private storage retains
`BlockDeviceClaim`. Capacity, I/O, ioctl and close use that original device even
after withdrawal or device-number reuse. The claim preserves `O_EXCL`; ioctl uses
the current nonblocking flag. Last-file release closes once.
块设备特殊 inode 尚无 canonical block address space，mmap 返回 ENODEV；启用前必须同时实现介质变更失效协议。
KVFS supplies checked argument access and `has_sys_admin` from the current caller.
The ioctl path receives the syscall caller's `&Cred`, without retaining extra credentials.
通用 block 命令及未知命令的 driver fallback 统一由 `BlockDevice::ioctl` 分派。普通 close 和 write 不隐式 flush，显式 fsync 才把
durability 请求传给 backend。KVFS 不维护第二张 `dev_t -> DeviceFileOps` 表；
devfs 只投影名称与 `rdev`，loop 设备也按普通 `Gendisk` 发布。

块设备文件的 byte I/O 先按当前容量裁剪。若 offset 和裁剪后的长度均按 backend
block size 对齐，整个借用 buffer 经一次 general block 调用提交，利用既有的连续多块
契约，避免每个 sector 都重复提交和等待；不跨文件调用合并，也不增加数据复制。
未对齐请求保留逐块读取或 read-modify-write。一次多块调用只有整体成功或错误结果，
KVFS 不猜测失败请求内部完成了多少 sector，不在错误后自动拆小重试写入。

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

The `openat2(2)` constraints `BENEATH`/`IN_ROOT`/`NO_XDEV`/`NO_SYMLINKS`/`CACHED`
are invariants of the namei state machine, not a syscall-layer wrapper: every
`enter` of a new path checks anchored-subtree membership and the starting mount,
symlink hops inherit the same root and mount, and membership is re-checked before
delivery. `RESOLVE_CACHED` returns `EAGAIN` before a slow lookup, a create or a
truncate. `sys_openat2` therefore only validates the arguments, selects the
dirfd/cwd base and decides whether the lookup is anchored; it does not duplicate
the lookup policy.

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
- cross-directory rename 先获取 superblock topology mutex，再按拓扑顺序锁父目录。无祖先关系
  时改用 `Dentry::namespace_lock_key()`（祖先 inode 指针链，再按指针）定序；`pivot_root`
  的多个 inode 取锁使用同一规则。rename 的 participant inode 是其后的独立阶段，不属该全序。

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
effective `CAP_DAC_OVERRIDE` overrides ordinary mode bits; effective
`CAP_DAC_READ_SEARCH` additionally grants exact read (and search for
directories). Neither grants execute permission on a regular file with no
execute bit set, matching Linux `generic_permission()`.

namespace 修改由 `Path` 或 `Filename` 在调用文件系统回调前统一检查：
create、mkdir、mknod、symlink、link、unlink、rmdir 和 rename 要求父目录
`MAY_WRITE | MAY_EXEC`。所有 create-like 操作由锁内 callback 调用共同的 `Path::vfs_*`
能力；unlink、rmdir 和 rename 把 Path policy 作为 validator 传入 Dentry 操作。
这些 policy 在父目录锁、所需 participant lock 和最终 lookup 结果仍然有效时执行。
Deletion and replacement in a sticky directory require effective
`CAP_FOWNER`, directory ownership, or ownership of the final victim; the
mountpoint check applies to that same dentry.

inode metadata 修改也由 `Path` 统一授权，再进入同一个后端 `setattr` callback：

- `chown` requires effective `CAP_CHOWN` to change the UID; the owner may
  preserve the current UID and change the GID to a current or supplementary
  group;
- `chown` also applies Linux `chown_common()` set-ID cleanup. Directories keep
  their mode. Other nodes always lose set-UID, and lose set-GID when group
  execute is set or the caller is not in the pre-chown GID and lacks effective
  `CAP_FSETID`. If cleanup changes the mode, Linux additionally drops set-GID
  unless the caller is in the new GID or holds effective `CAP_FSETID`; that
  new-GID condition does not apply when a GID change leaves both set-ID bits
  untouched. The generated mode update requires inode ownership or effective
  `CAP_FOWNER`, so `CAP_CHOWN`/`CAP_FSETID` alone do not authorize it and
  `chown(-1, -1)` stays a no-op when no set-ID bit changes;
- `chmod` requires inode ownership or effective `CAP_FOWNER`; retaining a
  requested setgid bit additionally requires the target GID to belong to the
  caller or effective `CAP_FSETID`;
- `set_times` distinguishes touch from an explicit times array. Empty times or
  two current-time values permit the owner, effective `CAP_FOWNER`, or a caller
  with write permission on the inode; explicit values and mixed
  `UTIME_NOW`/`UTIME_OMIT` permit only the owner or effective `CAP_FOWNER`.

`SetattrTime` 只承载单次调用中“当前值/显式值”的授权信息，落盘的 `MetadataUpdate`
只包含解析后的 atime/mtime/ctime 值，不在 inode 或 namei 状态中保存调用上下文。
Automatic I/O updates use `InodeUpdateTime`, corresponding to Linux
`FS_UPD_ATIME` / `FS_UPD_CMTIME`, and invoke filesystem `update_time`. Authorized
explicit values pass through `VfsInode::truncate_timestamp()`; automatic values
come from `current_time()` before comparison. Path retains mount/relatime atime
checks; the callback decides whether mtime/ctime changed. It receives the resident
inode, returns `VfsResult<()>`, and owns publication; KVFS does not invoke setters
again. The default and filesystem fallbacks share `generic_update_time`: compare
`times()`, skip unchanged values, otherwise call setattr and publish only confirmed
timestamps. Backends owning resident attributes may compare, update, and mark dirty
in one critical section, deferring persistence under filesystem sync rules. They
may reduce precision, couple fields, or ignore unsupported fields; raw setters do
not repeat capability policy.

`InodeOperations::setattr` 的默认实现是公共回退层，对应 Linux `notify_change()` 把
没有 `->setattr` 的 inode 路由到 `simple_setattr()` 的形态：owner/mode/时间更新被接受并
发布到 resident 属性，因此使用 `empty_inode_operations()` 的无后端 inode（匿名管道、
伪文件系统根等）无需专属表即可支持 `fchownat`/`fchmod`/`utimensat` 与自动
I/O 时间更新。回退层的前提是 inode identity 独立、metadata 仅驻留于自身；刻意共享
identity 的 inode 不满足该前提，必须显式覆写 `setattr` 保留拒绝语义——anon_inodefs
singleton 即此策略，它被所有进程的 eventfd/epoll/timerfd/signalfd/pidfd/socket/BPF
匿名 fd 共同引用，接受一次 fallback 更新就会改写所有进程 `fstat` 自己匿名 fd 时看到的
共享属性。回退层拒绝 size 更新并以 `EINVAL` 报错：这类 inode 没有可截断的存储，
发布伪造的 `i_size` 会破坏语义；这与 Linux 在 syscall 边界对非普通文件 truncate 返回
`EINVAL` 的可达行为一致，而自带存储的文件系统（memfs、kext4、fat、9p 等）各自实现的
`setattr` 不受影响。授权仍然只发生在 `Path` 层，回退表不重复执行 policy。

Xattr policy also flows through `Path`, which applies mount, namespace, and DAC
policy before entering `InodeOperations::{get,list,set,remove}_xattr`.
get/set/remove inputs keep names in a checked kernel `Vec<u8>`; list passes
complete names incrementally through `XattrNameRef` and `XattrNameSink`,
allowing backends to add namespace prefixes without copying and letting `Path`
filter `trusted.*` before returning names to the caller. Both representations
preserve non-UTF-8 suffixes intact. `XattrSetFlags` is converted from ABI raw
bits before reaching a backend, and the `CREATE|REPLACE` combination remains a
constraint requiring both flags to be set. `user.*` is allowed only on regular
files, directories, and sockets, and writes to a sticky directory require
directory ownership or effective `CAP_FOWNER`. `trusted.*` is visible only to
callers holding effective `CAP_SYS_ADMIN`. Reads of `security.*` retain the
Linux VFS LSM delegation model, but until concrete LSM hooks are connected,
set/remove require effective `CAP_SYS_ADMIN`, which approximately prevents
unprivileged callers from forging security attributes; `system.*` remains
delegated to the filesystem or ACL layer. Before any namespace branch, the
common write check rejects set/remove on inodes carrying
`NodeFlags::IMMUTABLE` or `NodeFlags::APPEND_ONLY` with `EPERM`; reads are
unaffected. Set/remove call the backend under the inode data lock, and each
filesystem must atomically resolve all four `CREATE`/`REPLACE` combinations
inside its own transaction or lock.

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

`MntNamespace::pivot_root()`（`pivot_root(2)` 语义）把 namespace 的可见 root 换成
`new_root` 指向的已注册 mount，并把原可见 root 挂载到 `put_old`（OCI runtime 随后以
`umount2(MNT_DETACH)` 摘除）。要求：`new_root` 是已注册 mount 根、`put_old` 是目录且
位于 `new_root` 子树内——允许 `put_old == new_root`（runc 的 `pivot_root(".", ".")`
路径），`put_old` 被其他挂载覆盖时保留被覆盖层。手术在 registry mutex + 受影响 inode 的 namespace 写锁
（按 `Dentry::namespace_lock_key()` 的全序、按 Arc 去重）下执行，并在持锁后就绪地复查旧可见
root 仍挂在其自身 parent mountpoint（读栈可能早于取锁），不满足则返回错误而不是断言：先把 `new_root` 从原位置摘除并恢复其覆盖
层，使其压栈到 namespace 的 structural root 之上，再把原可见 root 从其被 `new_root` 覆盖的
位置取出（其自身覆盖链仍随后续 root 保留，对应 Linux 的 stack-preserving move），最后把
原 root 挂到 `put_old`。所有可失败校验在首次修改前完成，保证失败的 pivot 不留下半编辑的
树。`mount_edge_detach` 是共享的单边摘除原语（恢复 covers/删除 child 索引 + 清空
location），供 pivot 与后续 MS_MOVE 复用。

`MntNamespace::make_mount_private()`/`make_mount_slave()` 记录一次性传播属性。KVFS 尚无
共享传播组，因此 slave 与 private 行为等价：没有共享 master 时 slave 不接收任何传播事件。
两者各自校验调用（`MS_PRIVATE`/`MS_SLAVE` 可带 `MS_REC`/`MS_SILENT`，其余组合返回
`EINVAL`），防止把非法 flags 静默吃掉。递归 bind（`MS_BIND | MS_REC`）仅在源子树挂载数为
零时接受——此时与克隆单 mount 的结果一致；否则返回 `EINVAL` 而不是静默压平挂载树。

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
进度会回退。atime 热路径只读取 `MountFlags` 和 `SuperBlockFlags`，不调用 `statfs`，
也不构造完整 `Metadata` 快照；RELATIME 与“等于当前时间”早退谓词通过
`InodeAttributeOperations::times()` 在单次存储锁内读取驻留 atime/mtime/ctime，对应 Linux
直接读取 `inode->{i_atime,i_mtime,i_ctime}` 而非填充 `struct kstat`；
RELATIME 使用 Linux 的 `mtime/ctime >= atime` 与“至少 24 小时”边界。

rename 在 syscall 边界用 `RenameFlags::from_bits` 拒绝未知位，并拒绝互斥模式。
VFS rename 入口再次检查组合不变量，文件系统 helper 再检查自身支持的子集。

默认 `FileOperations::read_iter` 使用页大小的内核缓冲区适配标量 read 回调。
一次迭代已经读取部分数据后，后续回调错误转换为已完成字节数，使 stream I/O
保持 POSIX partial-read 语义；首个回调失败时保留原错误。
默认 `write_iter` 只推进已被标量 write 接受的迭代器与 `ki_pos`，短写回退剩余输入；
`i_size` 由 `write_end` 或实际写入 backing store 的文件系统回调发布。

`Kiocb::flags()` records the open file description's `O_DIRECT`, `O_DSYNC`, and
`O_SYNC` state in `KiocbFlags` when the control block is created, mirroring Linux
`iocb_flags()`. `O_SYNC` contains `O_DSYNC` in its encoding, so a synchronized request
sets both `SYNC` and `DSYNC`. A control block is built per operation, so an `F_SETFL`
change applies from the next operation. The flags are recorded only: no read or write
path dispatches on them or changes completion behavior yet.

`libfs::new_pseudo_super_block()` 对应 Linux `init_pseudo()` 与
`pseudo_fs_fill_super()`：统一安装共享 simple `s_op`、`s_magic`、page-size geometry、
标准 private root inode 和具体文件系统的静态 `s_d_op`。root dentry 在
superblock publish 时继承该静态 table；已有 parent/superblock 的新 dentry 在构造或绑定时
继承同一个引用。dentry operation identity 因而不需要 per-dentry `Arc`、mutex 或由具体
文件系统在 `alloc_file_pseudo()` 后补装。

The `VfsInode` data lock serializes buffered writes and truncate. Shared-file
write faults do not take this exclusive lock: `VfsFile::page_mkwrite()` holds
address-space invalidate shared protection, then under the folio lock rechecks
EOF, calls the filesystem prepare hook, dirties the folio, and publishes the PTE.
`AddressSpace::writepages_control()` holds invalidate shared protection through
enumeration and submission; synchronous implementations also cover backing I/O.
Asynchronous filesystems must retain inode/mapping ownership separately and drain
requests before set-length preparation or physical release. Filesystem
`InodeOperations::set_len()` runs under invalidate exclusive protection and calls
`VfsInode::truncate_setsize()` once after backing preparation and before block
release. That entry publishes the sole `i_size`, unmaps views, truncates cached
folios, then unmaps again to remove private COW PTEs created during cache truncate.
Backing preparation must not publish `i_size` early, so the common boundary retains
the old/new EOF transition.
`VfsFile::fallocate()` dispatches the backend under the same lock shape (the data
lock and the invalidate exclusive lock) across both the backing change and the
visible-size publish: writeback samples the visible size per batch under the
invalidate shared lock, while fallocate-style backends commit the backing disk size
first and publish the visible size afterwards; without excluding concurrent
writeback, a batch carrying a stale visible-size snapshot would enter the backing
store after the disk-size advance and spuriously fail against the "writeback range
not yet visible" guard. A backend publishing growth inside the dispatch's
held-lock context must call `VfsInode::truncate_setsize()`; the self-locking
`update_size_after_backing_change()` / `grow_size_after_backing_write()` helpers
deadlock in that context. Before taking those locks, `VfsFile::fallocate()`
queries `FileOperations::supported_fallocate_modes()` and answers `EOPNOTSUPP`
for any requested mode bit outside the backend's declaration; the default empty
mask admits plain allocation (mode 0) and rejects every other mode, so backends
supporting only plain allocation need no override. The syscall layer consults the
same mask before dispatching: a backend that declares PUNCH_HOLE/ZERO_RANGE owns
those modes, and a mode it does not declare (kext4 today) is emulated locally by
zeroing the range, which frees no blocks.
无独立 backing 的文件使用 inode operation 的默认截断实现；有 backing 状态的文件系统
覆盖它，并在调用 `truncate_setsize()` 前完成自身准备。

`AddressSpaceOperations` 只提供 folio 读取、写回及缓存状态回调；不再通过字节级
`read_at`/`write_at` 默认转接。文件系统需要的字节读取可留在其私有 backing helper 中，
由 `read_folio` 和 `readahead` 显式复用。

`simple_write_end` 保留在 `kvfs::libfs`，只服务 ramfs/memfs 风格的 aops。
块设备文件系统完成自己的 write-end 后，经 `AddressSpace::write_end_set_size()`
在同一 generic-write data-lock 和已复制 folio 锁临界区发布接受后的 `i_size`，而不把 ext4 语义伪装成
libfs helper。

`AddressSpace` 自己持有 object-id、mapped views、静态共享的 `AddressSpaceOperations` 和
`i_pages: MappingPages`。该字段直接组合缓存容器，不单独分配 `Arc`，对应 Linux
`address_space::i_pages`；Folio 使用 `Arc<Folio>` 同时保持物理页、稳定 identity 和内部页锁生命周期。
`read_folio` 从 Folio 自身获取稳定 index；AddressSpace 构造不再传入仅控制诊断的 mapping kind。
`MappingPages` 不保存 length、object-id 或 views。`AddressSpace` 持有 cache-invalidate
lock，用于把 readahead 插入、writeback、page_mkwrite 与 truncate 串行到同一个
mapping。writeback 通过 owner 闭包在每个 folio 锁定后从 inode 读取实时 EOF。
构造 API 只接受 `&'static dyn AddressSpaceOperations`，因此 aops 不是 inode private object。
Backing-store callbacks 只从当前 `AddressSpace` 的 `mapping->host` 借用唯一 filesystem-private
inode，再经 host 的 `i_sb` 恢复 superblock；filesystem-private inode 因而不需要保存平行
inode identity 或 superblock 回指。
`InodeOperations::cache_residency()` supplies optional filesystem-owned admission
and removal callbacks once, before inode publication. Resident hits perform no
quota lookup or superblock recovery. The common cache contains no capacity policy.

Memory-file sparse reads and native copies share a memfs-owned reader. The generic
`AddressSpace::visit_resident_folios()` capability holds one invalidation read
guard for at most 31 pages and performs one bounded index snapshot. Initialized
resident hits then need only their folio lock. Retained initialization failures
are retried, not reported as holes. The callback samples EOF and selects data or
zeroes; no sparse policy or readahead is added to generic cache traversal.

`VfsFile::mapping()` 借用 Linux `f_mapping` 对应物；MM 不接触底层 storage，只有确实需要
超出 file borrow 的生命周期时才显式 clone 该 `Arc`。

Generic buffered-write limit check 只读取 `SuperBlock` 创建时缓存的最大文件大小。
需要按 inode 格式进一步收紧范围的文件系统由 `FileOperations::write_iter()` 调用
`generic_file_write_iter_with_prepare()`；该回调在同一个 inode data critical section
内组合文件系统策略、通用大小限制和自动时间戳发布，进入 page-cache write 前执行，
不把 open-file 写策略下沉到 address-space operation。
空请求也执行策略检查；被拒绝或裁剪后为空的请求不更新时间戳。

KVFS 在非 special file 的 writable open 进入文件系统 `open` callback 前增加
inode `write_count`，成功后用 `FMode::WRITER` 记录当前 file 持有该计数，打开
失败或 `FileOperations::release()` 返回后减少。因此 release callback 中读到 1
表示当前 file 是最后一个 writer，对应 Linux `do_dentry_open()` 中的
`get_write_access()`/`FMODE_WRITER` 和 `ext4_release_file()` 的调用时序。当前 KVFS
尚未实现 executable deny-write 导致的 negative count 状态。

`WritebackControl` 由 pagecache 定义，KVFS reexport 同一类型。扫描和提交同步借用
同一个控制对象；预算按交给后端处理的页数扣减，错误或拒绝不退还，完成前缀不改变预算。byte-range callback 的 `WritebackRangeResult` 是
`Result<usize, (usize, VfsError)>` 的别名，
表达完成前缀或完成前缀与错误，不再导出独立 writeback outcome 类型。

## 并发模型

dentry 的 inode 和 children 由各自 mutex 保护；静态 operation table 在首次绑定
superblock 时发布后不再改变。一个 live dentry
对 parent 持有强引用，parent 的 children map 只保存弱索引；superblock dentry cache
强持有仍处于 hashed 状态的 child，直到 unlink、rename、forget 等路径显式驱逐。
这对应 Linux dcache 中 child 引用 parent、零外部引用的 hashed dentry 仍可驻留缓存的
生命周期，同时避免父子强引用环。`DentryKey` 通过持有 parent 的弱引用稳定对象身份，
不需要为每个 dentry 分配额外 ID。`VfsFile` 的 position 和 private data 使用 mutex，
流式文件的普通读写入口不获取 position mutex，也不推进文件偏移，避免阻塞读
把同一 open-file 上的写操作串行化；非流式文件仍锁定并更新共享偏移。
`f_flags` 与 `f_mode` 使用原子整数存储，inode `write_count` 由打开与释放
路径原子更新。bitflags 类型是按值复制的语义快照，不额外引入锁或分配。`VfsFile::f_cred` 是创建后不变的 `Arc<Cred>`，无需额外锁。
FIEMAP writer 只在一次同步调用中借用，不由 VFS 或文件系统保存。inode capability 持有
对应 inode 的 shared lock 覆盖整个回调，避免 `SYNC` 写回后、extent 遍历前插入 mapping
修改；buffered write、shared write fault 和 truncate 都从 exclusive 侧更新 mapping 状态，
同时不扩大成文件系统挂载级写阻塞。

由外部注册表驱动命名空间的动态文件系统通过
`Dentry::invalidate_cached_child()` 报告带外增删。该接口在父目录 namespace
exclusive lock 下同时驱逐 positive/negative child；目录 child 还递归 forget 已缓存后代。
文件系统只应失效实际变化的最高层路径，以保持未变化目录及其 mountpoint 的 dentry
身份。

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
- `InodeOperations::set_len()` 必须进入 `VfsInode::truncate_setsize()`；文件系统不能自行组合
  i_size、cache resize 和 mmap invalidation，也不能建立第二套 EOF/cache owner。
- `FileOperations::fallocate()` must publish visible-size growth through
  `VfsInode::truncate_setsize()`; dispatch already holds the data and invalidate
  locks, so the self-locking publish helpers deadlock in that context.
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

`SimpleFileOps` 默认拒绝写入；需要可写文件时，由具体操作对象实现写入语义。
`CommandFile` 的 read callback 必须返回 `Some(content)`；`Ok(None)` 转为
`VfsError::InvalidData`，callback 自身错误原样传播，空内容使用 `Some(Vec::new())`。
`CommandFile` 表达 procfs/cgroupfs 一类 command-style pseudo file。一次 write/writev
请求先完整复制到最多 4096 字节的内核缓冲区，再作为一个命令交给 callback；文件 offset
和此前写入均不参与命令拼接。callback 同时收到保存 opener credential 的 `VfsFile`，因此
descriptor-based 授权不需要反向查询 current task。`SimpleFile` 根据具体操作对象的
`write_iter` 分派写入算法，不通过额外的模式字段选择。

`SimpleDirOps::mkdir()` 接收 VFS 已解析并加锁的 parent inode、准备后的 mode 和同一次
pathname 操作的 credential；`rmdir()` 接收 parent inode 与加锁的 victim dentry。动态
文件系统必须基于这些对象提交后端 mutation，不能按裸名称重新 lookup 或查询 ambient
credential。`child_names()` 可传播生命周期或后端错误，使旧目录 fd 的 readdir 不会被
静默转换为空目录。需要持久 identity 的 pseudo filesystem 可用 `SimpleFile::new_inode()`、
`SimpleDir::new_inode_with_owner()` 和 `SimpleDirLookup::{file,dir}_from_inode()` 重用 inode。

### SimpleDir 动态目录链接数

`SimpleDirOps::link_count()` 为整个目录提供可选的动态 nlink。`getattr()` 先取得
`SimpleFsNode` 的 metadata 副本，释放其 metadata lock 后才调用该回调；`Some(nlink)`
仅覆盖本次返回的链接数，`None` 保留副本中的原值，回调错误向上传播。
`ChainedDirOps` 不合并各组成目录的链接数，沿用 `None` 默认值。procfs 的 task 目录
通过该接口在每次查询时读取目标进程的线程成员；已经打开的目录 fd 也得到新查询值。
其他 metadata 字段与目录 lookup/iteration 路径沿用现有行为。
动态值只属于 getattr 返回副本，不回写 `SimpleFsNode` 或 `VfsInode` cache。
需要当前 nlink 的调用方应走 getattr；`metadata()` 保留缓存语义。目录枚举仅从
缓存取 inode/type，不使用 nlink。新增 guest 定义覆盖 Some 动态变化、默认 None、
错误传播、其他字段和缓存保持；这些定义尚未编译或执行。

### Buffered read 与同步预读

缓存读复用 `FolioBatch` 批量收集连续 resident 引用，每批最多 31 页；
缓存引用批次与每次打开文件的预读窗口独立，缺页时使用同步预读协议，
再重新收集。批次引用只保活分配；逐页复制时拒绝失效或非 uptodate 页，
回退到 resident 查询和按需初始化。后续批次只收集当前读取请求的剩余范围。

`VfsFile` 持有 `Mutex<FileRaState>`，独立 open / alloc_clone 初始化独立历史，
共享同一个打开文件描述的 dup/fork 引用共享历史。状态只保存页窗口起点、页数、
提前触发区长度、可缩减的页预算和已完成读取的字节位置，不保存 EOF 或 folio 引用。
打开文件时，从所属 superblock 的 canonical block device 取得 `Gendisk::bdi` 预读预算；
块设备特殊文件直接解析该设备，无设备文件使用 128 KiB 默认值。预算只在打开时快照，
既有 fd 不受后续 BLKRASET 影响。顺序读按请求规模建立窗口，消费 folio 预读标记后按上限增长。
小随机缺页只准备请求范围，不覆盖顺序窗口；必要时从缓存的有界连续历史重建窗口。
仅允许预读的打开实例可以消费共享标记；预算为零的实例保留标记供其他读者使用。
其他允许预读的打开实例也可以消费共享标记，并根据当前缓存恢复自己的窗口。按需读取最终
返回 EIO 时该打开实例的预算缩为四分之一，降为零后仅准备一个需求页。
仅标记命中时在 folio 锁内获取策略锁，把预算检查、标记消费与窗口计算作为一次决策；
普通命中不逐页获取策略锁。策略锁不覆盖 folio 等待、用户复制或文件系统 I/O。
Early triggering describes when the window advances. Current backends still read
synchronously, but the interface allows initialization ownership to be transferred
without introducing a background task.

窗口预算与批次容量独立；单页缺失段及分批后的单页尾段也在本轮初始化。
分发时只有一个预留页则直接调用 `read_folio`，避免为单页 I/O 分配预读中转缓冲。
The window is submitted as contiguous missing ranges of at most 31 pages per
batch. Private folios are allocated and locked outside the index lock, then a
contiguous prefix is rechecked and published under that lock. I/O runs after the
index lock is released. `ReadaheadControl` borrows the AddressSpace and reservation
iterator; `next_folio()` transfers a `FolioRead` owning the page reference and
responsibility for its held lock, without a separate request index.
Default readahead consumes existing reservations through per-page `read_folio`
calls and continues after submission failures. The completer publishes uptodate
and unlocks the folio.
首个候选被并发插入时，窗口游标只越过该位置，继续查询后面的缺页。
游标在尝试初始化时消费当前页；文件系统提供含空洞及 EOF 尾部零填充的完整页，再标记 uptodate。
已消费的失败页可保留供按需重试；回调未消费的后缀在退出时清理，避免空预读或提前返回
留下无用驻留页。清理保持未消费页的锁，取得索引锁复核身份并摘除后再解锁；
等待读取者醒来后重新查找 resident，不在已摘除的预留页上继续初始化。读取失败不暴露部分数据，已复制前缀优先返回。

Demand reads also reserve an uninitialized folio before reading under its lock.
Competing readers wait for the folio lock and repeat the resident lookup;
successful pages need no duplicate I/O, while failed pages can be initialized
again. A successful `read_folio` return indicates submission only.
`FolioRead::complete` completes that read, publishing uptodate only on success;
failure or abandonment leaves the page unreadable.
Demand waits reject invalid sleeping context and may return `Interrupted` for
an uncatchable kill without taking ownership back from submitted I/O. A read
token describes one initialization attempt; a detached replacement may require
another call for the same index. Buffered writes validate context before
`write_begin`, and size changes do so before publishing EOF, keeping mandatory
rollback/truncation waits inside an admitted operation.
文件系统负责本次读取边界与尾部清零，通用层不以完成后的 EOF 重验返回字节数。
缓冲缺页路径从 EOF 检查到初始化完成持 invalidate shared lock，缓存索引锁不覆盖 I/O。通用 buffered 复制期间不持 invalidate lock，锁定每页后重新读取
inode EOF。单次读取的已准备末端只用于避免对失败预留重复发起预读，不保证缓存驻留。
只有新发布的预留页携带窗口标记；在 folio 锁内完成标记消费与窗口决策，释放 folio 和策略锁后执行后续窗口。

### Advisory 文件锁

`locks.rs` 在 `VfsInode` 上按需建立 `FileLockContext`，对应 Linux `i_flctx`。
一个 mutex 保护 POSIX/OFD 区间集合和独立的 flock 集合；公开的 `FileLock`
仅表达不可变请求和查询结果，内部 `LockNode` 保存共享节点。`LockList` 与不可克隆的
`OwnedLock` 交接安装区间的唯一所有权，只有后者的独占借用能改区间；`Waiter`
区间终生不可变，`Blocker` 仅提供等待关系身份。通知链表由 `WakeQueue` 私有持有；链表统一使用
`intrusive-collections::LinkedList` 与同一个节点内嵌链域。
这些薄类型复用原有 Arc/链表，不增加节点字段、锁或堆分配，并阻止共享句柄绕过所有权
直接入链。相同 owner 的区间合并、切分、替换；冲突请求
沿具体 blocker 按冲突形成等待树，授锁时转移后继，取消时直接解链并唤醒下一层。
已安装区间由 inode 锁保护，首个同类型合并对象原位扩展并保留身份。
通知复用请求链表节点和已注册 waker，关闭清理不分配；通知出队前请求不得
重用节点。全局 owner 索引按
实际 blocker 的 POSIX 类型登记（包括 OFD 请求），仅 POSIX 请求执行沿树根的
有界 owner 死锁检测，不复制 inode 锁集合。传统 owner 是 fd 表身份，OFD/flock owner
是打开文件身份；锁记录不强持有这两类 owner。`FileOperations` 分派查询和
加锁；描述符关闭即使 flush 失败也清理 POSIX 锁，最终文件释放清理 OFD/flock。

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

- The effective process capability set now covers DAC, sticky directories,
  xattr, chown/chmod, and related checks; LSM, ACL, user-namespace ID mapping,
  idmapped mounts, and file capabilities are not supported. Ambient
  capabilities are not a separate filesystem input; they affect a check only
  after becoming effective. `trusted.*` access and `security.*` mutation require
  effective `CAP_SYS_ADMIN`; user xattr modification in a sticky directory also
  requires directory ownership or effective `CAP_FOWNER`.
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

## Sequential record rendering errors

`SeqIterator::try_show` defaults to the existing `show` formatter and maps its
formatting failure to `Io`. Iterators that need a filesystem error may override
it, for example to preserve procfs `NoSuchProcess` when a weak target expires.
`SeqFile` discards the failed record and resets its iterator before returning the
error. Successful records use the existing per-open buffer and offset semantics;
no second snapshot or credential cache is introduced.

## Per-open generated-file snapshots

`SimpleFile` owns a file-operation object selected at construction. Ordinary
files use `GeneratedFileOperations`; `new_snapshot` selects the private
`SnapshotFileOperations`. Snapshot initialization, reads, write capability and
seeking belong to that object, without a mode flag in the generic simple file.
The generator remains shared with the inode's symlink operations.

Read-capable opens generate one immutable, private `SimpleFileSnapshot` in
`VfsFile::private_data`. Write-only opens neither generate nor retain bytes.
Separate opens capture independent generations. Normal descriptor sharing
retains the same file description and buffer; low-level clones that bypass
open are not supported initializers; reads and `SEEK_END` with missing state
return `InvalidData`.
The snapshot representation and constructor are not public API.

`SEEK_SET` and `SEEK_CUR` use only the descriptor position and work even for
write-only opens without generating content. `SEEK_END` uses that
open's immutable buffer length, never shared inode metadata. Negative results,
invalid whence and signed arithmetic overflow return `InvalidInput` without
changing the position. Seeking beyond EOF is allowed and reading there returns
zero. Tests cover differently sized generations, EOF, backward seeks and
invalid offsets. Snapshot operations do not advertise `CAN_WRITE`: write-mode
writes fail with `InvalidInput`; read-only writes fail with `BadFileDescriptor`.

## Pinned file pages for private mappings

`AddressSpace::with_pinned_file_page()` initializes a stable page object while
holding the invalidation read lock. The callback validates EOF and consumes the
pin through `map_readonly`, storing its must-use `MappedFolio` token alongside
the PTE. Before materialization, the provider rejects page offsets at/beyond EOF
and overflowing indices under the same lock, without allocation or quota charge.
The callback still validates its exact byte range; its failure retains a valid
initialized page and its charge for ordinary cache reuse. Truncate submits
invalidation before detaching cache entries, but a busy
MM may queue that submission. Only completed PTE removal and TLB invalidation
permit releasing mapping tokens; cache removal alone does not.

`PinnedFolio` and `MappedFolio` retain the same `Arc<Folio>` that owns its page
and content lock. Ordinary access uses a restricted `FolioGuard`; initialization
uses an owning `FolioRead` that can be transferred to completion;
cache replacement changes an index reference, not a published object's identity.
The original page remains copyable under its original lock after detachment,
without retaining its replacement. Copies do not by themselves prove current EOF
or freeze bytes against truncate. Pins and tokens share the stable page and its
physical-allocation quota charge without an additional statistics subsystem.

## tmpfs quota integration with resident initialization

The inode address space retains the invalidate lock covering EOF, cache
reservation and read submission. Transferred folio locks serialize later completion with truncate. The filesystem owns capacity policy and supplies one `CacheResidency` provider
when constructing an inode mapping. Only a winning new identity calls
`try_add_folio()` under folio and index locks. Its single capacity charge stays
with the stable folio after cache detachment, while any pin, mapping token or
batch retains the allocation. Clones do not charge again. Final release frees
the physical page before returning capacity, outside both locks; the charge
keeps its provider alive independently of mapping destruction.
Providers must not reenter mappings, acquire folio locks, or perform I/O.
Release and final provider destruction must not sleep: the last reference can
be released by I/O completion. Admission still requires task context.
For memfs, the provider is its filesystem-owned quota, so lock order is cache
folio -> index -> quota limit; remount and statfs never acquire cache locks.
Speculative physical storage remains outside the admission policy until insertion.

The cache publishes non-uptodate reservations before filesystem I/O and keeps
its index lock out of I/O, preserving the current main concurrency contract.
Unconsumed readahead reservations detach while locked and return capacity only
after their final allocation reference is dropped outside the locks.


## Unrecoverable filesystem write failure

`SuperBlock::mark_read_only()` 在不可恢复的文件系统错误后原子设置现有 `RDONLY` 位，
保留其他标志。它不执行 flush 或 transaction 处理；文件系统负责停止内部写入，并在
释放 mutation 保护前发布只读状态，使现有 VFS 写入口和 statfs 导出立即观察该状态。
文件系统的 reconfigure 回调必须依据已有的失败状态拒绝恢复可写，且在返回错误前不应用
其他挂载选项。`RDONLY` 本身仍是挂载标志，不能替代文件系统内部的写入准入校验。

### Submission-based writeback batches

`AddressSpace::submit_cached_ranges` transfers bounded PageCache snapshots and
completion responsibility to the filesystem, sharing enumeration with synchronous
`writeback_cached_ranges`. Its budget counts pages handed to the callback; the
synchronous API uses the same attempted-page rule. Callback return does not mean completion.
The filesystem bounds in-flight work, drains I/O, completes folios in task context,
and protects inode/extent lifetime. Invalidate shared protection covers the call
and is not transferred to deferred completion. Filesystems therefore need their
own drain protocol. Raw `writepages` is submission in either mode: an
implementation may complete inline or return with owned I/O pending. Neither mode
expresses fsync/fdatasync intent. `WritebackControl` contains only the range,
submission mode, page budget and skipped count.

Cross-task completion uses `retain_writeback_completion` to retain the complete
`AddressSpace` and validate that the batch belongs to its embedded `i_pages`.
Synchronous completion may borrow the index; neither path creates an independent
index owner.

`AddressSpace` embeds its mapping `WritebackErrors`; `SuperBlock` embeds a separate
filesystem channel. Completion can borrow the mapping channel through its retained
VFS inode without allocating another Arc owner. Filesystems record real preparation,
submission, and completion errors in both channels before ending writeback or
dropping completion ownership. Capacity throttling is not an I/O failure.

Each channel keeps a generation, latest error, and observation flag. `record`
advances the generation and marks it unseen. `sample` skips only seen errors, so a
newly opened file still observes an unconsumed failure. `current_generation` reads
the current boundary without inheriting history or marking it seen. `check`
reports generation changes without consuming; `check_and_advance` marks the error
seen and advances only that observer.
Existing older cursors still see the failure. All operations require sleepable
task context, with no spinlock or IRQ/preemption-disable guard and no dependency
on a current process. A complete 2^64 generation wrap can alias an old cursor.

Each `VfsFile` samples separate mapping and filesystem cursors at open; fsync and
syncfs respectively check and advance them. Shared file descriptions share cursors;
independent opens do not consume each other's pending errors. Mapping observation
does not mark the filesystem channel seen. Successful retry never clears errors;
multiple unseen failures coalesce to the latest. Temporary synchronous writeback
passes snapshot `current_generation` before waiting for submission or existing
I/O, then use non-consuming checks for failures recorded during the pass. They
do not repeatedly inject old mapping history into syncfs after its filesystem
cursor has advanced. Historical errors remain pending for the appropriate file
cursor, including later opens while the mapping error is unseen. A direct error
takes precedence over recorded history, while required drains and cursor checks
still run. Superblock sync tries
all live inodes, all inode metadata, and the filesystem sync hook after an earlier
failure, retaining the first error. Filesystem abort checks remain authoritative,
and shutdown cannot skip remaining drain/flush stages because one stage failed.

## Buffered-write timestamp publication

Generic buffered writes and shared writable faults call `file_update_time()` outside folio
protection. `Kiocb::generic_file_write_iter_with_prepare` instead lets a filesystem
combine its admission checks and automatic timestamp publication in one callback.
It runs once under the inode data lock and before folio locking or data copying,
including for an empty request. It applies filesystem policy and generic size
limits in the required order, may narrow the accepted count, and must finish
fallible checks before publishing timestamps for a nonempty write; KVFS does not
repeat `file_update_time` for this path. This is a filesystem-owned preparation
contract, not an option for skipping timestamp semantics.
Filesystems may implement automatic `InodeOperations::update_time` with
resident attribute publication and deferred persistence; explicit setattr remains
a separate contract. The automatic callback owns comparison and publication and must
finish fallible checks before changing resident values. KVFS does not republish them.
The filesystem must retain metadata dirtiness through data-only
writeback and cover full fsync, syncfs/unmount and inode eviction.
## Generic quota ownership

Each `SuperBlock` owns one sleepable `Mutex<kquota::QuotaInfo<Arc<VfsInode>>>`. The state is
initially disabled for user, group and project quotas. `quota_info()` exposes
the canonical owner to filesystem integration; no filesystem-private duplicate
quota manager is needed. The core owns record caching, admission and quota
formats, while the concrete filesystem supplies direct backing-file I/O and
transactions. Each loaded class retains the canonical VFS inode in that
state, so filesystem integration needs no second class-to-inode array.
Suspension and failed disable retain that reference; successful disable returns
it for release after writeback and after leaving filesystem locks. Ext4 enables internal user/group/project usage accounting on mount.

Quota operations may sleep. Their backing-file I/O must bypass charging and
must not reenter the same quota owner or reacquire a caller-held filesystem
lock. Filesystem integration must establish one lock order and explicitly
flush/disable quota before final owner destruction.
The inode snapshot and eviction wait let a filesystem detach inode-held records
before releasing its quota cache. Snapshots are dropped without locks needed by
eviction, and the wait itself holds no filesystem mutation lock.

## Bounded private-fault readahead

`AddressSpace::readahead_for_fault` prefetches at most 16 missing file-cache
pages starting at a private mapping's fault offset. It is bounded by the inode's
visible length and an optional exclusive `file_end` supplied by the ELF loader.
A cached target does no work; only missing runs are submitted. The invalidate
lock is held for reading across EOF selection and submission, so concurrent
truncate cannot change the bound before stale pages are inserted. The helper
fills cache entries, not PTEs, and is optional: a returned error lets the caller
perform the original demand read.

## Final filesystem resource release

`SuperBlockOperations::put_super` runs after final ordinary inode writeback and
dcache eviction, under umount exclusion and without the lifecycle mutex. It lets
a filesystem disable quota and release other superblock-owned resources before
block-device ownership is dropped. Inode references must be dropped outside
filesystem mutation locks. `for_each_inode` visits a retained live-inode snapshot
without holding the registry lock, for serialized lifecycle initialization.
`FsContext::sync_filesystem` supplies writeback to reconfigure callbacks already
protected by umount exclusion, without recursively taking that lock.

## Read-only remount admission

The existing superblock umount lock is a read/write lock. Ordinary namespace,
attribute and xattr mutations hold its shared side through the backend call.
Writable open holds it until the existing inode write count is acquired;
that count remains held for the file's lifetime. Reconfigure and final shutdown
hold the exclusive side. Read-only remount pins the live inode snapshot,
waits for already-running cache evictions on their existing transition queues,
and rejects remaining writers or zero-link inodes with `EBUSY` before calling
the filesystem. Snapshot pins remain until the new flags are published.
This reuses inode counts and cache states without another removal counter.

The VFS cache binds and registers each successfully initialized inode before
cache publication. Filesystem inode constructors prepare identity and readable state;
write-dependent initialization belongs to writable-open and mutation callbacks
that participate in remount admission.

Special-file timestamp changes acquire shared write admission for the metadata
update because those files do not retain an inode writer count. After a
read-only policy is published, FIFO data I/O may continue while its timestamp
update is skipped. Ordinary writable files retain their existing admission
through the open-file writer count and do not recursively acquire this lock.

## Quota control and mutable inode policy

`SuperBlockOperations::quota_operations` selects filesystem control callbacks;
its default reports no support. `SuperBlock::quota_control` holds the existing
umount lock, validates lifecycle and class support, and invokes one command.
On/off use exclusive admission; other commands use shared admission.
`sync_for_quota` lets admitted callbacks drain writeback without reacquiring
shutdown exclusion. Quota state and retained files remain in `quota_info`.
`statfs` passes the queried dentry to the filesystem, allowing its result to
reflect a project-inherit directory's quota. File-attribute operations use the
inode's filesystem callbacks, with ownership and capability checks at the VFS
boundary and the inode mutation locks held across validation and publication.

`InodeAttributeOperations::flags` supplies filesystem-owned behavior flags from
resident state. The existing owned-attribute implementation stores construction
flags, while filesystem attribute implementations store their own flags. The
common inode wrapper retains no second behavior-flag field. This permits
quota-file protection to change without another attribute copy. Immutable policy is checked for write permission, cached
content writes, metadata changes, truncation, fallocate and namespace mutation;
NO_ATIME suppresses automatic access-time updates. File-data exclusion precedes
backend mutation locks when publishing protection.

`SuperBlockOperations::show_options` supplies filesystem-specific options to
proc mount reports. Mounted block-backed metadata uses the existing canonical
block-device identity, allowing tools to match st_dev to the source device.

Live inode cache invalidation is separate from final eviction. Under the
address-space invalidate write lock it synchronously revokes every registered
view's PTEs and completes TLB invalidation, then flushes dirty folios and removes
cache entries. A busy MM causes a retry after dropping that lock; no cached page
is detached while revocation remains deferred. Read faults publish shared PTEs
under the corresponding invalidate read lock.

`AddressSpace::invalidate_range()` is the byte-range counterpart for
backing-store transitions such as direct I/O. It holds the same invalidate write
lock, rounds the request outward to page boundaries so partial leading and
trailing pages are unmapped whole, and treats every covering view's synchronous
revocation as a precondition. A busy MM returns `Ok(false)` with no folio
discarded; a covering view without a notifier is rejected with
`OperationNotSupported`. Only after every view reports completion does it
discard clean folios in the range. A folio that stayed dirty is reported as
`ResourceBusy` after the clean ones were already discarded, matching Linux
partial invalidation.

Buffered `write()` and the mmap write fault share one exclusion boundary: the
folio instantiation and dirty window of the write path holds the invalidate read
lock, matching `page_mkwrite`, so a truncate or range invalidation holding the
write lock cannot discard a page between its residency check and its dirty
publication. Registered views cover page-aligned file ranges only: a partial tail
page is COW or zero-filled and owns no cache reference, so the view ends on the
page boundary instead of reaching into that anonymous tail. Executable images
register their private file views at load with a live invalidation handle, so the
same invalidation reaches text segments the loader maps directly from the cache.

Regular-file writes and writable open initialization hold shared superblock
lifecycle admission for the complete operation. Quota enable/disable hold its
exclusive side through writer initialization or reservation drain and teardown.
Shared write faults recheck inode mutability under a data read guard. MM callers
use nonblocking acquisition and retry a busy write fault outside the MM critical
section, preventing lock inversion with revocation.

Shared write-fault admission waits use the same superblock lifecycle and inode
data locks as the nonblocking fault checks. `VfsFile::wait_for_shared_mapping_write`
acquires them in that order outside MM serialization and releases them before
fresh fault lookup. It grants no write authority and does not cache policy.
The canonical lock wait queues allow the holder to run without polling budgets.


### Buffered reads into faultable destinations

Page-cache reads copy through `IovIterDest::copy_to_iter_nofault` while holding
the source folio. A short user copy advances the iterator and file offset only
by its successful prefix. After the folio and policy guards are released, the
read prepares the next destination prefix through `fault_in_writeable`. A retry
performs a fresh cache lookup and samples EOF under the folio lock again, so
invalidation or truncate during the wait cannot expose stale bytes. Permanent
faults return the completed prefix, or the error when no bytes were copied.
Kernel-memory short copies still finish the read without a fault retry.

An address-space view guard can extend its existing registration under MM
serialization. The registry mutex protects the VMA/object spans, preserving the
registration identity and invalidation notifier. Coordinates are validated before
publication; invalidation snapshots do not hold this mutex while entering MM.

## Bounded data write-and-wait

`AddressSpace::write_and_wait_range` samples the current mapping-error generation,
then captures the highest intersecting resident index under the existing page
index mutex. Clean pages carrying older WRITEBACK remain part of that target.
The helper clips the exclusive byte endpoint with overflow-safe arithmetic and
uses exactly that finite range for ALL submission and subsequent WRITEBACK wait.
A later append above the target cannot extend either stage. An empty resident
target bypasses raw submission; a file fsync still invokes the filesystem hook.
The bound is invocation-local and grants neither EOF authority nor a content
snapshot. There is no added persistent field, coordination owner or lock.

Submission errors do not skip waiting or the final nonconsuming error-generation
check; the first error remains authoritative. The invalidate read guard covers
submission only and is released before the generic wait. Filesystems still drain
their own detached/queued obligations before releasing backing mappings.

Live cache invalidation already owns invalidate exclusion for writing. It uses
the same data write-and-wait algorithm with direct submission under that existing
guard, before discarding cache pages. Target capture, completion waits and error
precedence are shared; failed data remains cached for retry.

`VfsFile::writeback_mapping`/`writeback_mapping_range`, inode/dentry sync, FIEMAP_SYNC
and superblock data sync use this data-completion boundary. They do not themselves
commit a journal or flush a device. Filesystem durability belongs to the file
fsync or superblock sync hook. Scan accounting and TOWRITE selection are owned by
pagecache; repeated dirtying through writable PTEs remains dependent on G.

### Buffered acceptance and live writeback EOF

Copying, dirty publication, write_end acceptance and visible i_size publication
finish under the same folio lock and invalidate read guard. A successful-copy
write_end hook may update reservations and inode metadata, but cannot reacquire
the folio, invalidate the cache or wait for writeback. Copy-failure cancellation
and short-accept/error cache rollback run after releasing these guards. The
writer retains its inode data lock; completion never needs that lock.

All cache writeback adapters pass one control object and a live inode-size
supplier. Pagecache selects a finite target independently of EOF and samples
valid bytes after locking each candidate. No pending-size field, copied EOF,
pass owner or extra lifecycle lock is introduced. These buffered guarantees do
not capture stores through an already writable PTE.
