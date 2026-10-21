# memfs — 设计文档

## 测试布局

guest 单元测试位于 crate 根目录 `ut-tests/`，通过 `unittest::path_mod_test!`
挂载在原父模块下，保留私有 `tests` 模块名、父模块访问及 `cfg(unittest)`。
测试文件为 `lib.rs`、`shmem.rs`、`quota.rs` 和 `inode_quota.rs`。
没有额外架构或 feature 门控，也没有 serial/user 标记或 host 测试。
四种内核目标在链接 memfs 后均选择这些用例；`fs/boot` 无条件依赖 memfs，
不需要新增文件系统 feature 开关。验证使用配置好的内核 `UNITTEST=y` 流程。

## 定位

`memfs` 提供纯内存文件系统的 inode 和目录树实现。普通文件内容通过
`VfsInode::i_mapping -> kvfs::AddressSpace` 提供
cached content 与 MM shared object identity。

Linux 对应关系：

- tmpfs/shmem inode 持有 `address_space`
- `mm/shmem.c` 通过 inode-backed page cache 管理文件内容
- `shmem_file_setup()` 创建不挂入全局路径空间的匿名 tmpfs 文件对象

## 背景

`memfs` 只拥有 inode、目录树、符号链接和元数据。普通文件的页缓存、evict
listener 和 MM object identity 归 VFS inode address-space 管理。
因此同一 inode 的 open-file、hard-link、mmap 和 truncate 路径共享同一个内容
owner，而不是由每个 dentry 或 open file 单独拥有内容对象。

## 范围

- `src/lib.rs`
- `src/shmem.rs`
- `src/quota.rs`、`src/ramfs.rs`
- `ut-tests/lib.rs`、`ut-tests/shmem.rs`、`ut-tests/quota.rs`
- regular-file inode 与 VFS address-space/page-cache 的连接
- 目录、链接、符号链接和元数据维护
- 基于 VFS `Cred` callback 的 inode owner 初始化
- fd-only tmpfs/shmem anonymous regular-file factory and opened-file conversion
- mounted tmpfs page quota, `size`/`nr_blocks` options, and `statfs` reporting
- Static KVFS `FileSystemType` descriptor for `tmpfs`.

## 架构

```text
MemoryFs
  -> inode_numbers: Slab<()>
  -> Option<Arc<TmpfsAccounting>> (tmpfs; shared by all inodes)

InodeRef / MemoryNode -> Arc<Inode> -> Arc<MemoryFs>

VFS-wide identity table: (SuperBlock, ino)
  -> VfsInode
       -> i_mapping: kvfs::AddressSpace
            -> i_pages: pagecache::MappingPages (directly owned)

VfsFile::mapping()
  -> VfsInode::i_mapping / kvfs::AddressSpace
```

`memfs` 自身不定义第二套 file-backed content owner。KFS 高层通过
`VfsFile::mapping()` 进入文件缓存路径后，只取得 inode address-space；private
page-cache storage、mapped views 与 MM shared object identity 的统一宿主是
`VfsInode::i_mapping` 下的 `AddressSpace`。

`MemoryFs` 也不保存 inode-number 到 `VfsInode` 的第二张表。构造时先分配
`SuperBlock`，再由 root initializer 通过 `SuperBlock::get_or_init_inode()` 进入 VFS-wide table
建立 root；普通 lookup/create/link 由当前目录 inode 的 superblock 进入同一个 table。`MemoryFs`
自己的 slab 只分配 inode 编号；目录项和 VFS 操作对象直接持有 backing inode。

`TMPFS_TYPE` describes only the canonical name and nodev superblock factory.
Boot and KVFS own filesystem-type registration and per-mount policy, so that
policy is not stored in `MemoryFs` inode state. Each tmpfs mount creates an
independent instance. Device discovery uses the separate `sysfs` crate rather
than a memfs-backed pseudo sysfs.

## 调用约束 / 执行上下文

- regular-file read/write/truncate 允许睡眠，因为可能分配 folio。
- 目录和元数据操作使用 `memfs` 自身锁保护。
- 不适用于中断上下文。

## 算法流程

### 创建普通文件

1. VFS 完成路径与父目录 DAC 后，把同一 `&Cred` 传给 create/mkdir/mknod/symlink callback。
2. `memfs` 调用 `inode_init_owner()`，用 `fsuid/fsgid` 初始化 owner，并处理 setgid
   父目录的组继承与子目录 setgid 传播。
3. `memfs` 创建对应 inode；`VfsInode` 构造时持有稳定的 `kvfs::AddressSpace`。
4. 首次通过 KFS page-cache file path 进入文件缓存路径。
5. `AddressSpace` 建立唯一的私有 `MappingPages` 实现组件。
6. open-file、mmap、truncate 和 evict 路径复用同一个 address-space mapping。

### 创建匿名文件

1. `memfs::shmem` factory 创建私有 `MemoryFs`，名称为 `tmpfs`。
2. factory 在该私有 filesystem 的 root mount 下创建 regular file。
3. file inode 仍通过同一个 `VfsInode::i_mapping` 获取 page cache 与 MM object
   identity。
4. 返回的 `Location` 由调用方通过 KFS `OpenOptions` 打开成 fd，不挂入进程可见
   路径空间。

### tmpfs mount entry

The public VFS mount path enters `tmpfs_get_tree` in `src/lib.rs`. That entry
parses the tmpfs context options, resolves the page limit and root metadata,
and passes the selected `FileSystemType`, VFS-wide `SuperBlockFlags`, and
mount options to `shmem::new_tmpfs_with_type_and_mount_options`, which
constructs the private `MemoryFs` superblock. After the factory returns, VFS
attaches the superblock to the caller's mount namespace and applies per-mount
policy there.

Without an explicit size, the mount limit is one half of `kalloc`'s allocatable
physical pages, matching Linux's default tmpfs policy. `size=<bytes>` accepts
`k`/`m`/`g`/`t`/`p`/`e` suffixes and `%` of total allocatable memory. Explicit byte and percentage sizes round up to a page, so `size=50%` of
257 allocatable pages allows 129 pages; the unspecified default still uses
integer half (128 pages), with zero selecting unlimited. `nr_blocks` uses
page units and accepts the same size suffixes. `size=0` and `nr_blocks=0`
explicitly select unlimited. `mode`, `uid`, and `gid` select the root inode's
metadata at initial mount; remount leaves those fields unchanged, as Linux does.
`size`, `nr_blocks`, and `nr_inodes` share decimal, octal and hexadecimal
number parsing with binary size suffixes. `nr_inodes` limits inode space;
zero selects unlimited, and finite limits are bounded by
`usize::MAX / 1024`. The default is `min(total_pages / 2, usize::MAX / 1024)`.
The root and each additional hard link consume one unit; directory `.` and `..`
links do not. Removing the last name preserves the inode charge until its final
reference is released. `statfs` reports total and free inode units. `noswap` is accepted as a no-op because this tmpfs implementation
has no swap backend. The page limit is a quota on actually resident folios, not a
reservation and not a file logical-size sum.

For a finite limit, `statfs()` reports the current maximum and available quota
pages. For an unlimited limit, it reports zero block fields like Linux
`shmem_statfs()`; zero means "no fixed capacity", not "the quota is exhausted".
Reconfigure validates explicitly requested page and inode limits together
before changing either one, without evicting existing data. A finite limit may be lowered only to the current
usage, and an unlimited instance cannot be retroactively limited; it may still
become unlimited again. A requested finite limit below current usage returns
`EINVAL` and preserves the old limit, matching Linux `shmem_reconfigure()`;
`ENOSPC` is reserved for materialization beyond an installed limit.
Anonymous shmem/memfd instances remain unlimited in both dimensions.
Both limits use zero for unlimited throughout parsing and accounting;
`Option` in a remount request only distinguishes an omitted limit from an explicit value.

### 读取普通文件

1. KFS file path 找到 inode address-space mapping。
2. memfs `MemoryNode::read_iter` owns sparse-hole handling and never invokes
   generic readahead. It uses `AddressSpace::visit_resident_folios` to access the
   common cache with invalidation excluded and EOF sampled under that guard.
3. Existing uptodate pages supply bytes; retained failed reservations retry
   initialization and preserve errors. Only an absent identity supplies zeroes
   from memfs's static zero page, without allocation or admission.
4. File-mapping faults materialize cache identities normally. Capacity failures
   become BusError/SIGBUS; buffered write capacity failures remain ENOSPC.

### 写入普通文件

1. KFS file path 找到 inode address-space mapping。
2. `kvfs::AddressSpace` materializes a missing folio through the tmpfs page
   quota and writes into it.
3. Candidate physical storage is prepared outside the cache index lock. The
   winning resident identity is charged before publication; a rejected charge
   returns `StorageFull` and the unpublished candidate is released. Candidate
   allocation can fail before quota admission, so the quota is not a physical
   memory reservation.
4. inode-owned `AddressSpace` 继续提供共享 object identity 与 mmap contract。

### 文件同步

`MemoryNode` 为内存文件和目录显式提供 `fsync/fdatasync` 成功的空操作，
包括 tmpfs、ramfs 和基于 shmem 的 memfd；这些对象没有持久化设备，
同步不能通过清除或驱逐 dirty folio 丢弃唯一的数据副本。
FIFO 和设备 inode 在 VFS open 时安装专用 operation table，不继承该空操作。

### memfd 与 exec

`create_memfd_file()` 用模式 `0o777` 创建匿名 memfd 文件（对齐 Linux
`shmem_file_setup` 配合 umask）。可执行位对 `fexecve(memfd)` /
`execveat(dirfd, "", AT_EMPTY_PATH)` 是必需的：runc/crun 把自身镜像拷入 memfd 并
重新 exec（CVE-2019-5736 自克隆加固），exec 的 `MAY_EXEC` 检查需要通过。memfd
exec 的显示路径为调用方提供的 `/proc/self/fd/<n>`，`/proc/self/exe` 跟随同一
已打开 backing file。

memfd seal 集合由 `ShmemSealSet` 原子管理，只允许添加。`create_memfd_file`
接收初始 seal：没有 `MFD_ALLOW_SEALING` 时立即置 `F_SEAL_SEAL`，此后不能再加
任何 seal（对齐 Linux）。

## 并发模型

- inode metadata 由 `Mutex<Metadata>` 保护。
- 目录项表由 `Mutex<HashMap<...>>` 保护。
- 同一 filesystem instance 的 VFS inode identity 由 VFS-wide table 按 `(SuperBlock, ino)` 串行
  初始化；`MemoryFs` 不增加 cache lock 或 cache 字段。
- inode address-space 的 private page-cache storage、mapped views 与 MM object identity
  由 `kvfs::AddressSpace` 内部同步保护；
- `MemoryNode::cache_residency()` gives each newly constructed mapping one
  reference to the filesystem quota; lookup never traverses weak inode or
  superblock references on a resident access.
- The generic cache serializes admission with winning identity publication and
  removal. Its callbacks take the quota limit lock after the cache index mutex;
  remount/statfs hold only the quota lock, never a mapping lock.
- `TmpfsAccounting` protects both limits and remaining inode space with
  `RwLock<TmpfsStats>`. Inode admission/release and remount take the write lock;
  statfs samples both capacities under the read lock. No per-inode charge object
  or additional usage counter is stored.
- Namespace link removal takes metadata → stats locks. Final inode destruction
  releases its number under the slab lock, then returns its allocation charge.
  Admission releases stats before acquiring slab; stats never acquires inode locks.

## 设计决策

1. `memfs` 不自行实现 file-backed content owner，而是复用 VFS inode
   address-space mapping。
   原因：同一 inode 的 file I/O、mmap、truncate 和 hard-link alias 必须共享同一
   内容对象。

2. 符号链接仍由 `FileContent.symlink` 保存。
   原因：当前 symlink 数据路径独立于 regular-file page cache。包括长 target 在内的
   String 分配不计入 resident-folio quota；该额度不是文件系统全部内存的总预算。

3. 匿名文件用私有 tmpfs mount 中的 regular file 表达。
   原因：这保持了 Linux `shmem_file_setup()` 的私有文件对象 + inode-owned page
   cache 语义，同时不把对象挂入进程可见路径空间。调用方通过 shmem 对象转换成
   opened `VfsFile`，不重新操作匿名路径的 open 细节。

4. `memfs` 不保存“当前凭据”。
   原因：当前 task 属于上层 `kprocess`；创建者身份由每次 VFS callback 的 `&Cred`
   显式提供，inode 只持久化派生出的 UID/GID。

5. filesystem type 描述符不保存 mount flags。
   原因：`nosuid`、`nodev`、`noexec` 和 atime policy 属于每个 KVFS `Mount`，
   The nodev factory only selects or creates the filesystem and superblock;
   every tmpfs mount creates a new instance.

6. tmpfs size is enforced at folio materialization rather than file length.
   原因：sparse holes do not consume pages, while a small file can dirty one
   full page. Charging actual folios also makes truncate/unlink release quota
   without a second accounting pass. A buffered-read hole is served from the
   zero page, so it cannot consume the last quota page or turn `read(2)` into
   `ENOSPC`. Mmap read faults deliberately materialize and charge a folio, as
   Linux does for `shmem_fault()`; only the buffered-read path follows
   `SGP_READ` and zero-fills holes.

## Drop / 资源释放

- inode 释放时，`AddressSpace` 在最后一个引用释放时销毁，其直接持有的 `MappingPages` 随字段释放。
- resident cache-entry removal releases its page-quota charge; `truncate(0)` and final
  inode eviction therefore make capacity available again.
- 目录删除逻辑由 `InodeRef` 和 nlink 维护。
- `InodeRef` 持有 backing inode；`Inode::drop` 在最后一个引用释放时归还编号与
  分配额度，临时引用与 VFS 操作对象遵循同一生命周期。

`quota.rs` owns the mount limit, usage and remount/statfs policy. Each mapping
stores one lifecycle provider reference; entries are plain shared folio handles.
Admission runs only for winning identities. Removal and final mapping destruction
return capacity immediately, even if read batches still retain invalidated folios.

Inode-space exhaustion rejects creation or an additional hard link with
`ENOSPC` before publication; publication failures undo the directory entry and
return its charge through the existing release path. A file-backed fault that exhausts resident capacity becomes
`BusError` and delivers `SIGBUS`; ordinary buffered writes still return `ENOSPC`.

### Shared sparse reader and native copy

`read_memory_file` is the common implementation for ordinary regular-file reads
and `move_copy_chunk`. Both access the same inode mapping, zero-fill source
holes without materialization, and preserve resident initialization failures.
Native copies write that prefix through the ordinary destination write checks;
a full quota does not prevent overwriting already resident destination pages.
Neither source readahead nor source quota admission occurs on holes.

Reads progress in batches of at most FolioBatch::CAPACITY (31) pages. Each batch
holds one invalidate read guard and takes one index snapshot, instead of repeating
both locks per page. Folio locks still serialize data copying. An unaligned EOF,
a short sink, or a later initialization error returns the copied prefix and
advances Kiocb by exactly that prefix. Sparse policy remains in memfs.
